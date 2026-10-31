//! Perintah `/mint <link>`, `/preflight`, `/war` ( blueprint §5, §6.1, §F6 ).
//!
//! Alur ( §5 ):
//! ```text
//! /mint <link> -> resolve_mint_link -> verify_contract ( on-chain )
//!   -> build_mint_plan -> ringkasan + info risiko
//!   -> [user konfirmasi] -> preflight -> eksekusi ( Fase 5 MintExecutor )
//! ```
//! - Link yang **gagal verifikasi** atau berisiko tinggi tidak diproses
//!   otomatis; balasan berisi peringatan jelas ( §5 poin 4 ).
//! - Tidak ada rahasia di pesan: alamat disingkat `WalletRef::short_address`.
//! - Eksekusi mint sebenarnya diikat konfirmasi tombol + kill switch (
//!   aturan keras #2, #3 ); command ini hanya **menyusun rencana**.
//!
//! ponytail: `/war` membutuhkan trigger ( §7 ) yang dipasang scheduler
//! Fase 10; sekarang `/war <link>` langsung resolve+verify+preflight dan
//! melaporkan kesiapan.

use nft_chain::{GasMode, RpcPool};
use nft_core::config::PolicyConfig;
use nft_mint::plan::MintPlan;
use nft_preflight::{preflight, PreflightConfig, PreflightReport};

use nft_linkresolver::{
    build_mint_plan, resolve_mint_link, summarize, verify_contract, RiskLevel, UserCtx,
};

/// Hasil pemrosesan `/mint <link>` untuk dikirim ke chat.
#[derive(Debug, Clone)]
pub struct MintCommandOutcome {
    /// Pesan siap kirim ( ringkasan / laporan / error ).
    pub text: String,
    /// True bila link valid & siap konfirmasi ( keyboard ditampilkan ).
    pub needs_confirmation: bool,
    /// Rencana yang sudah diverifikasi ( bila needs_confirmation ).
    pub plan: Option<MintPlan>,
}

/// Proses `/mint <link>`: resolve -> verify on-chain -> build_mint_plan.
///
/// `pool` ke chain target ( dari chain config ). Bila link gagal verifikasi
/// atau risiko tinggi, kembalikan pesan peringatan tanpa rencana.
pub async fn handle_mint_link(
    link: &str,
    pool: &RpcPool,
    wallets: &[nft_core::wallet::WalletRef],
) -> MintCommandOutcome {
    // 1. Resolve link -> kandidat.
    let candidate = match resolve_mint_link(link).await {
        Ok(c) => c,
        Err(e) => {
            return MintCommandOutcome {
                text: format!("[X] Link tidak valid: {e}"),
                needs_confirmation: false,
                plan: None,
            };
        }
    };

    // 2. Verifikasi on-chain ( §5 poin 2-4 ).
    let verified = match verify_contract(pool, &candidate).await {
        Ok(v) => v,
        Err(e) => {
            return MintCommandOutcome {
                text: format!(
                    "[X] Verifikasi kontrak gagal untuk {}: {e}",
                    candidate.domain
                ),
                needs_confirmation: false,
                plan: None,
            };
        }
    };

    // 3. Bangun rencana ( §5 poin 3 ).
    let ctx = UserCtx {
        wallets: wallets.to_vec(),
        gas_mode: GasMode::Aggressive,
    };
    let plan = match build_mint_plan(&verified, &ctx) {
        Ok(p) => p,
        Err(e) => {
            return MintCommandOutcome {
                text: format!("[X] Rencana mint tidak bisa disusun: {e}"),
                needs_confirmation: false,
                plan: None,
            };
        }
    };

    // 4. Ringkasan + peringatan risiko ( §5 poin 3-4 ).
    let mut text = summarize(&verified, &plan);
    if let Some(warning) = verified.risk.warning() {
        text.push_str(&format!("\n\n[!] {warning}"));
    }

    // Risiko Critical tidak diproses otomatis bahkan dengan konfirmasi.
    if verified.risk == RiskLevel::Critical {
        return MintCommandOutcome {
            text: format!("{text}\n\n[X] Risiko KRITIS — proses dibatalkan."),
            needs_confirmation: false,
            plan: None,
        };
    }

    MintCommandOutcome {
        needs_confirmation: true,
        plan: Some(plan),
        text,
    }
}

/// Jalankan preflight ( §6.1 ) untuk rencana yang sudah dikonfirmasi.
pub async fn run_preflight(
    plan: &MintPlan,
    pool: &RpcPool,
    nonces: &nft_chain::NonceManager,
    policy: &PolicyConfig,
) -> PreflightReport {
    preflight(plan, pool, nonces, policy, &PreflightConfig::default())
        .await
        .unwrap_or_else(|e| PreflightReport {
            checks: vec![nft_preflight::Check::fail(
                "Preflight",
                format!("error: {e}"),
            )],
            passed: false,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mint_link_invalid_error_jelas() {
        let pool = RpcPool::new(nft_chain::RpcPoolConfig {
            http_urls: vec!["http://127.0.0.1:9".into()],
            request_timeout_ms: 300,
            ..Default::default()
        })
        .unwrap();
        let out = handle_mint_link("bukan-url", &pool, &[]).await;
        assert!(!out.needs_confirmation);
        assert!(out.text.contains("[X]"));
    }

    #[tokio::test]
    async fn mint_link_domain_tidak_terjangkau_error_jelas() {
        let pool = RpcPool::new(nft_chain::RpcPoolConfig {
            http_urls: vec!["http://127.0.0.1:9".into()],
            request_timeout_ms: 300,
            ..Default::default()
        })
        .unwrap();
        let out = handle_mint_link("http://127.0.0.1:9/drop", &pool, &[]).await;
        assert!(!out.needs_confirmation);
        assert!(out.text.contains("[X]"));
    }

    #[tokio::test]
    async fn preflight_report_merah_saat_rpc_mati() {
        let pool = RpcPool::new(nft_chain::RpcPoolConfig {
            http_urls: vec!["http://127.0.0.1:9".into()],
            request_timeout_ms: 300,
            ..Default::default()
        })
        .unwrap();
        let plan = MintPlan {
            chain_id: 8453,
            contract: alloy::primitives::Address::from_slice(&[0x22; 20]),
            sale: nft_mint::sale::SaleState {
                mint_signature: "mint(uint256)".into(),
                mint_selector: nft_mint::sale::selector("mint(uint256)"),
                price: alloy::primitives::U256::from(1u128),
                max_per_wallet: Some(alloy::primitives::U256::from(3)),
                max_supply: Some(alloy::primitives::U256::from(100)),
                total_supply: Some(alloy::primitives::U256::ZERO),
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
        let nonces = nft_chain::NonceManager::new();
        let report = run_preflight(&plan, &pool, &nonces, &policy).await;
        assert!(!report.passed);
        assert!(report.summarize().contains("PREFLIGHT GAGAL"));
    }
}
