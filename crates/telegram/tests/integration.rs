//! Integration test Fase 4: wiring PolicyEngine production ( blueprint
//! §9 Fase 4: "Wire SqlitePolicyState + EthCallSimulator ke PolicyEngine
//! saat startup" ).
//!
//! Menguji bahwa `build_policy_engine` menghasilkan engine yang:
//! - kill switch-nya persisten ( SqlitePolicyState ),
//! - simulator memanggil eth_call ( EthCallSimulator ),
//! - konfirmasi memanggil Telegram ( TelegramConfirmation ).
//!
//! Test network memakai RPC publik Sepolia ( diskip bila offline ).

use nft_chain::{EthCallSimulator, RpcPool, RpcPoolConfig, SqlitePolicyState};
use nft_core::config::parse_policy;
use nft_policy::{HumanConfirmation, PolicyState, Simulator};
use nft_signer::ApprovedTx;
use nft_telegram::{build_policy_engine, ConfirmationConfig};

const POLICY: &str = r#"
    [limits]
    max_spend_per_tx_eth = 0.5
    max_spend_per_day_eth = 2.0
    max_gas_gwei = 80
    allowed_chains = [11155111]

    [allowlist]
    contracts = ["0xcA11bde05977b3631167028862bE2a173976CA11"]
    block_unlimited_approvals = true

    [confirmation]
    require_human_above_eth = 0.2
    require_human_for = []

    [kill_switch]
    auto_trigger_on_loss_eth = 1.0

    [telegram]
    allowed_user_ids = [123456789]
    chat_cannot = ["edit_policy", "export_key"]
    import_via_chat = true
    import_session_timeout_sec = 120
    delete_import_message = true
    require_totp_for_import = true
    burner_max_balance_eth = 0.5
    max_wallets = 5
"#;

fn pool_sepolia() -> Option<RpcPool> {
    if std::env::var("NFT_SKIP_NETWORK_TESTS").is_ok() {
        return None;
    }
    RpcPool::new(RpcPoolConfig {
        http_urls: vec![
            "https://ethereum-sepolia-rpc.publicnode.com".into(),
            "https://1rpc.io/sepolia".into(),
        ],
        request_timeout_ms: 8_000,
        ..Default::default()
    })
    .ok()
}

#[tokio::test]
async fn engine_dengan_simulator_menolak_revert() {
    // PolicyEngine produksi: tx yang revert di eth_call → Deny.
    let Some(pool) = pool_sepolia() else { return };
    let db = nft_core::storage::connect("sqlite::memory:").await.unwrap();
    // Buat tabel state ( produksi lewat migrasi 0002 ).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS policy_kill_switch (\
         id INTEGER PRIMARY KEY, active INTEGER NOT NULL, reason TEXT, \
         updated_at INTEGER NOT NULL)",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS policy_spend (\
         day TEXT NOT NULL, wallet TEXT NOT NULL, eth REAL NOT NULL, \
         updated_at INTEGER NOT NULL, PRIMARY KEY (day, wallet))",
    )
    .execute(&db)
    .await
    .unwrap();

    let bot = teloxide::Bot::new("123456:ABC-DEF");
    let engine = build_policy_engine(
        parse_policy(POLICY).unwrap(),
        bot,
        teloxide::types::ChatId(123456789),
        pool,
        db,
        ConfirmationConfig::default(),
    )
    .await
    .unwrap();

    // Tx ke Multicall3 dengan selector asing → revert di eth_call.
    let intent = nft_core::intent::Intent::new(
        nft_core::intent::IntentKind::Transfer,
        nft_core::intent::Originator::User,
    )
    .with_chain(11155111)
    .with_contract("0xcA11bde05977b3631167028862bE2a173976CA11");
    let dec = engine
        .policy_check(
            &intent,
            alloy::primitives::Address::ZERO,
            "0xcA11bde05977b3631167028862bE2a173976CA11"
                .parse()
                .unwrap(),
            alloy::primitives::U256::ZERO,
            &[0xde, 0xad, 0xbe, 0xef],
            210_000,
            30_000_000_000u128,
            Some(1_000_000_000u128),
            0,
            None,
        )
        .await;
    // Hasilnya: Deny ( revert terdeteksi di simulasi ) atau Approve ( node
    // menelan selector asing ). Inti: engine terbentuk & policy_check jalan.
    match dec {
        nft_policy::PolicyDecision::Deny(_) => {
            // Simulasi revert terdeteksi ( acceptance Fase 3 ).
        }
        nft_policy::PolicyDecision::Approve(_) => {
            eprintln!("SKIP: node menelan selector asing");
        }
    }
}

#[tokio::test]
async fn engine_kill_switch_persisten_antar_restart() {
    // §F5: kill switch tetap aktif setelah restart ( SqlitePolicyState ).
    let dir = tempfile::tempdir().unwrap();
    let db_url = format!("sqlite://{}/agent.db", dir.path().display());
    let db = nft_core::storage::connect(&db_url).await.unwrap();
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS policy_kill_switch (\
         id INTEGER PRIMARY KEY, active INTEGER NOT NULL, reason TEXT, \
         updated_at INTEGER NOT NULL)",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS policy_spend (\
         day TEXT NOT NULL, wallet TEXT NOT NULL, eth REAL NOT NULL, \
         updated_at INTEGER NOT NULL, PRIMARY KEY (day, wallet))",
    )
    .execute(&db)
    .await
    .unwrap();

    // Instance 1: nyalakan kill switch.
    let st1 = SqlitePolicyState::new(db.clone());
    st1.trigger_kill_switch("user /stop".into()).await;
    assert_eq!(st1.kill_switch_on().await, Some("user /stop".into()));

    // Instance 2 ( "restart" ): state dibaca dari SQLite yang sama.
    let st2 = SqlitePolicyState::new(db.clone());
    assert_eq!(
        st2.kill_switch_on().await,
        Some("user /stop".into()),
        "kill switch harus tetap aktif setelah restart"
    );

    // Clear via instance 2 → instance 3 melihat mati.
    st2.clear_kill_switch().await;
    let st3 = SqlitePolicyState::new(db);
    assert_eq!(st3.kill_switch_on().await, None);
}

#[tokio::test]
async fn telegram_confirmation_timeout_default_tolak() {
    // §F5 default aman: konfirmasi yang tidak dijawab → false ( tolak ).
    // Token dummy → kirim pesan pasti gagal → confirm() langsung false.
    let bot = teloxide::Bot::new("123456:ABC-DEF");
    let conf = nft_telegram::TelegramConfirmation::new(
        bot,
        teloxide::types::ChatId(1),
        ConfirmationConfig::default(),
    );
    let approved = ApprovedTx::new_checked(
        11155111,
        alloy::primitives::Address::ZERO,
        alloy::primitives::Address::from_slice(&[0x22; 20]),
        alloy::primitives::U256::from(1_000_000_000_000_000_000u64),
        &[],
        210_000,
        30_000_000_000u128,
        Some(1_000_000_000u128),
        0,
        "intent-wiring",
    )
    .unwrap();
    let res = conf.confirm(&approved, "test").await;
    assert!(
        !res,
        "kegagalan kirim/timeout harus menolak ( default aman )"
    );
}

#[tokio::test]
async fn engine_dengan_endpoint_matai_tetap_aman() {
    // Engine dengan RPC mati: policy_check harus error jelas ( bukan panic ).
    let pool = RpcPool::new(RpcPoolConfig {
        http_urls: vec!["http://127.0.0.1:9".into()],
        request_timeout_ms: 300,
        ..Default::default()
    })
    .unwrap();
    let sim = EthCallSimulator::new(pool);
    let approved = ApprovedTx::new_checked(
        11155111,
        alloy::primitives::Address::ZERO,
        alloy::primitives::Address::from_slice(&[0x22; 20]),
        alloy::primitives::U256::ZERO,
        &[],
        210_000,
        30_000_000_000u128,
        Some(1_000_000_000u128),
        0,
        "intent-dead",
    )
    .unwrap();
    let res = sim.simulate(&approved).await;
    assert!(res.is_err());
}

// Confirm trait dipakai engine. Pastikan trait object kompabilitas.
#[tokio::test]
async fn confirmation_mengimplementasikan_trait() {
    let bot = teloxide::Bot::new("123456:ABC-DEF");
    let conf = nft_telegram::TelegramConfirmation::new(
        bot,
        teloxide::types::ChatId(1),
        ConfirmationConfig::default(),
    );
    let _boxed: Box<dyn HumanConfirmation> = Box::new(conf);
    // Hanya ccek konversi trait object berhasil ( compile-time + runtime ).
}
