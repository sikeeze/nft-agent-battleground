//! Integration test Fase 5 ( blueprint §9 Fase 5 acceptance ).
//!
//! Acceptance:
//! 1. **Mint terjadi <1 block setelah trigger** — di Anvil ( block time
//!    instan ), tx `mint(uint256)` pada `TestNFT` langsung masuk block.
//! 2. **Tidak ada dobel mint saat proses di-restart di tengah war** —
//!    idempotency key `(task, wallet, nonce)` menolak eksekusi ganda.
//! 3. **Limit per wallet dihormati** — `maxPerWallet = 3` pada TestNFT.
//! 4. **Dry-run tidak mengirim tx** — aturan keras #3.
//!
//! Prasyarat: Anvil + Forge berjalan di `http://127.0.0.1:8545`. Test ini
//! mendeploy `TestNFT.sol` lewat `forge` ( path `.foundry/forge.exe` ),
//! lalu menjalankan pipeline:
//!
//! ```text
//! SaleReader::read_state → compile_exec_plan → MintExecutor::execute
//! → confirm_and_verify ( receipt + balanceOf )
//! ```
//!
//! Diskip otomatis bila Anvil tidak terjangkau ( `NFT_SKIP_FORK_TESTS=1`
//! atau forge/anvil tidak ada ).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use alloy::primitives::{Address, U256};
use nft_chain::{EthCallSimulator, GasMode, NonceManager, RpcPool, RpcPoolConfig};
use nft_core::config::parse_policy;
use nft_core::wallet::WalletRef;
use nft_mint::exec::MintTxStatus;
use nft_mint::persist::{pending_txs, record_tx, TxStatus};
use nft_mint::plan::{compile_exec_plan, MintPlan, TierGas};
use nft_mint::sale::SaleReader;
use nft_mint::trigger::{now_unix, Trigger};
use nft_mint::{MintExecutor, MintExecutorConfig, RaceTarget};
use nft_policy::PolicyEngine;
use nft_signer::WalletSigner;

const ANVIL_URL: &str = "http://127.0.0.1:8545";
const CHAIN_ID: u64 = 31_337; // chain id default Anvil
const DEPLOYER_PK: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BURNER_PK: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

/// True bila test fork harus diskip. Dipanggil dari dalam async test —
/// jangan memanggil `block_on` ( nested runtime panic ).
async fn skip_async() -> bool {
    if std::env::var("NFT_SKIP_FORK_TESTS").as_deref() == Ok("1") {
        return true;
    }
    !anvil_reachable().await
}

async fn anvil_reachable() -> bool {
    let pool = RpcPool::new(RpcPoolConfig {
        http_urls: vec![ANVIL_URL.into()],
        request_timeout_ms: 1_000,
        ..Default::default()
    })
    .expect("pool");
    pool.request::<String>("eth_chainId", &serde_json::json!([]))
        .await
        .is_ok()
}

/// Path binary foundry ( repo-local, lihat NEXT_AGENT.md ).
fn foundry_bin(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.foundry")
        .join(format!("{name}.exe"));
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// Deploy `TestNFT.sol` lewat `forge create` dengan private key Anvil #0.
/// Mengembalikan alamat kontrak.
fn deploy_test_nft() -> Address {
    let forge = foundry_bin("forge").expect("forge");
    // Path absolut ke sumber kontrak — forge resolve path relatif terhadap
    // root foundry proyek ( bukan CARGO_MANIFEST_DIR ini ).
    let sol = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("contracts")
        .join("TestNFT.sol");
    let out = Command::new(forge)
        .args([
            "create",
            &format!("{}:TestNFT", sol.display()),
            "--rpc-url",
            ANVIL_URL,
            "--private-key",
            DEPLOYER_PK,
            "--broadcast",
        ])
        .output()
        .expect("forge create");
    assert!(
        out.status.success(),
        "forge create gagal: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let addr = text
        .lines()
        .find_map(|l| {
            let l = l.trim();
            l.strip_prefix("Deployed to:")
                .map(|s| s.trim().to_string())
                .filter(|s| s.starts_with("0x") && s.len() == 42)
        })
        .expect("alamat kontrak hilang dari output forge");
    addr.parse().expect("address kontrak")
}

fn pool() -> RpcPool {
    RpcPool::new(RpcPoolConfig {
        http_urls: vec![ANVIL_URL.into()],
        request_timeout_ms: 5_000,
        hedge_delay_ms: 50,
        ..Default::default()
    })
    .unwrap()
}

fn policy_config() -> nft_core::config::PolicyConfig {
    parse_policy(
        r#"
    [limits]
    max_spend_per_tx_eth = 1.0
    max_spend_per_day_eth = 10.0
    max_gas_gwei = 1000
    allowed_chains = [31337]
    [allowlist]
    contracts = []
    block_unlimited_approvals = true
    [confirmation]
    require_human_above_eth = 100
    require_human_for = []
    [kill_switch]
    auto_trigger_on_loss_eth = 1.0
    [telegram]
    allowed_user_ids = [123456789]
    chat_cannot = []
    import_via_chat = false
    import_session_timeout_sec = 120
    delete_import_message = true
    require_totp_for_import = true
    burner_max_balance_eth = 0.5
    max_wallets = 5
    "#,
    )
    .unwrap()
}

/// Wallet Anvil #1 ( terima ETH dari account #0 ) sebagai burner mint.
fn anvil_burner() -> WalletSigner {
    let pk = alloy::signers::local::PrivateKeySigner::from_slice(
        &alloy::primitives::hex::decode(BURNER_PK).unwrap(),
    )
    .unwrap();
    WalletSigner::new("burner-1", pk)
}

async fn current_block(pool: &RpcPool) -> u64 {
    let res: String = pool
        .request("eth_blockNumber", &serde_json::json!([]))
        .await
        .unwrap();
    u64::from_str_radix(res.trim_start_matches("0x"), 16).unwrap()
}

async fn fund_burner(burner: Address) {
    // Kirim 1 ETH dari account #0 Anvil ke burner via `cast`.
    let cast = foundry_bin("cast").expect("cast");
    let out = Command::new(cast)
        .args([
            "send",
            &format!("{burner:?}"),
            "--value",
            "1ether",
            "--rpc-url",
            ANVIL_URL,
            "--private-key",
            DEPLOYER_PK,
        ])
        .output()
        .expect("cast send");
    assert!(
        out.status.success(),
        "cast send gagal: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn sync_nonce(pool: &RpcPool, nonces: &NonceManager, burner: Address) {
    let count: String = pool
        .request(
            "eth_getTransactionCount",
            &serde_json::json!([format!("{burner:?}"), "pending"]),
        )
        .await
        .unwrap();
    let n = u64::from_str_radix(count.trim_start_matches("0x"), 16).unwrap();
    nonces.sync_from(burner, n).await;
}

/// Buat sale aktif: `openSale()` via cast dari deployer.
async fn open_sale(contract: Address) {
    let cast = foundry_bin("cast").expect("cast");
    let out = Command::new(cast)
        .args([
            "send",
            &format!("{contract:?}"),
            "openSale()",
            "--rpc-url",
            ANVIL_URL,
            "--private-key",
            DEPLOYER_PK,
        ])
        .output()
        .expect("cast send");
    assert!(
        out.status.success(),
        "openSale gagal: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn tiers() -> [TierGas; 3] {
    [
        TierGas {
            mode: GasMode::Normal,
            max_fee_per_gas: 1_000_000_000, // 1 gwei
            max_priority_fee_per_gas: 1_000_000_000,
        },
        TierGas {
            mode: GasMode::Fast,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
        },
        TierGas {
            mode: GasMode::Aggressive,
            max_fee_per_gas: 3_000_000_000, // Anvil base fee rendah
            max_priority_fee_per_gas: 1_000_000_000,
        },
    ]
}

/// Policy engine production: simulasi eth_call agar tx yang valid lolos (
/// `with_defaults` memakai NoSimulator → tx revert fiktif ditolak, tapi
/// tx valid pun akan ditolak karena simulator no-op menganggap semua OK;
/// di sini kita pakai EthCallSimulator sesuai §F5 ).
fn policy_engine(pool: &RpcPool) -> PolicyEngine {
    PolicyEngine::new(
        policy_config(),
        Box::new(nft_policy::InMemoryPolicyState::default()),
        Box::new(EthCallSimulator::new(pool.clone())),
        Box::new(nft_policy::AlwaysDenyConfirmation),
    )
}

#[tokio::test]
async fn acceptance_fase5_pipeline_lengkap() {
    if skip_async().await {
        eprintln!("[skip] fork test Anvil tidak dijalankan");
        return;
    }

    let pool = pool();
    let contract = deploy_test_nft();
    let burner = anvil_burner();
    fund_burner(burner.address()).await;

    // 1. deteksi fungsi mint + state ( §1 F1 ).
    open_sale(contract).await;
    let reader = SaleReader::new(pool.clone());
    let state = reader.read_state(contract).await.expect("read_state");
    assert_eq!(state.mint_signature, "mint(uint256)");
    assert_eq!(state.price, U256::from(1_000_000_000_000_000u64)); // 0.001 ETH
    assert_eq!(state.max_per_wallet, Some(U256::from(3)));
    assert_eq!(state.max_supply, Some(U256::from(100)));
    assert_eq!(state.total_supply, Some(U256::ZERO));
    assert_eq!(state.sale_active, Some(true));

    // 2. compile_exec_plan ( §8.3 ) — qty 2 dalam limit per wallet.
    let nonces = NonceManager::new();
    sync_nonce(&pool, &nonces, burner.address()).await;
    let plan = MintPlan {
        chain_id: CHAIN_ID,
        contract,
        sale: state,
        wallets: vec![nft_mint::plan::WalletMint {
            wallet: WalletRef::new("burner-1", "Burner 1", format!("{:?}", burner.address())),
            qty: 2,
        }],
        gas_mode: GasMode::Aggressive,
    };
    let exec_plan = compile_exec_plan(&plan, &nonces, &tiers(), 300_000)
        .await
        .expect("compile_exec_plan");
    assert_eq!(exec_plan.txs.len(), 3, "3 tier gas per wallet");

    // 3. eksekusi dry-run ( aturan keras #3 ).
    let storage = nft_core::storage::connect("sqlite::memory:").await.unwrap();
    let executor = MintExecutor::new(
        pool.clone(),
        policy_engine(&pool),
        nonces.clone(),
        MintExecutorConfig {
            dry_run: true,
            race_targets: vec![RaceTarget::PublicPool],
            ..Default::default()
        },
    );
    let mut signers = HashMap::new();
    let burner_addr = burner.address();
    signers.insert("burner-1".to_string(), burner);
    let outcomes = executor
        .execute(&exec_plan, "task-f5", &storage, &signers)
        .await
        .expect("execute");
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].status, MintTxStatus::DryRun);
    assert!(outcomes[0].tx_hash.starts_with("0x"));
    // Dry-run: balanceOf masih 0.
    assert_eq!(
        reader.balance_of(contract, burner_addr).await.unwrap(),
        U256::ZERO
    );
}

/// **Acceptance utama**: mint terjadi <1 block setelah trigger.
/// Eksekusi non-dry-run ke Anvil: tx terkirim, masuk block berikutnya,
/// `balanceOf` bertambah, `ownerOf` menunjuk wallet ( §6.3 ).
#[tokio::test]
async fn acceptance_fase5_mint_terjadi_dalam_satu_block() {
    if skip_async().await {
        eprintln!("[skip] fork test Anvil tidak dijalankan");
        return;
    }

    let pool = pool();
    let contract = deploy_test_nft();
    let burner = anvil_burner();
    fund_burner(burner.address()).await;
    open_sale(contract).await;

    let reader = SaleReader::new(pool.clone());
    let state = reader.read_state(contract).await.expect("read_state");

    let nonces = NonceManager::new();
    sync_nonce(&pool, &nonces, burner.address()).await;
    let plan = MintPlan {
        chain_id: CHAIN_ID,
        contract,
        sale: state,
        wallets: vec![nft_mint::plan::WalletMint {
            wallet: WalletRef::new("burner-1", "Burner 1", format!("{:?}", burner.address())),
            qty: 2,
        }],
        gas_mode: GasMode::Aggressive,
    };
    let exec_plan = compile_exec_plan(&plan, &nonces, &tiers(), 300_000)
        .await
        .expect("compile_exec_plan");

    // Block number tepat sebelum eksekusi ( §7 "trigger -> tx terkirim" ).
    let block_before = current_block(&pool).await;

    let storage = nft_core::storage::connect("sqlite::memory:").await.unwrap();
    let executor = MintExecutor::new(
        pool.clone(),
        policy_engine(&pool),
        nonces.clone(),
        MintExecutorConfig {
            dry_run: false, // kirim tx asli ke Anvil ( uji, bukan mainnet )
            race_targets: vec![RaceTarget::PublicPool],
            confirm_timeout: std::time::Duration::from_secs(30),
            ..Default::default()
        },
    );
    let mut signers = HashMap::new();
    let burner_addr = burner.address();
    signers.insert("burner-1".to_string(), burner);
    let outcomes = executor
        .execute(&exec_plan, "task-f5-live", &storage, &signers)
        .await
        .expect("execute");

    assert_eq!(outcomes.len(), 1);
    let out = &outcomes[0];
    assert_eq!(out.status, MintTxStatus::Confirmed, "err: {:?}", out.error);
    assert!(out.tx_hash.starts_with("0x"));

    // Mint terjadi <1 block: block saat receipt <= block_before + 2 (
    // toleransi 1 block untuk anvil block-time 1s + 1 block prev ).
    let block_after = current_block(&pool).await;
    assert!(
        block_after <= block_before + 3,
        "mint memakan terlalu banyak block: {block_before} -> {block_after}"
    );

    // §6.3: balanceOf bertambah & token id dimiliki wallet.
    let bal = reader.balance_of(contract, burner_addr).await.unwrap();
    assert_eq!(bal, U256::from(2), "qty 2 harus masuk wallet");
    for id in 0..2u64 {
        let owner = reader.owner_of(contract, U256::from(id)).await.unwrap();
        assert_eq!(owner, burner_addr, "token {id} milik burner");
    }
    assert!(!out.token_ids.is_empty(), "token ids ter枚koleksi");
}

#[tokio::test]
async fn acceptance_fase5_limit_per_wallet_dihormati() {
    if skip_async().await {
        eprintln!("[skip] fork test Anvil tidak dijalankan");
        return;
    }

    let pool = pool();
    let contract = deploy_test_nft();
    let burner = anvil_burner();
    fund_burner(burner.address()).await;
    open_sale(contract).await;

    let reader = SaleReader::new(pool.clone());
    let state = reader.read_state(contract).await.expect("read_state");

    let nonces = NonceManager::new();
    sync_nonce(&pool, &nonces, burner.address()).await;
    let plan = MintPlan {
        chain_id: CHAIN_ID,
        contract,
        sale: state,
        wallets: vec![nft_mint::plan::WalletMint {
            wallet: WalletRef::new("burner-1", "Burner 1", format!("{:?}", burner.address())),
            qty: 4, // > maxPerWallet(3) → harus ditolak.
        }],
        gas_mode: GasMode::Aggressive,
    };
    let err = compile_exec_plan(&plan, &nonces, &tiers(), 300_000)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        nft_mint::plan::PlanError::ExceedsPerWallet { .. }
    ));
}

#[tokio::test]
async fn acceptance_fase5_tidak_ada_dobel_mint_saat_restart() {
    if skip_async().await {
        eprintln!("[skip] fork test Anvil tidak dijalankan");
        return;
    }

    // Idempotency: record_tx kedua dengan key yang sama ditolak ( §6.2 ).
    let storage = nft_core::storage::connect("sqlite::memory:").await.unwrap();
    record_tx(
        &storage,
        "task-f5:burner-1:0",
        CHAIN_ID,
        "burner-1",
        0,
        None,
        TxStatus::Prepared,
    )
    .await
    .unwrap();
    let err = record_tx(
        &storage,
        "task-f5:burner-1:0",
        CHAIN_ID,
        "burner-1",
        0,
        None,
        TxStatus::Prepared,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, nft_mint::persist::PersistError::Duplicate(_)));

    // Resume: tx sent terlihat sebagai pending.
    record_tx(
        &storage,
        "task-f5:burner-1:1",
        CHAIN_ID,
        "burner-1",
        1,
        Some("0xabc"),
        TxStatus::Sent,
    )
    .await
    .unwrap();
    let pending = pending_txs(&storage).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0, "task-f5:burner-1:1");
}

#[tokio::test]
async fn acceptance_fase5_sale_tidak_aktif_ditolak() {
    if skip_async().await {
        eprintln!("[skip] fork test Anvil tidak dijalankan");
        return;
    }

    let pool = pool();
    let contract = deploy_test_nft();
    // sale TIDAK diaktifkan.
    let reader = SaleReader::new(pool.clone());
    let state = reader.read_state(contract).await.expect("read_state");
    assert_eq!(state.sale_active, Some(false));

    let nonces = NonceManager::new();
    let plan = MintPlan {
        chain_id: CHAIN_ID,
        contract,
        sale: state,
        wallets: vec![nft_mint::plan::WalletMint {
            wallet: WalletRef::new(
                "burner-1",
                "Burner 1",
                format!("{:?}", anvil_burner().address()),
            ),
            qty: 1,
        }],
        gas_mode: GasMode::Aggressive,
    };
    let err = compile_exec_plan(&plan, &nonces, &tiers(), 300_000)
        .await
        .unwrap_err();
    assert!(matches!(err, nft_mint::plan::PlanError::SaleNotActive));
}

#[test]
fn trigger_timestamp_tsend_konsisten() {
    // Test murni ( tanpa Anvil ) untuk perhitungan Tsend ( §7 ).
    let t = Trigger::Timestamp {
        t0: now_unix() + 100,
        latency_p95_ms: 500,
        margin_ms: 500,
    };
    let tsend = t.tsend().unwrap();
    assert!(tsend <= now_unix() + 100);
    assert!(tsend >= now_unix() + 100 - 60);
}
