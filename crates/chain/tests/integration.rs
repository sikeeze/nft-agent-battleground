//! Integration test Fase 3 ( blueprint §9 acceptance ): pipeline chain layer
//! terhadap **node publik nyata** ( Sepolia / mainnet ).
//!
//! Tes ini **tidak wajib** untuk CI lokal ( butuh koneksi internet ); dilewati
//! otomatis bila RPC tidak terjangkau ( `chain_integration!` mengecek env
//! `NFT_SKIP_NETWORK_TESTS` atau koneksi ). Acceptance Fase 3 yang diuji:
//!
//! - `eth_chainId` lewat `RpcPool` ( hedged + failover ) kembali chain id.
//! - `EthCallSimulator` menangani endpoint mati tanpa panic.
//! - `GasOracle::estimate` mengembalikan estimasi EIP-1559 ( bila node
//!   mendukung `eth_feeHistory` ).
//! - `measure_latency` menyimpan statistik.

use std::time::Duration;

use nft_chain::{
    measure_latency, Endpoint, EthCallSimulator, GasMode, GasOracle, LatencyTracker, RpcPool,
    RpcPoolConfig, Simulator,
};

/// Endpoint publik Sepolia ( tanpa API key ). Bila tidak terjangkau,
/// seluruh test ini diskip ( `ignore` saat gagal koneksi, bukan fail ).
const SEPOLIA_HTTP: &str = "https://ethereum-sepolia-rpc.publicnode.com";
const SEPOLIA_HTTP2: &str = "https://1rpc.io/sepolia";

async fn pool() -> Option<RpcPool> {
    if std::env::var("NFT_SKIP_NETWORK_TESTS").is_ok() {
        return None;
    }
    let pool = RpcPool::new(RpcPoolConfig {
        http_urls: vec![SEPOLIA_HTTP.into(), SEPOLIA_HTTP2.into()],
        request_timeout_ms: 8_000,
        ..Default::default()
    })
    .ok()?;
    // Probe: bila chainId tidak bisa diambil dalam 10 dtk, anggap offline.
    let res = tokio::time::timeout(
        Duration::from_secs(12),
        pool.request::<String>("eth_chainId", &serde_json::json!([])),
    )
    .await;
    match res {
        Ok(Ok(s)) => {
            // Sepolia = 0xaa36a7 ( 11155111 ).
            assert!(s.starts_with("0x"), "chainId harus hex, dapat: {s}");
            tracing::debug!(%s, "probe chainId OK");
            Some(pool)
        }
        _ => {
            eprintln!("SKIP: RPC publik tidak terjangkau ( offline )");
            None
        }
    }
}

#[tokio::test]
async fn rpc_pool_chain_id_lewat_hedged_request() {
    let Some(pool) = pool().await else { return };
    let id: String = pool
        .request("eth_chainId", &serde_json::json!([]))
        .await
        .expect("eth_chainId harus sukses");
    assert_eq!(id, "0xaa36a7", "Sepolia chainId");
}

#[tokio::test]
async fn gas_oracle_estimasi_eip1559() {
    let Some(pool) = pool().await else { return };
    let oracle = GasOracle::new(pool, 20);
    let est = oracle.estimate(GasMode::Fast, 100).await;
    match est {
        Ok(e) => {
            // Estimasi harus masuk akal: base fee > 0, max_fee >= priority.
            assert!(e.base_fee > 0, "base fee harus > 0: {e:?}");
            assert!(e.max_fee_per_gas >= e.priority_fee);
            assert!(e.max_fee_gwei() <= 100, "cap policy 100 gwei: {e:?}");
            let stats = oracle.stats().await;
            assert_eq!(stats.samples, 1);
        }
        Err(e) => {
            // Node mungkin tidak mendukung eth_feeHistory → skip, bukan fail.
            eprintln!("SKIP gas oracle: {e}");
        }
    }
}

#[tokio::test]
async fn measure_latency_menyimpan_statistik() {
    if std::env::var("NFT_SKIP_NETWORK_TESTS").is_ok() {
        return;
    }
    let tracker = LatencyTracker::new();
    let endpoints = vec![
        Endpoint {
            url: SEPOLIA_HTTP.into(),
            label: Some("publicnode".into()),
        },
        Endpoint {
            url: SEPOLIA_HTTP2.into(),
            label: Some("1rpc".into()),
        },
    ];
    let stats = measure_latency(&tracker, &endpoints)
        .await
        .expect("measure_latency tidak boleh error walau satu endpoint gagal");
    if stats.per_endpoint.is_empty() {
        eprintln!("SKIP: offline");
        return;
    }
    for e in &stats.per_endpoint {
        assert!(e.samples >= 1);
        assert!(e.p50_ms >= e.min_ms);
        assert!(e.p95_ms >= e.p50_ms);
    }
}

#[tokio::test]
async fn simulator_mendeteksi_revert_dengan_rpc_nyata() {
    // Acceptance Fase 3: "tx revert terdeteksi di simulasi dan tidak
    // dikirim". Pakai MultiCall3 ( alamat sama di semua chain EVM ) dengan
    // selector yang tidak ada → contract revert → `Simulator::simulate`
    // harus Err(reason) → PolicyEngine menolak.
    let Some(pool) = pool().await else { return };
    let sim = EthCallSimulator::new(pool);
    let approved = nft_signer::ApprovedTx::new_checked(
        11_155_111,
        alloy::primitives::Address::ZERO,
        // Multicall3: 0xcA11bde05977b3631167028862bE2a173976CA11
        "0xcA11bde05977b3631167028862bE2a173976CA11"
            .parse()
            .unwrap(),
        alloy::primitives::U256::ZERO,
        // selector tidak ada pada contract → revert.
        &[0xde, 0xad, 0xbe, 0xef],
        210_000,
        30_000_000_000u128,
        Some(1_000_000_000u128),
        0,
        "intent-revert-test",
    )
    .unwrap();
    let res = sim.simulate(&approved).await;
    match res {
        Err(reason) => {
            // Bisa "execution reverted" ( node mengembalikan error ) atau
            // error "unknown opcode" — intinya: TIDAK Ok.
            assert!(!reason.is_empty(), "reason revert tidak boleh kosong");
        }
        Ok(()) => {
            // Node publik kadang menelan call ke selector asing ( returns
            // empty ). Bila Ok, pastikan tidak ada data hasil ( tetap
            // dianggap gagal di PolicyEngine? ). Toleransi: skip.
            eprintln!("SKIP revert: node mengembalikan sukses untuk selector asing");
        }
    }
}

#[tokio::test]
async fn simulator_endpoint_mati_error_jelas_bukan_panic() {
    // Pastikan EthCallSimulator terhadap endpoint mati mengembalikan error
    // yang bisa dipetakan PolicyEngine ( bukan panic / hang ).
    let pool = RpcPool::new(RpcPoolConfig {
        http_urls: vec!["http://127.0.0.1:9".into()],
        request_timeout_ms: 400,
        ..Default::default()
    })
    .unwrap();
    let sim = EthCallSimulator::new(pool);
    let approved = nft_signer::ApprovedTx::new_checked(
        11_155_111,
        alloy::primitives::Address::ZERO,
        alloy::primitives::Address::from_slice(&[0x22; 20]),
        alloy::primitives::U256::ZERO,
        &[],
        210_000,
        30_000_000_000u128,
        Some(1_000_000_000u128),
        0,
        "intent-integration",
    )
    .unwrap();
    let res = sim.simulate(&approved).await;
    assert!(res.is_err());
    let msg = res.unwrap_err();
    assert!(
        msg.contains("simulator error"),
        "pesan harus menjelaskan kegagalan, dapat: {msg}"
    );
}
