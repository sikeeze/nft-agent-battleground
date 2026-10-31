//! `compile_exec_plan` ( blueprint §8.3, §7 "Multi-wallet paralel", §9 Fase 5 ).
//!
//! Menyusun **tx varian pre-signed** per wallet: tiap wallet mendapat 3 tier
//! gas ( normal / aggressive / max-cap, §7 "tx pre-signed untuk 3 tingkat
//! gas" ). Tx dipilih berdasarkan kondisi jaringan saat Tsend — semua varian
//! sudah siap sehingga hot path tidak menyentuh assembler.
//!
//! Aturan keras #2: **tidak ada jalur sign yang lewati Policy Engine**.
//! `compile_exec_plan` TIDAK menandatangani apa pun — ia hanya menyusun
//! `TxRequest` mentah. Penandatanganan hanya terjadi lewat `PolicyEngine::
//! policy_check` → `ApprovedTx` → `WalletSigner::sign_approved`.
//!
//! Alur lengkap ( §8.3 ):
//! ```text
//! MintPlan → compile_exec_plan → ExecPlan{ TxRequest[] }
//! → reserve_nonces → policy_check per tx → sign → SignedTx[]
//! ```
//!
//! ponytail: `ExecPlan` tidak menyimpan private key. Signature hanya
//! dihasilkan di `exec.rs` saat policy sudah Approve.

use alloy::primitives::{Address, U256};
use thiserror::Error;

use nft_chain::{GasEstimate, GasMode, NonceManager};
use nft_core::wallet::WalletRef;

use crate::sale::SaleState;

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("qty {qty} melebihi max_per_wallet {max} ( §5 limit per wallet )")]
    ExceedsPerWallet { qty: u64, max: u64 },

    #[error("qty {qty} melebihi sisa supply {remaining}")]
    ExceedsSupply { qty: u64, remaining: u64 },

    #[error("sale tidak aktif ( saleActive() = false )")]
    SaleNotActive,

    #[error("total qty {total} melebihi sisa supply {remaining}")]
    TotalExceedsSupply { total: u64, remaining: u64 },
}

/// Rencana mint untuk satu wallet ( sebelum penandatanganan ).
#[derive(Debug, Clone)]
pub struct WalletMint {
    pub wallet: WalletRef,
    pub qty: u64,
}

/// Rencana mint lengkap ( §5 "MintPlan" ). Dibuat oleh "link resolver" Fase 6
/// atau manual lewat Telegram; di sini sudah berupa input `compile_exec_plan`.
#[derive(Debug, Clone)]
pub struct MintPlan {
    pub chain_id: u64,
    pub contract: Address,
    pub sale: SaleState,
    pub wallets: Vec<WalletMint>,
    pub gas_mode: GasMode,
}

/// Satu tx mentah per ( wallet, tier gas ) — **belum ditandatangani**.
#[derive(Debug, Clone)]
pub struct TxRequest {
    pub wallet: WalletRef,
    /// Tier gas varian ini ( §7 multi-tier ).
    pub gas_mode: GasMode,
    /// Calldata mint ( selector + argumen qty ).
    pub data: Vec<u8>,
    /// Nilai tx = price x qty ( wei ).
    pub value: U256,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub nonce: u64,
}

impl TxRequest {
    /// Idempotency key per ( task, wallet, nonce ) — §6.2 "Dobel mint tak
    /// sengaja". `task_id` dari Task yang memunculkan rencana ini.
    pub fn idempotency_key(&self, task_id: &str) -> String {
        format!("{task_id}:{}:{}", self.wallet.id, self.nonce)
    }
}

/// Hasil `compile_exec_plan`: tx mentah siap untuk `reserve_nonces` +
/// `policy_check` + sign.
#[derive(Debug, Clone)]
pub struct ExecPlan {
    pub chain_id: u64,
    pub contract: Address,
    pub txs: Vec<TxRequest>,
}

impl ExecPlan {
    /// Tier gas yang dipakai ( §7 "dipilih berdasarkan kondisi jaringan saat
    /// Tsend" ). `Normal` / `Aggressive` / `MaxCap` — MaxCap = cap policy.
    pub fn tier_modes() -> [GasMode; 3] {
        [GasMode::Normal, GasMode::Fast, GasMode::Aggressive]
    }
}

/// Estimasi gas varian tier ( §7 ). Tier dibuat dari `GasEstimate` oracle
/// (`Normal`/`Fast`/`Aggressive`) — `Aggressive` = persentil p90, masih dibatasi
/// `max_gas_gwei` policy ( lihat `GasOracle::estimate` ).
#[derive(Debug, Clone, Copy)]
pub struct TierGas {
    pub mode: GasMode,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

/// Susun `ExecPlan` dari `MintPlan` + estimasi gas per tier ( §8.3 ).
///
/// **Tidak menandatangani** — ini hanya assembler. Nonce di-reserve di sini
/// via `NonceManager` ( §8.3 `reserve_nonces` ) supaya tiap tx varian
/// mendapat nonce yang sudah dipetakan; nonce sama dipakai untuk semua tier
/// wallet yang sama ( varian = pilihan gas, bukan tx berbeda ).
///
/// Validasi ( §5, §6.1 ):
/// - qty per wallet <= `max_per_wallet`
/// - total qty <= sisa supply ( `remaining()` )
/// - sale aktif ( bila `sale_active` terbaca )
pub async fn compile_exec_plan(
    plan: &MintPlan,
    nonces: &NonceManager,
    tiers: &[TierGas; 3],
    gas_limit: u64,
) -> Result<ExecPlan, PlanError> {
    // 1. validasi limit per wallet ( §5 "Limit per wallet dihormati" ).
    for w in &plan.wallets {
        if let Some(max) = plan.sale.max_per_wallet {
            if U256::from(w.qty) > max {
                return Err(PlanError::ExceedsPerWallet {
                    qty: w.qty,
                    max: max.to::<u64>(),
                });
            }
        }
    }

    // 2. validasi sisa supply.
    if let Some(remaining) = plan.sale.remaining() {
        let total: u64 = plan.wallets.iter().map(|w| w.qty).sum();
        if U256::from(total) > remaining {
            return Err(PlanError::ExceedsSupply {
                qty: total,
                remaining: remaining.to::<u64>(),
            });
        }
    }

    // 3. sale aktif?
    if let Some(false) = plan.sale.sale_active {
        return Err(PlanError::SaleNotActive);
    }

    // 4. susun tx varian: 1 nonce per wallet, 3 tier gas per nonce.
    let mut txs = Vec::new();
    for w in &plan.wallets {
        let reservation = nonces
            .reserve(w.wallet.address.parse().expect("WalletRef address valid"))
            .await
            .expect("reservasi nonce tidak gagal ( monoton naik )");

        let value = plan.sale.price.saturating_mul(U256::from(w.qty));
        let data = build_mint_calldata(&plan.sale, w.qty);

        for tier in tiers {
            txs.push(TxRequest {
                wallet: w.wallet.clone(),
                gas_mode: tier.mode,
                data: data.clone(),
                value,
                gas_limit,
                max_fee_per_gas: tier.max_fee_per_gas,
                max_priority_fee_per_gas: tier.max_priority_fee_per_gas,
                nonce: reservation.nonce,
            });
        }
    }

    Ok(ExecPlan {
        chain_id: plan.chain_id,
        contract: plan.contract,
        txs,
    })
}

/// Bangun calldata mint sesuai signature yang terdeteksi ( §1 F1 ). Untuk
/// signature yang arity-nya > 1, argumen setelah qty diisi 0 ( kontrak uji
/// Fase 5 memakai `mint(uint256)` ).
pub fn build_mint_calldata(sale: &SaleState, qty: u64) -> Vec<u8> {
    let mut data = sale.mint_selector.to_vec();
    // argumen pertama = qty ( uint256 ).
    data.extend_from_slice(&U256::from(qty).to_be_bytes::<32>());
    // signature arity > 1 → argumen tersisa diisi 0 (Address zero / uint 0).
    let arity = crate::sale::arg_count_pub(&sale.mint_signature);
    for _ in 1..arity {
        data.extend_from_slice(&[0u8; 32]);
    }
    data
}

/// Estimasi 3 tier dari `GasOracle` ( §7 ). Tiap tier memakai persentil yang
/// berbeda; `Aggressive` ( p90 ) adalah tier tercepat yang masih dibatasi
/// `max_gas_gwei` policy.
pub async fn estimate_tiers(
    oracle: &nft_chain::GasOracle,
    max_gas_gwei: u64,
) -> Result<[TierGas; 3], nft_chain::GasError> {
    let mut out = Vec::with_capacity(3);
    for mode in [GasMode::Normal, GasMode::Fast, GasMode::Aggressive] {
        let est: GasEstimate = oracle.estimate(mode, max_gas_gwei).await?;
        out.push(TierGas {
            mode,
            max_fee_per_gas: est.max_fee_per_gas,
            max_priority_fee_per_gas: est.priority_fee,
        });
    }
    Ok([out[0], out[1], out[2]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;
    use nft_core::wallet::WalletRef;

    fn sale(price: U256, max_per_wallet: Option<U256>, active: Option<bool>) -> SaleState {
        SaleState {
            mint_signature: "mint(uint256)".into(),
            mint_selector: crate::sale::selector("mint(uint256)"),
            price,
            max_per_wallet,
            max_supply: Some(U256::from(100)),
            total_supply: Some(U256::from(10)),
            sale_active: active,
        }
    }

    fn wallet(id: &str, qty: u64) -> WalletMint {
        WalletMint {
            wallet: WalletRef::new(id, id, test_address()),
            qty,
        }
    }

    /// Alamat hex valid 42 char ( "0x" + 40 hex ) untuk test.
    fn test_address() -> String {
        format!("0x{}", "11".repeat(20))
    }

    fn plan(sale: SaleState) -> MintPlan {
        MintPlan {
            chain_id: 8453,
            contract: Address::from_slice(&[0x22; 20]),
            sale,
            wallets: vec![wallet("burner-1", 2)],
            gas_mode: GasMode::Aggressive,
        }
    }

    fn tiers() -> [TierGas; 3] {
        [
            TierGas {
                mode: GasMode::Normal,
                max_fee_per_gas: 10,
                max_priority_fee_per_gas: 1,
            },
            TierGas {
                mode: GasMode::Fast,
                max_fee_per_gas: 20,
                max_priority_fee_per_gas: 2,
            },
            TierGas {
                mode: GasMode::Aggressive,
                max_fee_per_gas: 30,
                max_priority_fee_per_gas: 3,
            },
        ]
    }

    #[tokio::test]
    async fn compile_tiga_tier_per_wallet() {
        let p = plan(sale(U256::from(1), Some(U256::from(3)), Some(true)));
        let nm = NonceManager::new();
        let exec = compile_exec_plan(&p, &nm, &tiers(), 200_000).await.unwrap();
        assert_eq!(exec.txs.len(), 3, "1 wallet x 3 tier");
        assert_eq!(exec.txs[0].nonce, 0);
        // semua tier memakai nonce yang sama ( varian gas, bukan tx baru ).
        assert_eq!(exec.txs[0].nonce, exec.txs[1].nonce);
        assert_eq!(exec.txs[1].nonce, exec.txs[2].nonce);
        assert_eq!(exec.txs[0].value, U256::from(2), "price 1 x qty 2");
    }

    #[tokio::test]
    async fn qty_melebihi_max_per_wallet_ditolak() {
        let p = plan(sale(U256::from(1), Some(U256::from(1)), Some(true)));
        let nm = NonceManager::new();
        let err = compile_exec_plan(&p, &nm, &tiers(), 200_000)
            .await
            .unwrap_err();
        assert!(matches!(err, PlanError::ExceedsPerWallet { .. }));
    }

    #[tokio::test]
    async fn total_melebihi_sisa_supply_ditolak() {
        let p = MintPlan {
            chain_id: 8453,
            contract: Address::from_slice(&[0x22; 20]),
            sale: sale(U256::from(1), None, Some(true)),
            wallets: vec![wallet("a", 50), wallet("b", 50)],
            gas_mode: GasMode::Aggressive,
        };
        let nm = NonceManager::new();
        let err = compile_exec_plan(&p, &nm, &tiers(), 200_000)
            .await
            .unwrap_err();
        assert!(matches!(err, PlanError::ExceedsSupply { .. }));
    }

    #[tokio::test]
    async fn sale_tidak_aktif_ditolak() {
        let p = plan(sale(U256::from(1), None, Some(false)));
        let nm = NonceManager::new();
        let err = compile_exec_plan(&p, &nm, &tiers(), 200_000)
            .await
            .unwrap_err();
        assert!(matches!(err, PlanError::SaleNotActive));
    }

    #[tokio::test]
    async fn idempotency_key_per_task_wallet_nonce() {
        let p = plan(sale(U256::from(1), None, Some(true)));
        let nm = NonceManager::new();
        let exec = compile_exec_plan(&p, &nm, &tiers(), 200_000).await.unwrap();
        let key = exec.txs[0].idempotency_key("task-1");
        assert_eq!(key, "task-1:burner-1:0");
    }

    #[test]
    fn calldata_mint_uint256() {
        let s = sale(U256::from(1), None, Some(true));
        let data = build_mint_calldata(&s, 3);
        // selector( mint(uint256) ) + uint256(3)
        assert_eq!(data.len(), 4 + 32);
        assert_eq!(&data[..4], &crate::sale::selector("mint(uint256)"));
        assert_eq!(U256::from_be_slice(&data[4..]), U256::from(3));
    }

    #[test]
    fn calldata_arity_dua_argumen_kedua_nol() {
        let mut s = sale(U256::from(1), None, Some(true));
        s.mint_signature = "mint(uint256,uint256)".into();
        s.mint_selector = crate::sale::selector("mint(uint256,uint256)");
        let data = build_mint_calldata(&s, 1);
        assert_eq!(data.len(), 4 + 64, "selector + 2 argumen");
        assert_eq!(U256::from_be_slice(&data[36..]), U256::ZERO);
    }
}
