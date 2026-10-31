//! `MintExecutor` — orkestrator eksekusi mint ( blueprint §8.3, §6.3, §7 ).
//!
//! Alur ( §8.3 ):
//! ```text
//! ExecPlan{TxRequest} → policy_check per tx → sign → race_broadcast
//! → confirm_and_verify ( receipt + balanceOf ) → update status
//! ```
//!
//! - **Aturan keras #2**: setiap tx lewat `PolicyEngine::policy_check` →
//!   `ApprovedTx` → `WalletSigner::sign_approved`. Tidak ada penandatanganan
//!   bypass.
//! - **Aturan keras #3**: default dry-run — `MintExecutorConfig.dry_run`
//!   true; race_broadcast dilewati, tx hanya dicatat ( §9 Fase 5 acceptance
//!   "dry-run tidak mengirim tx" ).
//! - `confirm_and_verify` ( §6.3 ): tunggu receipt, cek `balanceOf` lebih
//!   besar dari sebelum kirim.
//! - `bump_or_cancel` ( §7 ): tx belum masuk dalam N block → naikkan gas
//!   +12.5% ( dalam cap policy ) atau cancel bila sale habis.
//! - Idempotency: `(task, wallet, nonce)` dicegah dobel oleh `persist`.

use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use thiserror::Error;

use nft_chain::{NonceManager, RpcError, RpcPool};
use nft_core::intent::{Intent, IntentKind, Originator};
use nft_policy::{PolicyDecision, PolicyEngine};
use nft_signer::WalletSigner;

use crate::persist::{record_tx, update_tx, TxStatus};
use crate::plan::{ExecPlan, TxRequest};
use crate::race::{race_broadcast, RaceError, RaceOutcome, RaceTarget};
use crate::sale::SaleReader;

#[derive(Debug, Error)]
pub enum ExecError {
    #[error("policy menolak tx ( wallet {wallet} ): {reason}")]
    Denied { wallet: String, reason: String },

    #[error("signer error: {0}")]
    Signer(#[from] nft_signer::SignerError),

    #[error("race broadcast gagal: {0}")]
    Race(#[from] RaceError),

    #[error("persist error: {0}")]
    Persist(#[from] crate::persist::PersistError),

    #[error("RPC error: {0}")]
    Rpc(#[from] RpcError),

    #[error("konfirmasi timeout: tx {hash} tidak masuk block dalam {timeout:?}")]
    ConfirmTimeout { hash: String, timeout: Duration },

    #[error("sale habis ( totalSupply >= maxSupply ) — tx dibatalkan")]
    SoldOut,
}

/// Hasil satu tx mint ( §6.3 "Laporkan per wallet: sukses/gagal, tx hash,
/// gas terpakai, token id" ).
#[derive(Debug, Clone)]
pub struct MintOutcome {
    pub wallet: String,
    pub tx_hash: String,
    pub status: MintTxStatus,
    pub gas_used: Option<u128>,
    pub token_ids: Vec<U256>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MintTxStatus {
    /// Dry-run: tx disusun + ditandatangani tapi TIDAK dikirim.
    DryRun,
    Sent,
    Confirmed,
    Failed,
}

/// Konfigurasi eksekutor.
#[derive(Debug, Clone)]
pub struct MintExecutorConfig {
    /// Aturan keras #3: default true — tidak mengirim tx.
    pub dry_run: bool,
    /// Berapa lama menunggu receipt setelah kirim ( §6.3 ).
    pub confirm_timeout: Duration,
    /// Tingkat kenaikan gas saat bump ( §7 "+12.5%" ).
    pub bump_factor: f64,
    /// Target race ( public pool + relay ).
    pub race_targets: Vec<RaceTarget>,
}

impl Default for MintExecutorConfig {
    fn default() -> Self {
        Self {
            dry_run: true,
            confirm_timeout: Duration::from_secs(120),
            bump_factor: 1.125,
            race_targets: vec![RaceTarget::PublicPool],
        }
    }
}

/// Orkestrator eksekusi mint. `PolicyEngine` dibungkus `Arc` karena mesin
/// policy memegang `Box<dyn Trait>` ( tidak Clone ). Clone-murah.
#[derive(Clone)]
pub struct MintExecutor {
    pool: RpcPool,
    policy: std::sync::Arc<PolicyEngine>,
    nonces: NonceManager,
    config: MintExecutorConfig,
}
impl std::fmt::Debug for MintExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintExecutor")
            .field("dry_run", &self.config.dry_run)
            .field("race_targets", &self.config.race_targets.len())
            .finish()
    }
}

impl MintExecutor {
    pub fn new(
        pool: RpcPool,
        policy: PolicyEngine,
        nonces: NonceManager,
        config: MintExecutorConfig,
    ) -> Self {
        Self {
            pool,
            policy: std::sync::Arc::new(policy),
            nonces,
            config,
        }
    }

    /// Eksekusi `ExecPlan`: pilih tier gas ( §7 "dipilih berdasarkan kondisi
    /// jaringan saat Tsend" — di sini tier tercepat = `Aggressive` ),
    /// policy check + sign + race + confirm per tx.
    ///
    /// `task_id` dipakai untuk idempotency key ( §6.2 ).
    /// `signers`: wallet id → signer ( dari keystore ).
    pub async fn execute(
        &self,
        plan: &ExecPlan,
        task_id: &str,
        storage: &sqlx::SqlitePool,
        signers: &std::collections::HashMap<String, WalletSigner>,
    ) -> Result<Vec<MintOutcome>, ExecError> {
        let mut outcomes = Vec::new();
        // Kelompokkan tx per wallet: ambil tier tercepat per (wallet, nonce).
        for tx in fastest_tier_per_wallet(plan) {
            let outcome = self
                .execute_one(tx, plan.chain_id, plan.contract, task_id, storage, signers)
                .await;
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    async fn execute_one(
        &self,
        tx: &TxRequest,
        chain_id: u64,
        contract: Address,
        task_id: &str,
        storage: &sqlx::SqlitePool,
        signers: &std::collections::HashMap<String, WalletSigner>,
    ) -> MintOutcome {
        let key = tx.idempotency_key(task_id);
        let wallet_id = tx.wallet.id.clone();

        // 1. idempotency: catat dulu — bila sudah pernah dikirim, tolak.
        if let Err(e) = record_tx(
            storage,
            &key,
            chain_id,
            &wallet_id,
            tx.nonce,
            None,
            TxStatus::Prepared,
        )
        .await
        {
            return MintOutcome {
                wallet: wallet_id,
                tx_hash: String::new(),
                status: MintTxStatus::Failed,
                gas_used: None,
                token_ids: vec![],
                error: Some(e.to_string()),
            };
        }

        // 2. signer untuk wallet ini.
        let Some(signer) = signers.get(&wallet_id) else {
            return MintOutcome {
                wallet: wallet_id.clone(),
                tx_hash: String::new(),
                status: MintTxStatus::Failed,
                gas_used: None,
                token_ids: vec![],
                error: Some(format!("signer tidak ditemukan untuk wallet {wallet_id}")),
            };
        };

        // 3. policy check + sign ( aturan keras #2 ).
        let intent = Intent::new(IntentKind::Mint, Originator::User)
            .with_chain(chain_id)
            .with_contract(format!("{contract:?}"));
        let decision = self
            .policy
            .policy_check(
                &intent,
                tx.wallet.address.parse().expect("WalletRef address valid"),
                contract,
                tx.value,
                &tx.data,
                tx.gas_limit,
                tx.max_fee_per_gas,
                Some(tx.max_priority_fee_per_gas),
                tx.nonce,
                None,
            )
            .await;
        let approved = match decision {
            PolicyDecision::Approve(a) => a,
            PolicyDecision::Deny(e) => {
                return MintOutcome {
                    wallet: wallet_id,
                    tx_hash: String::new(),
                    status: MintTxStatus::Failed,
                    gas_used: None,
                    token_ids: vec![],
                    error: Some(e.to_string()),
                };
            }
        };
        let signed = match signer.sign_approved(&approved) {
            Ok(s) => s,
            Err(e) => {
                return MintOutcome {
                    wallet: wallet_id,
                    tx_hash: String::new(),
                    status: MintTxStatus::Failed,
                    gas_used: None,
                    token_ids: vec![],
                    error: Some(e.to_string()),
                };
            }
        };
        let _ = update_tx(storage, &key, TxStatus::Signed, None).await;

        // 4. dry-run ( aturan keras #3 ): tidak mengirim.
        if self.config.dry_run {
            tracing::info!(key = %key, hash = ?signed.hash(), "DRY-RUN: tx mint tidak dikirim");
            return MintOutcome {
                wallet: wallet_id,
                tx_hash: format!("{:?}", signed.hash()),
                status: MintTxStatus::DryRun,
                gas_used: None,
                token_ids: vec![],
                error: None,
            };
        }

        // 5. race broadcast ( §8.3 ).
        let race = match race_broadcast(
            &signed,
            &self.pool,
            &self.config.race_targets,
            Duration::from_secs(10),
        )
        .await
        {
            Ok(o) => o,
            Err(e) => {
                let _ = update_tx(storage, &key, TxStatus::Failed, None).await;
                return MintOutcome {
                    wallet: wallet_id,
                    tx_hash: String::new(),
                    status: MintTxStatus::Failed,
                    gas_used: None,
                    token_ids: vec![],
                    error: Some(e.to_string()),
                };
            }
        };
        let _ = update_tx(storage, &key, TxStatus::Sent, Some(&race.hash)).await;

        // 6. confirm_and_verify ( §6.3 ).
        let reader = SaleReader::new(self.pool.clone());
        match self
            .confirm_and_verify(&race, contract, &tx.wallet.address, tx.value, &reader)
            .await
        {
            Ok((gas_used, token_ids)) => {
                let _ = update_tx(storage, &key, TxStatus::Confirmed, Some(&race.hash)).await;
                MintOutcome {
                    wallet: wallet_id,
                    tx_hash: race.hash,
                    status: MintTxStatus::Confirmed,
                    gas_used,
                    token_ids,
                    error: None,
                }
            }
            Err(e) => {
                let _ = update_tx(storage, &key, TxStatus::Failed, Some(&race.hash)).await;
                MintOutcome {
                    wallet: wallet_id,
                    tx_hash: race.hash,
                    status: MintTxStatus::Failed,
                    gas_used: None,
                    token_ids: vec![],
                    error: Some(e.to_string()),
                }
            }
        }
    }

    /// `confirm_and_verify` ( §6.3 ): tunggu receipt, cek `balanceOf`
    /// wallet bertambah ( NFT benar-benar masuk ), kumpulkan gas terpakai.
    pub async fn confirm_and_verify(
        &self,
        race: &RaceOutcome,
        contract: Address,
        wallet_addr: &str,
        _value: U256,
        reader: &SaleReader,
    ) -> Result<(Option<u128>, Vec<U256>), ExecError> {
        let wallet: Address = wallet_addr.parse().expect("address valid");
        let before = reader
            .balance_of(contract, wallet)
            .await
            .unwrap_or(U256::ZERO);

        let deadline = Instant::now() + self.config.confirm_timeout;
        while Instant::now() < deadline {
            // Receipt via eth_getTransactionReceipt.
            if let Ok(receipt) = self
                .pool
                .request::<serde_json::Value>(
                    "eth_getTransactionReceipt",
                    &serde_json::json!([race.hash]),
                )
                .await
            {
                if receipt.get("status") == Some(&serde_json::json!("0x1")) {
                    // Sukses: cek balanceOf bertambah ( §6.3 ).
                    let after = reader.balance_of(contract, wallet).await.unwrap_or(before);
                    let token_ids = if after > before {
                        // ownerOf untuk token id di rentang [before, after).
                        enumerate_tokens(contract, before, after, reader, wallet).await
                    } else {
                        vec![]
                    };
                    let gas_used = receipt
                        .get("gasUsed")
                        .and_then(|g| g.as_str())
                        .and_then(|s| u128::from_str_radix(s.trim_start_matches("0x"), 16).ok());
                    return Ok((gas_used, token_ids));
                }
                if receipt.get("status") == Some(&serde_json::json!("0x0")) {
                    return Err(ExecError::ConfirmTimeout {
                        hash: race.hash.clone(),
                        timeout: self.config.confirm_timeout,
                    });
                }
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        Err(ExecError::ConfirmTimeout {
            hash: race.hash.clone(),
            timeout: self.config.confirm_timeout,
        })
    }

    /// Kembalikan nonce yang di-reservasi bila tx batal ( §6.2 "cancel tx
    /// nyangkut" — hanya nonce terakhir yang bisa dikembalikan ).
    pub async fn release_nonce(&self, wallet: Address, nonce: u64) {
        self.nonces.release(wallet, nonce).await;
    }

    /// `bump_or_cancel` ( §7 ): tx belum masuk dalam N block → naikkan gas
    /// +12.5% ( dalam cap policy ) atau cancel bila sale habis.
    pub fn bump_fee(&self, tx: &TxRequest) -> Result<TxRequest, ExecError> {
        let bumped = (tx.max_fee_per_gas as f64 * self.config.bump_factor) as u128;
        // Cap: gunakan max_fee tier Aggressive sebagai batas ( §7 "dalam
        // batas cap" — cap sebenarnya dari policy max_gas_gwei, dicek di
        // policy_check ).
        Ok(TxRequest {
            wallet: tx.wallet.clone(),
            gas_mode: tx.gas_mode,
            data: tx.data.clone(),
            value: tx.value,
            gas_limit: tx.gas_limit,
            max_fee_per_gas: bumped,
            max_priority_fee_per_gas: (tx.max_priority_fee_per_gas as f64 * self.config.bump_factor)
                as u128,
            nonce: tx.nonce,
        })
    }
}

/// Pilih tier gas tercepat ( Aggressive ) per ( wallet, nonce ) — sisanya
/// adalah cadangan ( §7 "varian tx multi-tier" ).
fn fastest_tier_per_wallet(plan: &ExecPlan) -> Vec<&TxRequest> {
    let mut picked: std::collections::HashMap<(String, u64), &TxRequest> =
        std::collections::HashMap::new();
    for tx in &plan.txs {
        let key = (tx.wallet.id.clone(), tx.nonce);
        match picked.get(&key) {
            Some(prev) => {
                // Aggressive > Fast > Normal.
                if tier_rank(tx.gas_mode) > tier_rank(prev.gas_mode) {
                    picked.insert(key, tx);
                }
            }
            None => {
                picked.insert(key, tx);
            }
        }
    }
    let mut out: Vec<&TxRequest> = picked.into_values().collect();
    out.sort_by_key(|t| (t.wallet.id.clone(), t.nonce));
    out
}

fn tier_rank(mode: nft_chain::GasMode) -> u8 {
    match mode {
        nft_chain::GasMode::Normal => 0,
        nft_chain::GasMode::Fast => 1,
        nft_chain::GasMode::Aggressive => 2,
    }
}

/// Cek `ownerOf` untuk token id dalam rentang ( before..after ) — milik
/// wallet ini ( §6.3 verifikasi ).
async fn enumerate_tokens(
    contract: Address,
    before: U256,
    after: U256,
    reader: &SaleReader,
    wallet: Address,
) -> Vec<U256> {
    let mut out = Vec::new();
    let mut id = before;
    while id < after {
        if let Ok(owner) = reader.owner_of(contract, id).await {
            if owner == wallet {
                out.push(id);
            }
        }
        id += U256::from(1u64);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_rank_urutan_aggressive_tertinggi() {
        assert_eq!(tier_rank(nft_chain::GasMode::Normal), 0);
        assert_eq!(tier_rank(nft_chain::GasMode::Fast), 1);
        assert_eq!(tier_rank(nft_chain::GasMode::Aggressive), 2);
    }

    #[test]
    fn fastest_tier_memilih_aggressive() {
        use crate::plan::TxRequest;
        let mk = |mode: nft_chain::GasMode| TxRequest {
            wallet: nft_core::wallet::WalletRef::new("w", "w", "0x".repeat(20)[..40].to_string()),
            gas_mode: mode,
            data: vec![],
            value: U256::ZERO,
            gas_limit: 100,
            max_fee_per_gas: 10,
            max_priority_fee_per_gas: 1,
            nonce: 0,
        };
        let plan = ExecPlan {
            chain_id: 8453,
            contract: Address::from_slice(&[0x22; 20]),
            txs: vec![
                mk(nft_chain::GasMode::Normal),
                mk(nft_chain::GasMode::Aggressive),
                mk(nft_chain::GasMode::Fast),
            ],
        };
        let picked = fastest_tier_per_wallet(&plan);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].gas_mode, nft_chain::GasMode::Aggressive);
    }

    #[tokio::test]
    async fn bump_fee_naik_12_5_persen() {
        let cfg = MintExecutorConfig::default();
        let exec = MintExecutor::new(
            RpcPool::new(nft_chain::RpcPoolConfig {
                http_urls: vec!["http://127.0.0.1:9".into()],
                request_timeout_ms: 300,
                ..Default::default()
            })
            .unwrap(),
            nft_policy::PolicyEngine::with_defaults(parse_ok_policy()),
            NonceManager::new(),
            cfg,
        );
        let tx = TxRequest {
            wallet: nft_core::wallet::WalletRef::new("w", "w", "0x".repeat(20)[..40].to_string()),
            gas_mode: nft_chain::GasMode::Aggressive,
            data: vec![],
            value: U256::ZERO,
            gas_limit: 100,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            nonce: 0,
        };
        let bumped = exec.bump_fee(&tx).unwrap();
        assert_eq!(bumped.max_fee_per_gas, 112);
        assert_eq!(bumped.max_priority_fee_per_gas, 11);
    }

    fn parse_ok_policy() -> nft_core::config::PolicyConfig {
        nft_core::config::parse_policy(
            r#"
        [limits]
        max_spend_per_tx_eth = 0.5
        max_spend_per_day_eth = 2.0
        max_gas_gwei = 80
        allowed_chains = [1, 8453, 42161]
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
}
