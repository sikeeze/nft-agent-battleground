//! `build_mint_plan` ( blueprint §8.3, §5 poin 3 ).
//!
//! Mengubah `VerifiedTarget` menjadi `MintPlan` ( nft-mint ) yang siap
//! untuk `compile_exec_plan`. Qty per wallet dihitung dari `maxPerWallet`
//! dan sisa supply ( §5 poin 3 "Qty: 3 + 3" ), dipotong bila kurang.
//!
//! Output text ringkasan ( §5 poin 3 ) untuk pesan Telegram:
//! ```text
//! Koleksi: XYZ (Base)   Kontrak: 0xAb..12 [OK] verified
//! Harga: 0.01 ETH   Limit: 3/wallet   Supply: 4,210/10,000
//! Wallet: burner-1, burner-2     Qty: 3 + 3
//! Gas: aggressive (cap 40 gwei)  Risiko: RENDAH
//! ```

use alloy::primitives::U256;
use thiserror::Error;

use nft_chain::GasMode;
use nft_core::wallet::WalletRef;
use nft_mint::plan::{MintPlan, WalletMint};
use nft_mint::sale::SaleState;

use crate::verify::{RiskLevel, VerifiedTarget};

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("qty per wallet {qty} melebihi max_per_wallet {max}")]
    ExceedsPerWallet { qty: u64, max: u64 },

    #[error("total qty {total} melebihi sisa supply {remaining}")]
    ExceedsSupply { total: u64, remaining: u64 },
}

/// Konteks user: wallet burner yang akan dipakai ( dari keystore ).
#[derive(Debug, Clone)]
pub struct UserCtx {
    pub wallets: Vec<WalletRef>,
    pub gas_mode: GasMode,
}

/// Susun `MintPlan` dari target terverifikasi + konteks user.
/// qty per wallet = min( max_per_wallet atau default 1, sisa supply ).
pub fn build_mint_plan(target: &VerifiedTarget, ctx: &UserCtx) -> Result<MintPlan, PlanError> {
    let sale = &target.sale;
    let max_per_wallet = sale
        .max_per_wallet
        .map(|m| m.to::<u64>())
        .unwrap_or(1)
        .max(1);

    // Qty per wallet: max_per_wallet, dipotong bila sisa supply lebih kecil.
    let remaining = sale.remaining().map(|r| r.to::<u64>()).unwrap_or(u64::MAX);
    let n_wallets = ctx.wallets.len().max(1);
    let qty_each = max_per_wallet.min(remaining / n_wallets as u64);
    if qty_each == 0 {
        return Err(PlanError::ExceedsSupply {
            total: 0,
            remaining,
        });
    }

    let wallets: Vec<WalletMint> = ctx
        .wallets
        .iter()
        .map(|w| WalletMint {
            wallet: w.clone(),
            qty: qty_each,
        })
        .collect();

    let total: u64 = wallets.iter().map(|w| w.qty).sum();
    if total > remaining {
        return Err(PlanError::ExceedsSupply { total, remaining });
    }

    Ok(MintPlan {
        chain_id: target.candidate.chain_id,
        contract: target.candidate.contract,
        sale: sale.clone(),
        wallets,
        gas_mode: ctx.gas_mode,
    })
}

/// Format ringkasan untuk pesan Telegram ( §5 poin 3 ). Bebas rahasia
/// ( alamat disingkat via `WalletRef::short_address` ).
pub fn summarize(target: &VerifiedTarget, plan: &MintPlan) -> String {
    let sale: &SaleState = &plan.sale;
    let chain = chain_name(target.candidate.chain_id);
    let mut s = String::new();
    s.push_str(&format!(
        "Koleksi: {} ({chain})   Kontrak: {} {}\n",
        target.candidate.platform.as_str(),
        short_addr(&format!("{:?}", target.candidate.contract)),
        if target.is_proxy { "[PROXY]" } else { "[OK]" },
    ));
    let stage = match sale.sale_active {
        Some(true) => "Sale aktif",
        Some(false) => "Sale belum aktif",
        None => "Status sale tidak diketahui",
    };
    s.push_str(&format!("Stage: {stage}\n"));
    s.push_str(&format!(
        "Harga: {}   Limit: {}/wallet   Supply: {}\n",
        eth_str(sale.price),
        sale.max_per_wallet.map(|m| m.to::<u64>()).unwrap_or(0),
        supply_str(sale),
    ));
    let wallets: Vec<String> = plan
        .wallets
        .iter()
        .map(|w| format!("{}({})", w.wallet.short_address(), w.qty))
        .collect();
    s.push_str(&format!("Wallet: {}\n", wallets.join(", ")));
    s.push_str(&format!(
        "Gas: {:?}   Risiko: {}",
        plan.gas_mode,
        target.risk.as_str()
    ));
    if !target.flags.is_empty() {
        s.push_str("\nCatatan: ");
        s.push_str(&target.flags.join("; "));
    }
    s
}

fn chain_name(chain_id: u64) -> &'static str {
    match chain_id {
        1 => "Ethereum",
        8453 => "Base",
        42161 => "Arbitrum",
        10 => "Optimism",
        11_155_111 => "Sepolia",
        31_337 => "Anvil",
        _ => "?",
    }
}

fn short_addr(addr: &str) -> String {
    if addr.len() <= 10 {
        addr.to_string()
    } else {
        format!("{}..{}", &addr[..6], &addr[addr.len() - 2..])
    }
}

fn eth_str(wei: U256) -> String {
    // Wei -> ETH string dengan 4 desimal.
    let eth = wei_to_f64(wei);
    format!("{:.4} ETH", eth)
}

fn wei_to_f64(wei: U256) -> f64 {
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

fn supply_str(sale: &SaleState) -> String {
    match (sale.total_supply, sale.max_supply) {
        (Some(t), Some(m)) => format!("{}/{}", t, m),
        (Some(t), None) => format!("{}/?", t),
        _ => "?".into(),
    }
}

impl RiskLevel {
    /// Pesan peringatan untuk link dengan risiko tinggi ( §5 poin 4 ).
    pub fn warning(&self) -> Option<String> {
        match self {
            Self::High => Some(
                "Risiko TINGGI terdeteksi. Link adalah vektor umum wallet drainer. \
                 Pastikan Anda mengenali koleksi ini sebelum konfirmasi."
                    .into(),
            ),
            Self::Critical => Some(
                "Risiko KRITIS: kontrak/domain mencurigakan. Tidak disarankan dilanjutkan.".into(),
            ),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::{MintCandidate, Platform};
    use alloy::primitives::Address;
    use nft_mint::sale::SaleState;

    fn candidate() -> MintCandidate {
        MintCandidate {
            url: "https://opensea.io/drop/x".into(),
            domain: "opensea.io".into(),
            contract: Address::from_slice(&[0x22; 20]),
            chain_id: 8453,
            platform: Platform::OpenSea,
        }
    }

    fn sale() -> SaleState {
        SaleState {
            mint_signature: "mint(uint256)".into(),
            mint_selector: nft_mint::sale::selector("mint(uint256)"),
            price: U256::from(10_000_000_000_000_000u64), // 0.01 ETH
            max_per_wallet: Some(U256::from(3)),
            max_supply: Some(U256::from(10_000)),
            total_supply: Some(U256::from(4_210)),
            sale_active: Some(true),
        }
    }

    fn target(sale: SaleState) -> VerifiedTarget {
        VerifiedTarget {
            candidate: candidate(),
            sale,
            is_proxy: false,
            implementation: None,
            risk: RiskLevel::Low,
            flags: vec![],
        }
    }

    fn ctx(n: usize) -> UserCtx {
        let wallets = (0..n)
            .map(|i| {
                WalletRef::new(
                    format!("burner-{i}"),
                    format!("Burner {i}"),
                    format!("0x{}", "11".repeat(20)),
                )
            })
            .collect();
        UserCtx {
            wallets,
            gas_mode: GasMode::Aggressive,
        }
    }

    #[test]
    fn build_plan_qty_per_wallet_max() {
        let t = target(sale());
        let plan = build_mint_plan(&t, &ctx(2)).unwrap();
        assert_eq!(plan.wallets.len(), 2);
        assert_eq!(plan.wallets[0].qty, 3, "max_per_wallet 3");
        assert_eq!(plan.chain_id, 8453);
        assert_eq!(plan.contract, t.candidate.contract);
    }

    #[test]
    fn build_plan_qty_dipotong_sisa_supply() {
        let mut s = sale();
        s.max_supply = Some(U256::from(5));
        s.total_supply = Some(U256::from(4));
        // sisa 1, 2 wallet -> qty_each = 0 -> error ExceedsSupply.
        let t = target(s);
        let err = build_mint_plan(&t, &ctx(2)).unwrap_err();
        assert!(matches!(err, PlanError::ExceedsSupply { .. }));
    }

    #[test]
    fn build_plan_tanpa_max_per_wallet_default_1() {
        let mut s = sale();
        s.max_per_wallet = None;
        let t = target(s);
        let plan = build_mint_plan(&t, &ctx(2)).unwrap();
        assert_eq!(plan.wallets[0].qty, 1);
    }

    #[test]
    fn ringkasan_memuat_info_poin_3() {
        let t = target(sale());
        let plan = build_mint_plan(&t, &ctx(2)).unwrap();
        let s = summarize(&t, &plan);
        assert!(s.contains("Base"));
        assert!(s.contains("0.0100 ETH"));
        assert!(s.contains("3/wallet"));
        assert!(s.contains("4210/10000"));
        assert!(s.contains("RENDAH"));
        assert!(s.contains("Sale aktif"));
        // tidak ada alamat panjang ( disingkat ).
        assert!(!s.contains("0x2222222222222222222222222222222222222222"));
    }

    #[test]
    fn warning_hanya_high_dan_critical() {
        assert!(RiskLevel::Low.warning().is_none());
        assert!(RiskLevel::Medium.warning().is_none());
        assert!(RiskLevel::High.warning().is_some());
        assert!(RiskLevel::Critical.warning().is_some());
    }
}
