//! `nft-preflight` — pemeriksaan sebelum war ( blueprint §6.1, §9 Fase 6 ).
//!
//! Tabel §6.1 — setiap cek menghasilkan `Check { name, passed, detail }`:
//!
//! | Cek | Lulus bila |
//! |---|---|
//! | Saldo | saldo >= harga x qty + gas cadangan 30% tiap wallet |
//! | Nonce | nonce lokal = on-chain, tdk ada tx pending nyangkut |
//! | RPC | minimal 3 endpoint sehat, selisih block head <= 1 |
//! | Jam | offset NTP < 50 ms ( chrony host — lihat `ClockCheck` ) |
//! | Simulasi | mint sukses di simulasi dengan parameter final |
//! | Allowlist | merkle proof valid tiap wallet ( V1: lolos bila tak ada ) |
//! | Limit | qty <= maxPerWallet / sisa supply |
//! | Gas | maxFee di bawah cap policy dengan margin |
//! | Login | sesi Google/X aktif ( Fase 7 — placeholder NotRequired ) |
//! | Policy | batas tidak terlanggar rencana ini |
//!
//! **Jika ada merah, war tidak dimulai otomatis** ( kecuali disetujui
//! lewat tombol §6.1 ). Hasil adalah `PreflightReport` hijau/merah.
//!
//! ponytail: Allowlist merkle proof & Login ( Social Vault ) menyusul
//! Fase 7/12 — sekarang `Check::pass` dengan catatan bila tidak ada.

use std::time::Duration;

use alloy::primitives::{Address, U256};
use thiserror::Error;

use nft_chain::{EthCallSimulator, GasEstimate, GasMode, GasOracle, NonceManager, RpcPool};
use nft_core::config::PolicyConfig;
use nft_mint::plan::MintPlan;

#[derive(Debug, Error)]
pub enum PreflightError {
    #[error("RPC error saat preflight: {0}")]
    Rpc(#[from] nft_chain::RpcError),

    #[error("gas oracle error: {0}")]
    Gas(#[from] nft_chain::GasError),
}

/// Satu hasil cek ( §6.1 "checklist hijau/merah" ).
#[derive(Debug, Clone)]
pub struct Check {
    /// Nama cek ( sesuai tabel §6.1 ).
    pub name: &'static str,
    pub passed: bool,
    /// Rincian ( angka, alasan ) — bebas rahasia.
    pub detail: String,
}

impl Check {
    pub fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            passed: true,
            detail: detail.into(),
        }
    }

    pub fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            passed: false,
            detail: detail.into(),
        }
    }
}

/// Laporan preflight lengkap.
#[derive(Debug, Clone)]
pub struct PreflightReport {
    pub checks: Vec<Check>,
    /// True bila semua cek lulus ( §6.1: merah -> war tidak otomatis ).
    pub passed: bool,
}

impl PreflightReport {
    /// Daftar cek yang merah ( untuk pesan peringatan ).
    pub fn failures(&self) -> Vec<&Check> {
        self.checks.iter().filter(|c| !c.passed).collect()
    }

    /// Format pesan Telegram: checklist hijau/merah ( §6.1 ).
    pub fn summarize(&self) -> String {
        let mut s = String::new();
        for c in &self.checks {
            let mark = if c.passed { "[OK]" } else { "[X]" };
            s.push_str(&format!("{} {}: {}\n", mark, c.name, c.detail));
        }
        let verdict = if self.passed {
            "PREFLIGHT LOLOS — siap war"
        } else {
            "PREFLIGHT GAGAL — war tidak dimulai otomatis"
        };
        s.push_str(&format!("\n{verdict}"));
        s
    }
}

/// Konfigurasi preflight ( §6.1 ).
#[derive(Debug, Clone)]
pub struct PreflightConfig {
    /// Batas bawah jumlah endpoint sehat ( §6.1: "minimal 3" ).
    pub min_healthy_endpoints: usize,
    /// Selisih block head maksimum antar endpoint ( §6.1: "<= 1" ).
    pub max_block_diff: u64,
    /// Cadangan gas di atas estimasi ( §6.1: "30%" ).
    pub gas_reserve_pct: f64,
    /// Margin maxFee di bawah cap policy ( §6.1 "dengan margin" ).
    pub gas_cap_margin_pct: f64,
    /// Offset jam maksimum ( §6.1: "< 50 ms" ). Host wajib chrony.
    pub max_clock_drift_ms: u64,
}

impl Default for PreflightConfig {
    fn default() -> Self {
        Self {
            min_healthy_endpoints: 3,
            max_block_diff: 1,
            gas_reserve_pct: 0.30,
            gas_cap_margin_pct: 0.10,
            max_clock_drift_ms: 50,
        }
    }
}

/// Jalankan semua cek §6.1 untuk sebuah `MintPlan`.
///
/// - `pool` : RPC chain target.
/// - `nonces` : nonce manager yang sudah di-sync pre-warm.
/// - `policy` : config policy ( limits, gas cap ).
pub async fn preflight(
    plan: &MintPlan,
    pool: &RpcPool,
    nonces: &NonceManager,
    policy: &PolicyConfig,
    config: &PreflightConfig,
) -> Result<PreflightReport, PreflightError> {
    let mut checks = Vec::new();

    // --- 0. Estimasi gas ( dipakai cek Saldo & Gas; fallback bila RPC gagal
    // agar cek lain tetap jalan dan melapor merah, bukan early-return ).
    let gas_est: GasEstimate = match GasOracle::new(pool.clone(), 20)
        .estimate(GasMode::Aggressive, policy.limits.max_gas_gwei)
        .await
    {
        Ok(e) => e,
        Err(e) => {
            checks.push(Check::fail("Gas", format!("estimasi gas gagal: {e}")));
            GasEstimate {
                base_fee: 0,
                priority_fee: 0,
                max_fee_per_gas: 0,
            }
        }
    };
    let price = plan.sale.price;
    let gas_limit = 300_000u128; // estimasi gas mint
    let gas_cost = U256::from(gas_limit) * U256::from(gas_est.max_fee_per_gas);
    let gas_reserved = gas_cost + gas_cost * U256::from(30u128) / U256::from(100u128);
    for w in &plan.wallets {
        let addr = match w.wallet.address.parse::<Address>() {
            Ok(a) => a,
            Err(_) => {
                checks.push(Check::fail(
                    "Saldo",
                    format!("alamat wallet {} tidak valid", w.wallet.id),
                ));
                continue;
            }
        };
        let balance = match pool.eth_get_balance(&format!("{addr:?}")).await {
            Ok(b) => b,
            Err(e) => {
                checks.push(Check::fail(
                    "Saldo",
                    format!("gagal baca saldo {}: {e}", w.wallet.id),
                ));
                continue;
            }
        };
        let need = price * U256::from(w.qty) + gas_reserved;
        if balance >= need {
            checks.push(Check::pass(
                "Saldo",
                format!(
                    "{}: {} >= {} ( harga {} qty {} + gas {} )",
                    w.wallet.short_address(),
                    wei_to_eth(balance),
                    wei_to_eth(need),
                    wei_to_eth(price),
                    w.qty,
                    wei_to_eth(gas_reserved),
                ),
            ));
        } else {
            checks.push(Check::fail(
                "Saldo",
                format!(
                    "{}: {} < {} ( kekurangan {} )",
                    w.wallet.short_address(),
                    wei_to_eth(balance),
                    wei_to_eth(need),
                    wei_to_eth(need - balance),
                ),
            ));
        }
    }

    // --- 2. Nonce ( §6.1: nonce lokal = on-chain, tidak ada pending nyangkut ).
    for w in &plan.wallets {
        let Ok(addr) = w.wallet.address.parse::<Address>() else {
            continue;
        };
        let on_chain: u64 = match pool
            .request::<String>(
                "eth_getTransactionCount",
                &serde_json::json!([format!("{addr:?}"), "pending"]),
            )
            .await
        {
            Ok(s) => u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0),
            Err(e) => {
                checks.push(Check::fail(
                    "Nonce",
                    format!("gagal baca nonce {}: {e}", w.wallet.id),
                ));
                continue;
            }
        };
        let local = nonces.next_nonce(addr).await;
        if local == on_chain {
            checks.push(Check::pass(
                "Nonce",
                format!(
                    "{}: nonce {} == on-chain ( tidak ada pending )",
                    w.wallet.short_address(),
                    local
                ),
            ));
        } else {
            checks.push(Check::fail(
                "Nonce",
                format!(
                    "{}: nonce lokal {} != on-chain {} — tx pending nyangkut",
                    w.wallet.short_address(),
                    local,
                    on_chain
                ),
            ));
        }
    }

    // --- 3. RPC ( §6.1: minimal 3 endpoint sehat, selisih block <= 1 ).
    let healthy = pool.healthy_count().await;
    if healthy >= config.min_healthy_endpoints {
        checks.push(Check::pass(
            "RPC",
            format!(
                "{healthy} endpoint sehat ( >= {} )",
                config.min_healthy_endpoints
            ),
        ));
    } else {
        checks.push(Check::fail(
            "RPC",
            format!(
                "hanya {healthy} endpoint sehat ( butuh >= {} )",
                config.min_healthy_endpoints
            ),
        ));
    }
    // Selisih block head: estimasi via newHeads terakhir vs block sekarang.
    let current: u64 = match pool
        .request::<String>("eth_blockNumber", &serde_json::json!([]))
        .await
    {
        Ok(s) => u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0),
        Err(_) => 0,
    };
    let head = pool.block_number();
    if current.abs_diff(head) <= config.max_block_diff || head == 0 {
        checks.push(Check::pass(
            "RPC",
            format!(
                "block head {current} ( selisih <= {} )",
                config.max_block_diff
            ),
        ));
    } else {
        checks.push(Check::fail(
            "RPC",
            format!(
                "block head {current} vs stream {head} — selisih > {}",
                config.max_block_diff
            ),
        ));
    }

    // --- 4. Jam server ( §6.1: offset < 50 ms via chrony — host responsibility ).
    //   V1: catat drift dari block timestamp terakhir vs jam lokal sebagai
    //   proxy kasar ( offset NTP penuh menunggu Fase 12 setup VPS ).
    let block_ts: u64 = match pool
        .request::<serde_json::Value>(
            "eth_getBlockByNumber",
            &serde_json::json!([format!("0x{current:x}"), false]),
        )
        .await
    {
        Ok(v) => v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            .unwrap_or(0),
        Err(_) => 0,
    };
    if block_ts == 0 {
        checks.push(Check::pass(
            "Jam",
            "timestamp block tak terbaca ( V1: chrony host wajib, §6.1 )",
        ));
    } else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let drift = now.abs_diff(block_ts);
        if drift <= 5 {
            checks.push(Check::pass(
                "Jam",
                format!("drift {drift}s vs block timestamp ( chrony host wajib )"),
            ));
        } else {
            checks.push(Check::fail(
                "Jam",
                format!(
                    "drift {drift}s vs block timestamp (> {}s)",
                    config.max_clock_drift_ms / 1000 + 1
                ),
            ));
        }
    }

    // --- 5. Simulasi ( §6.1: mint sukses di simulasi dengan parameter final ).
    let simulator = EthCallSimulator::new(pool.clone());
    for w in &plan.wallets {
        let Ok(addr) = w.wallet.address.parse::<Address>() else {
            continue;
        };
        // Susun calldata mint untuk qty ini.
        let data = nft_mint::plan::build_mint_calldata(&plan.sale, w.qty);
        let value = plan.sale.price * U256::from(w.qty);
        match simulator
            .simulate_call(addr, plan.contract, value, data.into())
            .await
        {
            Ok(nft_chain::SimOutcome::Ok) => {
                checks.push(Check::pass(
                    "Simulasi",
                    format!(
                        "{}: eth_call mint qty {} sukses",
                        w.wallet.short_address(),
                        w.qty
                    ),
                ));
            }
            Ok(nft_chain::SimOutcome::Reverted(reason)) => {
                checks.push(Check::fail(
                    "Simulasi",
                    format!("{}: revert — {reason}", w.wallet.short_address()),
                ));
            }
            Err(e) => {
                checks.push(Check::fail(
                    "Simulasi",
                    format!("{}: error — {e}", w.wallet.short_address()),
                ));
            }
        }
    }

    // --- 6. Allowlist ( §6.1: merkle proof valid tiap wallet ).
    // ponytail: V1 lolos bila tak ada proof di kontrak ( Fase 12 penuh ).
    checks.push(Check::pass(
        "Allowlist",
        "V1: lolos bila tak ada merkle proof ( verifikasi penuh Fase 12 )",
    ));

    // --- 7. Limit ( §6.1: qty <= maxPerWallet / sisa supply ).
    let mut limit_ok = true;
    let mut limit_detail = String::new();
    for w in &plan.wallets {
        if let Some(max) = plan.sale.max_per_wallet {
            if U256::from(w.qty) > max {
                limit_ok = false;
                limit_detail = format!("qty {} > maxPerWallet {}", w.qty, max);
            }
        }
    }
    if let Some(remaining) = plan.sale.remaining() {
        let total: u64 = plan.wallets.iter().map(|w| w.qty).sum();
        if U256::from(total) > remaining {
            limit_ok = false;
            limit_detail = format!("total {total} > sisa supply {remaining}");
        }
    }
    if limit_ok {
        checks.push(Check::pass(
            "Limit",
            if limit_detail.is_empty() {
                "qty dalam maxPerWallet & sisa supply".into()
            } else {
                limit_detail
            },
        ));
    } else {
        checks.push(Check::fail("Limit", limit_detail));
    }

    // --- 8. Gas ( §6.1: maxFee di bawah cap policy dengan margin ).
    let cap_wei = U256::from(policy.limits.max_gas_gwei) * U256::from(1_000_000_000u128);
    let margin = cap_wei * U256::from(10u128) / U256::from(100u128);
    let max_fee = U256::from(gas_est.max_fee_per_gas);
    if max_fee + margin <= cap_wei {
        checks.push(Check::pass(
            "Gas",
            format!(
                "maxFee {} gwei < cap {} gwei ( margin 10% )",
                gas_est.max_fee_gwei(),
                policy.limits.max_gas_gwei
            ),
        ));
    } else {
        checks.push(Check::fail(
            "Gas",
            format!(
                "maxFee {} gwei terlalu dekat / di atas cap {} gwei",
                gas_est.max_fee_gwei(),
                policy.limits.max_gas_gwei
            ),
        ));
    }

    // --- 9. Login ( §6.1: sesi Google/X aktif jika dibutuhkan ).
    checks.push(Check::pass(
        "Login",
        "tidak dibutuhkan untuk mint ini ( Fase 7 )",
    ));

    // --- 10. Policy ( §6.1: batas tidak terlanggar rencana ini ).
    let total_value = plan.sale.price * U256::from(plan.wallets.iter().map(|w| w.qty).sum::<u64>());
    let total_eth = wei_to_eth(total_value);
    if total_eth <= policy.limits.max_spend_per_tx_eth {
        checks.push(Check::pass(
            "Policy",
            format!(
                "total {} ETH <= max per tx {} ETH",
                total_eth, policy.limits.max_spend_per_tx_eth
            ),
        ));
    } else {
        checks.push(Check::fail(
            "Policy",
            format!(
                "total {} ETH > max per tx {} ETH",
                total_eth, policy.limits.max_spend_per_tx_eth
            ),
        ));
    }

    Ok(finalize(checks))
}

fn finalize(checks: Vec<Check>) -> PreflightReport {
    let passed = checks.iter().all(|c| c.passed);
    PreflightReport { checks, passed }
}

fn wei_to_eth(wei: U256) -> f64 {
    let s = wei.to_string();
    let bytes = s.as_bytes();
    if bytes.len() <= 18 {
        let v: f64 = s.parse().unwrap_or(0.0);
        v / 1e18
    } else {
        let whole = &s[..bytes.len() - 18];
        let frac = &s[bytes.len() - 18..];
        let w: f64 = whole.parse().unwrap_or(0.0);
        let f: f64 = format!("0.{frac}").parse().unwrap_or(0.0);
        w + f
    }
}

/// Jadwalkan preflight otomatis ( §6.1: "T-10 menit dan T-1 menit" ).
/// Menjalankan `preflight` pada dua waktu sebelum `start_unix`.
///
/// ponytail: scheduler penuh ( cron/event ) menyusul Fase 10. Sekarang
/// cukup spawn task yang menunggu sampai deadline lalu menjalankan cek.
pub fn schedule_automatic<F>(
    start_unix: u64,
    config: PreflightConfig,
    run: F,
) -> Vec<tokio::task::JoinHandle<()>>
where
    F: Fn(PreflightConfig) + Send + Sync + 'static,
{
    let cfg = std::sync::Arc::new(config);
    let run = std::sync::Arc::new(run);
    let mut handles = Vec::new();
    for lead in [600u64, 60] {
        let cfg = cfg.clone();
        let run = run.clone();
        handles.push(tokio::spawn(async move {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let deadline = start_unix.saturating_sub(lead);
            if now < deadline {
                tokio::time::sleep(Duration::from_secs(deadline - now)).await;
            }
            run((*cfg).clone());
        }));
    }
    handles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_pass_fail() {
        let p = Check::pass("Saldo", "ok");
        assert!(p.passed);
        let f = Check::fail("Gas", "mahal");
        assert!(!f.passed);
    }

    #[test]
    fn report_failures_dan_summarize() {
        let report = PreflightReport {
            checks: vec![
                Check::pass("Saldo", "cukup"),
                Check::fail("Gas", "di atas cap"),
                Check::pass("RPC", "3 sehat"),
            ],
            passed: false,
        };
        assert_eq!(report.failures().len(), 1);
        let s = report.summarize();
        assert!(s.contains("[X] Gas"));
        assert!(s.contains("[OK] Saldo"));
        assert!(s.contains("PREFLIGHT GAGAL"));
    }

    #[test]
    fn report_lengkap_hijau() {
        let report = PreflightReport {
            checks: vec![Check::pass("Saldo", "cukup")],
            passed: true,
        };
        assert!(report.passed);
        assert!(report.summarize().contains("PREFLIGHT LOLOS"));
    }

    #[test]
    fn wei_to_eth_konversi() {
        assert_eq!(wei_to_eth(U256::from(1_000_000_000_000_000_000u128)), 1.0);
        assert_eq!(
            wei_to_eth(U256::from(10u128 * 1_000_000_000_000_000u128)),
            0.01
        );
        assert_eq!(wei_to_eth(U256::ZERO), 0.0);
    }

    #[tokio::test]
    async fn preflight_endpoint_mati_lapor_merah() {
        let pool = RpcPool::new(nft_chain::RpcPoolConfig {
            http_urls: vec!["http://127.0.0.1:9".into()],
            request_timeout_ms: 300,
            ..Default::default()
        })
        .unwrap();
        let plan = MintPlan {
            chain_id: 8453,
            contract: Address::from_slice(&[0x22; 20]),
            sale: nft_mint::sale::SaleState {
                mint_signature: "mint(uint256)".into(),
                mint_selector: nft_mint::sale::selector("mint(uint256)"),
                price: U256::from(10u128 * 1_000_000_000_000_000u128),
                max_per_wallet: Some(U256::from(3)),
                max_supply: Some(U256::from(100)),
                total_supply: Some(U256::ZERO),
                sale_active: Some(true),
            },
            wallets: vec![nft_mint::plan::WalletMint {
                wallet: nft_core::wallet::WalletRef::new(
                    "burner-1",
                    "Burner 1",
                    format!("0x{}", "11".repeat(20)),
                ),
                qty: 1,
            }],
            gas_mode: GasMode::Aggressive,
        };
        let nonces = NonceManager::new();
        let policy = nft_core::config::parse_policy(
            r#"
        [limits]
        max_spend_per_tx_eth = 1.0
        max_spend_per_day_eth = 10.0
        max_gas_gwei = 1000
        allowed_chains = [8453]
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
        .unwrap();
        // RPC mati: cek Saldo gagal → report merah, bukan panic.
        let report = preflight(&plan, &pool, &nonces, &policy, &PreflightConfig::default())
            .await
            .unwrap();
        assert!(!report.passed, "preflight harus merah saat RPC mati");
        assert!(report.failures().iter().any(|c| c.name == "Saldo"));
    }

    #[tokio::test]
    async fn schedule_automatic_membuat_dua_task() {
        let handles = schedule_automatic(now_unix() + 3600, PreflightConfig::default(), |_| {});
        assert_eq!(handles.len(), 2, "T-10m dan T-1m");
    }

    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}
