//! Integration test Fase 6 ( blueprint §9 Fase 6 acceptance ).
//!
//! Acceptance:
//! - 10 link uji ( valid + scam/typosquat + drainer ) diproses benar.
//! - **Link scam tidak pernah jadi tx** ( §5 poin 4 ).
//! - Preflight merah mencegah war otomatis ( §6.1 ).
//!
//! Setup: Anvil di `http://127.0.0.1:8545` + mock HTTP server yang
//! menyajikan halaman drop berisi alamat kontrak. Test ini mendeploy
//! `TestNFT.sol` ( valid ) dan `DrainerNFT.sol` ( berbahaya ) lewat forge.
//!
//! Diskip otomatis bila Anvil tak terjangkau ( `NFT_SKIP_FORK_TESTS=1` ).

use std::path::PathBuf;
use std::process::Command;

use alloy::primitives::{Address, U256};
use nft_chain::{GasMode, NonceManager, RpcPool, RpcPoolConfig};
use nft_core::wallet::WalletRef;
use nft_linkresolver::{build_mint_plan, resolve_mint_link, verify_contract, RiskLevel, UserCtx};
use nft_preflight::{preflight, PreflightConfig};

const ANVIL_URL: &str = "http://127.0.0.1:8545";
const DEPLOYER_PK: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

async fn skip_async() -> bool {
    if std::env::var("NFT_SKIP_FORK_TESTS").as_deref() == Ok("1") {
        return true;
    }
    let pool = RpcPool::new(RpcPoolConfig {
        http_urls: vec![ANVIL_URL.into()],
        request_timeout_ms: 1_000,
        ..Default::default()
    })
    .expect("pool");
    pool.request::<String>("eth_chainId", &serde_json::json!([]))
        .await
        .is_err()
}

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

/// Deploy contract uji via forge; kembalikan alamat.
fn deploy(sol_path: &str, name: &str) -> Address {
    let forge = foundry_bin("forge").expect("forge");
    let target = format!("{sol_path}:{name}");
    let out = Command::new(forge)
        .args([
            "create",
            &target,
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
        "forge create {name} gagal: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|l| {
            let l = l.trim();
            l.strip_prefix("Deployed to:")
                .map(|s| s.trim().to_string())
                .filter(|s| s.starts_with("0x") && s.len() == 42)
        })
        .unwrap_or_else(|| panic!("alamat {name} hilang dari output forge"))
        .parse()
        .expect("address")
}

fn sol(name: &str) -> String {
    // TestNFT.sol milik crate mint ( dipakai bersama ); DrainerNFT.sol
    // milik crate ini.
    let mint_sol = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../mint/contracts")
        .join(name);
    if mint_sol.exists() {
        return mint_sol.to_str().unwrap().to_string();
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("contracts")
        .join(name)
        .to_str()
        .unwrap()
        .to_string()
}

fn pool() -> RpcPool {
    RpcPool::new(RpcPoolConfig {
        http_urls: vec![ANVIL_URL.into()],
        request_timeout_ms: 5_000,
        ..Default::default()
    })
    .unwrap()
}

/// Mock HTTP server yang menyajikan halaman drop berisi alamat kontrak.
/// Mengembalikan ( base_url, server_handle ).
async fn mock_drop_page(addr: Address, chain_hint: &str) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let body = format!(
        r#"<html><head><meta property="og:nft:contract" content="{addr:?}"></head>
        <body><script type="application/json">{{
            "collection": {{ "contract": "{addr:?}", "chain": "{chain_hint}" }},
            "nft": {{ "address": "{addr:?}" }}
        }}</script></body></html>"#,
    );
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let body = body.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                use tokio::io::AsyncWriteExt;
                let mut buf = [0u8; 1024];
                let mut stream = stream;
                let _ = stream.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}/drop"), handle)
}

fn policy_config() -> nft_core::config::PolicyConfig {
    nft_core::config::parse_policy(
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
    allowed_user_ids = [1]
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

/// **Acceptance: 10 link uji** diproses benar.
/// 1-5: valid ( TestNFT terverifikasi di mock page ).
/// 6: typosquat ( open-sea.io ) → resolve error.
/// 7: domain non-legit tanpa kontrak → NoContract.
/// 8: DrainerNFT → risk High → perlu konfirmasi & tidak auto-proses.
/// 9: URL scheme invalid.
/// 10: port tak terjangkau.
#[tokio::test]
async fn acceptance_fase6_sepuluh_link_uji() {
    if skip_async().await {
        eprintln!("[skip] fork test Anvil tidak dijalankan");
        return;
    }
    let pool = pool();

    // Deploy TestNFT ( valid ) + DrainerNFT ( berbahaya ).
    let nft = deploy(&sol("TestNFT.sol"), "TestNFT");
    let drainer = deploy(&sol("DrainerNFT.sol"), "DrainerNFT");

    // Buka sale TestNFT supaya read_state lengkap.
    let cast = foundry_bin("cast").expect("cast");
    let out = Command::new(cast)
        .args([
            "send",
            &format!("{nft:?}"),
            "openSale()",
            "--rpc-url",
            ANVIL_URL,
            "--private-key",
            DEPLOYER_PK,
        ])
        .output()
        .expect("cast send");
    assert!(out.status.success(), "openSale gagal");

    let (valid_url, _h) = mock_drop_page(nft, "base").await;
    let (drainer_url, _h2) = mock_drop_page(drainer, "base").await;

    let wallets = vec![WalletRef::new(
        "burner-1",
        "Burner 1",
        format!("0x{}", "11".repeat(20)),
    )];
    let ctx = UserCtx {
        wallets: wallets.clone(),
        gas_mode: GasMode::Aggressive,
    };

    let mut valid_count = 0;
    let mut blocked = 0;

    // Link 1-5: variasi path valid ( resolve + verify + plan OK, Low/Medium ).
    for path in ["/drop", "/drop/x", "/collection/abc", "/mint", "/nft/1"] {
        let link = valid_url.replace("/drop", path);
        let cand = resolve_mint_link(&link).await.expect("resolve valid");
        assert_eq!(cand.contract, nft);
        let v = verify_contract(&pool, &cand).await.expect("verify valid");
        assert!(v.sale.price > U256::ZERO, "price terbaca");
        let plan = build_mint_plan(&v, &ctx).expect("plan valid");
        assert!(!plan.wallets.is_empty());
        valid_count += 1;
    }

    // Link 6: typosquat → resolve error.
    let res = resolve_mint_link("https://open-sea.io/drop").await;
    assert!(res.is_err(), "typosquat harus ditolak");
    blocked += 1;

    // Link 7: domain acak, halaman tanpa kontrak → NoContract.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let empty_page = "<html><body>no contract here</body></html>";
    let h4 = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
                empty_page.len(),
                empty_page
            );
            use tokio::io::AsyncWriteExt;
            let _ = stream.write_all(resp.as_bytes()).await;
        }
    });
    let res = resolve_mint_link(&format!("http://127.0.0.1:{port}/nope")).await;
    assert!(
        res.is_err(),
        "halaman tanpa kontrak → error, dapat: {res:?}"
    );
    blocked += 1;
    drop(h4);

    // Link 8: DrainerNFT → risk High → warning + needs confirmation.
    let cand = resolve_mint_link(&drainer_url)
        .await
        .expect("resolve drainer");
    let v = verify_contract(&pool, &cand).await.expect("verify drainer");
    assert!(v.risk.needs_human(), "drainer harus High/Critical");
    assert!(v.risk.warning().is_some());
    blocked += 1;

    // Link 9: URL scheme invalid.
    let res = resolve_mint_link("ftp://x.test/drop").await;
    assert!(res.is_err());
    blocked += 1;

    // Link 10: port tak terjangkau.
    let res = resolve_mint_link("http://127.0.0.1:9/drop").await;
    assert!(res.is_err(), "host mati → error, dapat: {res:?}");
    blocked += 1;

    assert_eq!(valid_count, 5, "5 link valid harus lolos verifikasi");
    assert_eq!(blocked, 5, "5 link bermasalah harus diblokir");
}

/// **Acceptance: link scam tidak pernah jadi tx.**
/// DrainerNFT ( Critical/High ) → build plan diizinkan untuk konfirmasi
/// tapi risk warning dikirim; bila RiskLevel::Critical, plan tidak dibuat.
/// Eksekusi nyata di Fase 5 memerlukan policy_check, di sini kita verifikasi
/// saja: resolve+verify+plan pada drainer menghasilkan needs_human.
#[tokio::test]
async fn acceptance_fase6_link_scam_tidak_jadi_tx() {
    if skip_async().await {
        eprintln!("[skip] fork test Anvil tidak dijalankan");
        return;
    }
    let pool = pool();
    let drainer = deploy(&sol("DrainerNFT.sol"), "DrainerNFT");
    let (url, _h) = mock_drop_page(drainer, "base").await;

    let cand = resolve_mint_link(&url).await.unwrap();
    let v = verify_contract(&pool, &cand).await.unwrap();

    // Drainer → minimal High ( setApprovalForAll ).
    assert!(matches!(v.risk, RiskLevel::High | RiskLevel::Critical));
    // Plan tetap bisa dibuat ( untuk konfirmasi manual ), TAPI risk
    // wajib needs_human — _eksekusi_ otomatis dilarang ( §5 poin 4 ).
    let ctx = UserCtx {
        wallets: vec![WalletRef::new("w", "w", format!("0x{}", "11".repeat(20)))],
        gas_mode: GasMode::Aggressive,
    };
    assert!(v.risk.needs_human());
    let _plan = build_mint_plan(&v, &ctx).expect("plan untuk konfirmasi manusia");
}

/// **Acceptance: preflight merah mencegah war otomatis.**
/// Preflight pada chain Anvil dengan wallet tak terdaftar → merah.
#[tokio::test]
async fn acceptance_fase6_preflight_merah_mencegah_war() {
    if skip_async().await {
        eprintln!("[skip] fork test Anvil tidak dijalankan");
        return;
    }
    let pool = pool();
    let nft = deploy(&sol("TestNFT.sol"), "TestNFT");

    // Sale TIDAK dibuka → sale_active false. Preflight pada plan dengan
    // sale belum aktif: compile_exec_plan menolak; di sini preflight
    // dijalankan pada plan sintetis untuk verifikasi merah RPC ( block
    // head drift ).
    let plan = nft_mint::plan::MintPlan {
        chain_id: 31_337,
        contract: nft,
        sale: nft_mint::sale::SaleState {
            mint_signature: "mint(uint256)".into(),
            mint_selector: nft_mint::sale::selector("mint(uint256)"),
            price: U256::from(1_000_000_000_000_000u64),
            max_per_wallet: Some(U256::from(3)),
            max_supply: Some(U256::from(100)),
            total_supply: Some(U256::ZERO),
            sale_active: Some(false),
        },
        wallets: vec![nft_mint::plan::WalletMint {
            wallet: WalletRef::new("w", "w", format!("0x{}", "11".repeat(20))),
            qty: 1,
        }],
        gas_mode: GasMode::Aggressive,
    };
    let nonces = NonceManager::new();
    let report = preflight(
        &plan,
        &pool,
        &nonces,
        &policy_config(),
        &PreflightConfig::default(),
        None,
    )
    .await
    .unwrap();
    // Wallet 0x11..11 tak ada di anvil → saldo 0 < kebutuhan → merah.
    assert!(!report.passed, "preflight harus merah, dapat: {report:?}");
    let s = report.summarize();
    assert!(s.contains("PREFLIGHT GAGAL"));
    // Setidaknya satu cek menyebut Saldo.
    assert!(report.checks.iter().any(|c| c.name == "Saldo"));
}
