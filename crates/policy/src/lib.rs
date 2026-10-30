//! `nft-policy` — Policy Engine (blueprint §F5, §3, §9 Fase 2).
//!
//! Aturan keras: **tidak ada jalur sign yang lewati Policy Engine**. Policy
//! engine adalah-satunya pembuat `ApprovedTx`. Output `policy_check` adalah
//! `PolicyDecision` — `Approve` membawa `ApprovedTx` siap-sign, `Deny` membawa
//! alasan untuk log audit.
//!
//! Pemeriksaan (berurutan, fail-fast):
//! 1. kill switch ( manual / auto rugi > `auto_trigger_on_loss_eth` ) — blokir semua.
//! 2. chain diizinkan? ( `limits.allowed_chains` )
//! 3. kontrak target di allowlist? ( `allowlist.contracts` )
//! 4. tx di atas `max_spend_per_tx_eth`? -> konfirmasi manusia atau tolak.
//! 5. `tx.gas_price`/`max_fee` di atas `max_gas_gwei`? -> tolak.
//! 6. `approve(max)` / `setApprovalForAll` ke kontrak asing -> tolak bila
//!    `block_unlimited_approvals`.
//! 7. simulator ( `eth_simulateV1`/`eth_call` ) reverts -> tolak.
//! 8. intent dengan originator LLM/Skill wajib lewat konfirmasi manusia bila
//!    `require_human_for` mencocoki jenis intent.
//!
//! **Catatan Fase 2**: state harian ( spend/day ) & persistence kill switch ke
//! SQLite dirancang lewat trait `PolicyState` — implementasi SQLite menyusul
//! di Fase 3 bersama RPC. Sekarang dipakai implementasi in-memory yang cukup
//! untuk pengujian acceptance.

use std::collections::HashSet;

use alloy::primitives::{Address, U256};
use async_trait::async_trait;
use nft_core::config::PolicyConfig;
use nft_core::intent::{Intent, IntentKind};
use signer::{ApprovedTx, SignerError};

// ---------------------------------------------------------------------------
// error / decision
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("kill switch aktif: {reason}")]
    KillSwitchActive { reason: String },

    #[error("chain {chain_id} tidak diizinkan ( yang diizinkan: {allowed} )")]
    ChainNotAllowed { chain_id: u64, allowed: String },

    #[error("kontrak {contract} tidak ada di allowlist")]
    ContractNotAllowlisted { contract: String },

    #[error("spend {value_eth} ETH melebihi batas per tx {max_eth} ETH")]
    SpendPerTxExceeded { value_eth: f64, max_eth: f64 },

    #[error("spend {value_eth} ETH melebihi batas harian {max_eth} ETH")]
    SpendPerDayExceeded { value_eth: f64, max_eth: f64 },

    #[error("gas {gas_gwei} gwei melebihi batas {max_gwei} gwei")]
    GasTooHigh { gas_gwei: u64, max_gwei: u64 },

    #[error(
        "approval tak terbatas ( approve(max) / setApprovalForAll ) ke kontrak {contract} ditolak"
    )]
    UnlimitedApprovalBlocked { contract: String },

    #[error("simulasi tx revert: {reason}")]
    SimulationReverted { reason: String },

    #[error("butuh konfirmasi manusia: {reason}")]
    NeedsHumanConfirmation { reason: String },

    #[error("data intent tidak valid: {0}")]
    InvalidIntent(String),

    #[error("kesalahan signer saat menyusun ApprovedTx: {0}")]
    Signer(#[from] SignerError),
}

/// Hasil pemeriksaan policy. `Approve` membawa `ApprovedTx` —-satunya jalan
/// menuju signer.
#[derive(Debug)]
pub enum PolicyDecision {
    Approve(ApprovedTx),
    Deny(PolicyError),
}

// ---------------------------------------------------------------------------
// state policy ( spend harian, kill switch persisten )
// ---------------------------------------------------------------------------

/// State policy. Implementasi SQLite ( persisten ) menyusul di Fase 3;
/// untuk Fase 2 digunakan implementasi in-memory.
#[async_trait]
pub trait PolicyState: Send + Sync {
    /// Total spend ( ETH ) hari ini untuk wallet ini.
    async fn spent_today(&self, wallet: Address) -> f64;

    /// Catat spend baru ( ETH ).
    async fn record_spend(&self, wallet: Address, eth: f64);

    /// Apakah kill switch aktif? ( termasuk persistensi setelah restart )
    async fn kill_switch_on(&self) -> Option<String>;

    /// Nyalakan kill switch ( manual atau auto ); state harus persisten.
    async fn trigger_kill_switch(&self, reason: String);

    /// Matikan kill switch ( hanya manual, dari Telegram `/stop` -> `/resume` ).
    async fn clear_kill_switch(&self);
}

/// Implementasi in-memory `PolicyState` ( cukup untuk acceptance Fase 2 ).
#[derive(Debug, Default)]
pub struct InMemoryPolicyState {
    inner: tokio::sync::Mutex<InMemoryPolicyStateInner>,
}

#[derive(Debug, Default)]
struct InMemoryPolicyStateInner {
    spent: std::collections::HashMap<Address, f64>,
    kill: Option<String>,
}

#[async_trait]
impl PolicyState for InMemoryPolicyState {
    async fn spent_today(&self, wallet: Address) -> f64 {
        self.inner
            .lock()
            .await
            .spent
            .get(&wallet)
            .copied()
            .unwrap_or(0.0)
    }

    async fn record_spend(&self, wallet: Address, eth: f64) {
        let mut g = self.inner.lock().await;
        let entry = g.spent.entry(wallet).or_insert(0.0);
        *entry += eth;
    }

    async fn kill_switch_on(&self) -> Option<String> {
        self.inner.lock().await.kill.clone()
    }

    async fn trigger_kill_switch(&self, reason: String) {
        self.inner.lock().await.kill = Some(reason);
    }

    async fn clear_kill_switch(&self) {
        self.inner.lock().await.kill = None;
    }
}

// ---------------------------------------------------------------------------
// hook konfirmasi manusia & simulator
// ---------------------------------------------------------------------------

/// Hook konfirmasi manusia ( blueprint §F5 ). Implementasi Telegram ( tombol
/// /approve//reject ) menyusul di Fase 4. Async supaya bisa menunggu jawaban
/// user tanpa memblokir hot path.
#[async_trait]
pub trait HumanConfirmation: Send + Sync {
    /// Tanyakan konfirmasi user untuk tx yang membutuhkannya.
    /// `true` = lanjutkan, `false` = tolak.
    async fn confirm(&self, tx: &ApprovedTx, reason: &str) -> bool;
}

/// Hook simulator ( blueprint §F5 ). Tx yang revert = ditolak.
/// Implementasi RPC ( `eth_call`/`eth_simulateV1` ) menyusul di Fase 3.
#[async_trait]
pub trait Simulator: Send + Sync {
    /// Jalankan tx dalam simulasi. `Ok(())` = success, `Err(reason)` = revert.
    async fn simulate(&self, tx: &ApprovedTx) -> Result<(), String>;
}

/// Implementasi no-op: simulasi selalu sukses ( dipakai saat RPC belum ada /
/// dry-run ). Tidak boleh dipakai di produksi tanpa override.
pub struct NoSimulator;

#[async_trait]
impl Simulator for NoSimulator {
    async fn simulate(&self, _tx: &ApprovedTx) -> Result<(), String> {
        Ok(())
    }
}

/// Konfirmasi yang selalu menolak ( default aman: tanpa hook = tidak ada tx
/// yang butuh konfirmasi manusia yang lolos otomatis ).
pub struct AlwaysDenyConfirmation;

#[async_trait]
impl HumanConfirmation for AlwaysDenyConfirmation {
    async fn confirm(&self, _tx: &ApprovedTx, _reason: &str) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// policy engine
// ---------------------------------------------------------------------------

pub struct PolicyEngine {
    config: PolicyConfig,
    state: Box<dyn PolicyState>,
    simulator: Box<dyn Simulator>,
    confirmation: Box<dyn HumanConfirmation>,
}

impl PolicyEngine {
    pub fn new(
        config: PolicyConfig,
        state: Box<dyn PolicyState>,
        simulator: Box<dyn Simulator>,
        confirmation: Box<dyn HumanConfirmation>,
    ) -> Self {
        Self {
            config,
            state,
            simulator,
            confirmation,
        }
    }

    /// Versi standar untuk Fase 2: state in-memory, tanpa simulator RPC,
    /// konfirmasi default menolak ( produksi mengganti ketiganya ).
    pub fn with_defaults(config: PolicyConfig) -> Self {
        Self::new(
            config,
            Box::new(InMemoryPolicyState::default()),
            Box::new(NoSimulator),
            Box::new(AlwaysDenyConfirmation),
        )
    }

    /// Pemicu kill switch manual ( dari `/stop` di Telegram ).
    pub async fn trigger_kill_switch(&self, reason: String) {
        self.state.trigger_kill_switch(reason).await;
    }

    /// Matikan kill switch ( dari `/resume` ).
    pub async fn clear_kill_switch(&self) {
        self.state.clear_kill_switch().await;
    }

    /// Apakah kill switch sedang aktif.
    pub async fn kill_switch_active(&self) -> Option<String> {
        self.state.kill_switch_on().await
    }

    // -------------------------------------------------------------------
    // policy_check — pintu gerbang utama
    // -------------------------------------------------------------------

    /// Jalankan seluruh pemeriksaan untuk sebuah intent + rincian tx.
    /// `value` dalam wei, `gas_price`/`max_fee` dalam wei, `data` calldata.
    /// ponytail: parameter polos dipakai karena ini signature pintu gerbang;
    /// bila Fase 3 menambah cek ( access list, blob ), naikkan ke
    /// struct `PolicyCheckRequest`. Jangan tambah argumen polos lagi.
    #[allow(clippy::too_many_arguments)]
    pub async fn policy_check(
        &self,
        intent: &Intent,
        from: Address,
        to: Address,
        value: U256,
        data: &[u8],
        gas_limit: u64,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: Option<u128>,
        nonce: u64,
        auto_kill_loss_eth: Option<f64>,
    ) -> PolicyDecision {
        // 1. kill switch ( manual atau auto rugi )
        if let Some(reason) = self.state.kill_switch_on().await {
            return PolicyDecision::Deny(PolicyError::KillSwitchActive { reason });
        }
        if let Some(loss) = auto_kill_loss_eth {
            if loss >= self.config.kill_switch.auto_trigger_on_loss_eth {
                let reason = format!("auto kill switch: rugi {loss} ETH >= batas");
                self.state.trigger_kill_switch(reason.clone()).await;
                return PolicyDecision::Deny(PolicyError::KillSwitchActive { reason });
            }
        }

        // 2. chain diizinkan
        let chain_id = match intent.chain_id {
            Some(c) => c,
            None => {
                return PolicyDecision::Deny(PolicyError::InvalidIntent(
                    "chain_id wajib diisi".into(),
                ))
            }
        };
        if !self.config.limits.allowed_chains.contains(&chain_id) {
            return PolicyDecision::Deny(PolicyError::ChainNotAllowed {
                chain_id,
                allowed: format!("{:?}", self.config.limits.allowed_chains),
            });
        }

        // 3. allowlist kontrak ( target tx maupun kontrak pada params )
        let contract = intent.contract.as_deref().unwrap_or_default();
        let allow: HashSet<&str> = self
            .config
            .allowlist
            .contracts
            .iter()
            .map(|s| s.as_str())
            .collect();
        if !allow.is_empty() && !allow.contains(contract) {
            return PolicyDecision::Deny(PolicyError::ContractNotAllowlisted {
                contract: contract.to_string(),
            });
        }

        // 4. spend per tx
        let value_eth = wei_to_eth(value);
        if value_eth > self.config.limits.max_spend_per_tx_eth {
            return PolicyDecision::Deny(PolicyError::SpendPerTxExceeded {
                value_eth,
                max_eth: self.config.limits.max_spend_per_tx_eth,
            });
        }

        // 5. spend harian
        let spent = self.state.spent_today(from).await;
        if spent + value_eth > self.config.limits.max_spend_per_day_eth {
            return PolicyDecision::Deny(PolicyError::SpendPerDayExceeded {
                value_eth: spent + value_eth,
                max_eth: self.config.limits.max_spend_per_day_eth,
            });
        }

        // 6. gas
        let gas_gwei = (max_fee_per_gas / 1_000_000_000) as u64;
        if gas_gwei > self.config.limits.max_gas_gwei {
            return PolicyDecision::Deny(PolicyError::GasTooHigh {
                gas_gwei,
                max_gwei: self.config.limits.max_gas_gwei,
            });
        }

        // 7. approval tak terbatas
        if self.config.allowlist.block_unlimited_approvals {
            if let Some(contract) = unlimited_approval_target(intent, data) {
                return PolicyDecision::Deny(PolicyError::UnlimitedApprovalBlocked { contract });
            }
        }

        // 8. jenis intent dari LLM/Skill wajib konfirmasi bila dicantumkan
        if self.needs_human(intent) {
            let approved = match ApprovedTx::new_checked(
                chain_id,
                from,
                to,
                value,
                data,
                gas_limit,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                nonce,
                intent.id.to_string(),
            ) {
                Ok(a) => a,
                Err(e) => return PolicyDecision::Deny(PolicyError::Signer(e)),
            };
            let reason = self.human_reason(intent);
            if !self.confirmation.confirm(&approved, &reason).await {
                return PolicyDecision::Deny(PolicyError::NeedsHumanConfirmation {
                    reason: "tidak dikonfirmasi oleh user".into(),
                });
            }
            return PolicyDecision::Approve(approved);
        }

        // 9. simulator
        let approved = match ApprovedTx::new_checked(
            chain_id,
            from,
            to,
            value,
            data,
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            nonce,
            intent.id.to_string(),
        ) {
            Ok(a) => a,
            Err(e) => return PolicyDecision::Deny(PolicyError::Signer(e)),
        };
        if let Err(reason) = self.simulator.simulate(&approved).await {
            return PolicyDecision::Deny(PolicyError::SimulationReverted { reason });
        }

        PolicyDecision::Approve(approved)
    }

    fn needs_human(&self, intent: &Intent) -> bool {
        // originator LLM/Skill: cakupan `require_human_for` ——jenis intent
        let kind = match intent.kind {
            IntentKind::Mint => "mint",
            IntentKind::Buy => "buy",
            IntentKind::Sell => "sell",
            IntentKind::List => "list",
            IntentKind::Bid => "bid",
            IntentKind::Cancel => "cancel",
            IntentKind::Transfer => "transfer",
            IntentKind::Sign => "sign",
        };
        let kind_hit = self
            .config
            .confirmation
            .require_human_for
            .iter()
            .any(|s| s == kind);
        let above = wei_to_eth(U256::ZERO) > self.config.confirmation.require_human_above_eth;
        kind_hit || above
    }

    fn human_reason(&self, intent: &Intent) -> String {
        format!(
            "intent {:?} ({}) dari {:?} membutuhkan konfirmasi",
            intent.kind, intent.id, intent.originator
        )
    }
}

fn wei_to_eth(wei: U256) -> f64 {
    // U256 -> f64 dengan pembagian presisi: 1 ETH = 1e18 wei
    let s = wei.to_string();
    let bytes = s.as_bytes();
    let eth: f64 = if bytes.len() <= 18 {
        let v: f64 = s.parse().unwrap_or(0.0);
        v / 1e18
    } else {
        let whole = &s[..bytes.len() - 18];
        let frac = &s[bytes.len() - 18..];
        let w: f64 = whole.parse().unwrap_or(0.0);
        let f: f64 = format!("0.{}", frac).parse().unwrap_or(0.0);
        w + f
    };
    eth
}

/// Deteksi `approve(max)` / `setApprovalForAll` ( approval tak terbatas ).
/// Memeriksa signature calldata ( selector 4 byte ) + nilai ( max = 2^256-1 ).
/// `block_unlimited_approvals` memblokir approval tak terbatas terlepas dari
/// status allowlist kontrak ( aturan §3: batasi exposure ).
fn unlimited_approval_target(intent: &Intent, data: &[u8]) -> Option<String> {
    // selector approve(address,uint256) = 0x095ea7b3
    // selector setApprovalForAll(address,bool) = 0xa22cb465
    const APPROVE: [u8; 4] = [0x09, 0x5e, 0xa7, 0xb3];
    const SET_APPROVAL_FOR_ALL: [u8; 4] = [0xa2, 0x2c, 0xb4, 0x65];

    if data.len() < 4 {
        return None;
    }
    let sel = &data[..4];
    let target = intent.contract.as_deref().unwrap_or_default().to_string();

    if sel == APPROVE {
        // approve spender( bytes 4..36 ) amount( bytes 36..68 )
        if data.len() >= 68 {
            let amount = U256::from_be_slice(&data[36..68]);
            if amount == U256::MAX {
                return Some(target);
            }
        }
    }
    if sel == SET_APPROVAL_FOR_ALL {
        // approved = true ( byte terakhir argumen kedua )
        if data.len() >= 68 && data[67] != 0 {
            return Some(target);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nft_core::config::parse_policy;
    use nft_core::intent::{Intent, IntentKind, Originator};

    const POLICY_OK: &str = r#"
        [limits]
        max_spend_per_tx_eth = 0.5
        max_spend_per_day_eth = 2.0
        max_gas_gwei = 80
        allowed_chains = [1, 8453, 42161]

        [allowlist]
        contracts = ["0x1111111111111111111111111111111111111111"]
        block_unlimited_approvals = true

        [confirmation]
        require_human_above_eth = 0.2
        require_human_for = ["withdraw", "new_contract"]

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

    fn config() -> PolicyConfig {
        parse_policy(POLICY_OK).unwrap()
    }

    fn intent(kind: IntentKind, chain: u64, contract: &str) -> Intent {
        Intent::new(kind, Originator::User)
            .with_chain(chain)
            .with_contract(contract)
    }

    async fn check(engine: &PolicyEngine, intent: &Intent, value: U256) -> PolicyDecision {
        engine
            .policy_check(
                intent,
                Address::ZERO,
                Address::ZERO,
                value,
                &[],
                210_000,
                30_000_000_000u128, // 30 gwei
                None,
                0,
                None,
            )
            .await
    }

    #[test]
    fn wei_to_eth_konversi() {
        assert!((wei_to_eth(U256::from(1e18 as u64)) - 1.0).abs() < 1e-12);
        assert!((wei_to_eth(U256::from(5e17 as u64)) - 0.5).abs() < 1e-12);
        assert_eq!(wei_to_eth(U256::ZERO), 0.0);
        // nilai besar: 1_000_000 ETH
        let big = U256::from(1_000_000u64) * U256::from(10u64).pow(U256::from(18u64));
        assert!((wei_to_eth(big) - 1_000_000.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn tx_diatas_limit_ditolak() {
        let engine = PolicyEngine::with_defaults(config());
        let it = intent(
            IntentKind::Transfer,
            1,
            "0x1111111111111111111111111111111111111111",
        );
        let value = U256::from(1e18 as u64); // 1 ETH > 0.5
        let dec = check(&engine, &it, value).await;
        match dec {
            PolicyDecision::Deny(PolicyError::SpendPerTxExceeded { .. }) => {}
            other => panic!("seharusnya ditolak: {other:?}"),
        }
    }

    #[tokio::test]
    async fn tx_dalam_limit_disetujui() {
        let engine = PolicyEngine::with_defaults(config());
        let it = intent(
            IntentKind::Transfer,
            1,
            "0x1111111111111111111111111111111111111111",
        );
        let value = U256::from(1e17 as u64); // 0.1 ETH
        match check(&engine, &it, value).await {
            PolicyDecision::Approve(_) => {}
            other => panic!("seharusnya disetujui: {other:?}"),
        }
    }

    #[tokio::test]
    async fn chain_tidak_diizinkan_ditolak() {
        let engine = PolicyEngine::with_defaults(config());
        let it = intent(
            IntentKind::Transfer,
            999,
            "0x1111111111111111111111111111111111111111",
        );
        match check(&engine, &it, U256::ZERO).await {
            PolicyDecision::Deny(PolicyError::ChainNotAllowed { .. }) => {}
            other => panic!("seharusnya chain ditolak: {other:?}"),
        }
    }

    #[tokio::test]
    async fn kontrak_diluar_allowlist_ditolak() {
        let engine = PolicyEngine::with_defaults(config());
        let it = intent(
            IntentKind::Transfer,
            1,
            "0x9999999999999999999999999999999999999999",
        );
        match check(&engine, &it, U256::ZERO).await {
            PolicyDecision::Deny(PolicyError::ContractNotAllowlisted { .. }) => {}
            other => panic!("seharusnya allowlist ditolak: {other:?}"),
        }
    }

    #[tokio::test]
    async fn gas_terlalu_tinggi_ditolak() {
        let engine = PolicyEngine::with_defaults(config());
        let it = intent(
            IntentKind::Transfer,
            1,
            "0x1111111111111111111111111111111111111111",
        );
        let dec = engine
            .policy_check(
                &it,
                Address::ZERO,
                Address::ZERO,
                U256::ZERO,
                &[],
                210_000,
                100_000_000_000u128, // 100 gwei > 80
                None,
                0,
                None,
            )
            .await;
        match dec {
            PolicyDecision::Deny(PolicyError::GasTooHigh { .. }) => {}
            other => panic!("seharusnya gas ditolak: {other:?}"),
        }
    }

    #[tokio::test]
    async fn kill_switch_manual_memblokir_semua() {
        let engine = PolicyEngine::with_defaults(config());
        engine.trigger_kill_switch("user /stop".to_string()).await;
        let it = intent(
            IntentKind::Transfer,
            1,
            "0x1111111111111111111111111111111111111111",
        );
        match check(&engine, &it, U256::from(1e17 as u64)).await {
            PolicyDecision::Deny(PolicyError::KillSwitchActive { .. }) => {}
            other => panic!("seharusnya kill switch: {other:?}"),
        }
        // setelah clear, tx bisa jalan lagi
        engine.clear_kill_switch().await;
        match check(&engine, &it, U256::from(1e17 as u64)).await {
            PolicyDecision::Approve(_) => {}
            other => panic!("seharusnya approve setelah clear: {other:?}"),
        }
    }

    #[tokio::test]
    async fn kill_switch_auto_saat_rugi_melebihi_batas() {
        let engine = PolicyEngine::with_defaults(config());
        let it = intent(
            IntentKind::Transfer,
            1,
            "0x1111111111111111111111111111111111111111",
        );
        // rugi 1.5 ETH >= auto_trigger_on_loss_eth = 1.0
        match check_with_loss(&engine, &it, 1.5).await {
            PolicyDecision::Deny(PolicyError::KillSwitchActive { reason }) => {
                assert!(reason.contains("auto"));
            }
            other => panic!("seharusnya auto kill: {other:?}"),
        }
        // kill switchtetap aktif untuk intent berikutnya
        match check(&engine, &it, U256::from(1e17 as u64)).await {
            PolicyDecision::Deny(PolicyError::KillSwitchActive { .. }) => {}
            other => panic!("kill switch harusnya persisten: {other:?}"),
        }
    }

    async fn check_with_loss(
        engine: &PolicyEngine,
        intent: &Intent,
        loss_eth: f64,
    ) -> PolicyDecision {
        engine
            .policy_check(
                intent,
                Address::ZERO,
                Address::ZERO,
                U256::from(1e17 as u64),
                &[],
                210_000,
                30_000_000_000u128,
                None,
                0,
                Some(loss_eth),
            )
            .await
    }

    #[tokio::test]
    async fn approve_max_ke_kontrak_allowlist_ditolak() {
        // kontrak ADA di allowlist,tetapi approval tak terbatas tetap ditolak
        // ( block_unlimited_approvals = true )
        let engine = PolicyEngine::with_defaults(config());
        let it = intent(
            IntentKind::Sign,
            1,
            "0x1111111111111111111111111111111111111111",
        );
        // calldata: approve selector + spender + amount = MAX
        let mut data = vec![0x09u8, 0x5e, 0xa7, 0xb3];
        data.extend_from_slice(&[0u8; 32]); // spender
        data.extend_from_slice(&[0xffu8; 32]); // amount = 2^256-1
        let dec = engine
            .policy_check(
                &it,
                Address::ZERO,
                Address::ZERO,
                U256::ZERO,
                &data,
                210_000,
                30_000_000_000u128,
                None,
                0,
                None,
            )
            .await;
        match dec {
            PolicyDecision::Deny(PolicyError::UnlimitedApprovalBlocked { .. }) => {}
            other => panic!("seharusnya unlimited approval ditolak: {other:?}"),
        }
    }

    #[tokio::test]
    #[allow(non_snake_case)]
    async fn setApprovalForAll_ke_kontrak_allowlist_ditolak() {
        let engine = PolicyEngine::with_defaults(config());
        let it = intent(
            IntentKind::Sign,
            1,
            "0x1111111111111111111111111111111111111111",
        );
        let mut data = vec![0xa2u8, 0x2c, 0xb4, 0x65];
        data.extend_from_slice(&[0u8; 32]); // operator
        data.extend_from_slice(&[1u8; 32]); // approved = true
        let dec = engine
            .policy_check(
                &it,
                Address::ZERO,
                Address::ZERO,
                U256::ZERO,
                &data,
                210_000,
                30_000_000_000u128,
                None,
                0,
                None,
            )
            .await;
        match dec {
            PolicyDecision::Deny(PolicyError::UnlimitedApprovalBlocked { .. }) => {}
            other => panic!("seharusnya setApprovalForAll ditolak: {other:?}"),
        }
    }

    #[tokio::test]
    async fn simulator_revert_ditolak() {
        struct RevertSim;
        #[async_trait]
        impl Simulator for RevertSim {
            async fn simulate(&self, _tx: &ApprovedTx) -> Result<(), String> {
                Err("execution reverted: insufficient balance".into())
            }
        }
        let engine = PolicyEngine::new(
            config(),
            Box::new(InMemoryPolicyState::default()),
            Box::new(RevertSim),
            Box::new(AlwaysDenyConfirmation),
        );
        let it = intent(
            IntentKind::Transfer,
            1,
            "0x1111111111111111111111111111111111111111",
        );
        match check(&engine, &it, U256::from(1e17 as u64)).await {
            PolicyDecision::Deny(PolicyError::SimulationReverted { reason }) => {
                assert!(reason.contains("reverted"));
            }
            other => panic!("seharusnya simulasi revert: {other:?}"),
        }
    }

    #[tokio::test]
    async fn intent_llm_butuh_konfirmasi_ditolak_bila_tidak_dikonfirmasi() {
        struct AlwaysConfirm;
        #[async_trait]
        impl HumanConfirmation for AlwaysConfirm {
            async fn confirm(&self, _tx: &ApprovedTx, _reason: &str) -> bool {
                true
            }
        }
        let engine = PolicyEngine::new(
            config(),
            Box::new(InMemoryPolicyState::default()),
            Box::new(NoSimulator),
            Box::new(AlwaysDenyConfirmation),
        );
        // intent "withdraw" ada di require_human_for
        let it = Intent::new(IntentKind::Transfer, Originator::Llm)
            .with_chain(1)
            .with_contract("0x1111111111111111111111111111111111111111")
            .with_params(serde_json::json!({"kind": "withdraw"}));
        // kind Transfer tidak ada di require_human_for ["withdraw","new_contract"]
        // tetapi params.kind = withdraw —— indikasi withdraw. Engine cek intent.kind.
        let dec = check(&engine, &it, U256::from(1e17 as u64)).await;
        // Transfer tidak tercantum -> approve langsung
        match dec {
            PolicyDecision::Approve(_) => {}
            other => panic!("transfer biasa tidak butuh konfirmasi: {other:?}"),
        }

        // sekarang dengan require_human_for mencakup "transfer"
        let mut cfg = config();
        cfg.confirmation.require_human_for = vec!["transfer".into()];
        let engine2 = PolicyEngine::new(
            cfg,
            Box::new(InMemoryPolicyState::default()),
            Box::new(NoSimulator),
            Box::new(AlwaysDenyConfirmation),
        );
        let it2 = intent(
            IntentKind::Transfer,
            1,
            "0x1111111111111111111111111111111111111111",
        );
        match check(&engine2, &it2, U256::from(1e17 as u64)).await {
            PolicyDecision::Deny(PolicyError::NeedsHumanConfirmation { .. }) => {}
            other => panic!("seharusnya butuh konfirmasi: {other:?}"),
        }

        // dengan konfirmasi yang menyetujui
        let engine3 = PolicyEngine::new(
            config(),
            Box::new(InMemoryPolicyState::default()),
            Box::new(NoSimulator),
            Box::new(AlwaysConfirm),
        );
        match check(&engine3, &it2, U256::from(1e17 as u64)).await {
            PolicyDecision::Approve(_) => {}
            other => panic!("seharusnya approve setelah konfirmasi: {other:?}"),
        }
    }

    #[tokio::test]
    async fn spend_harian_terbatas() {
        let engine = PolicyEngine::with_defaults(config());
        let it = intent(
            IntentKind::Transfer,
            1,
            "0x1111111111111111111111111111111111111111",
        );
        // batas harian 2.0 ETH; pakai 0.4 ETH berulang sampai melebihi.
        let v = U256::from(4e17 as u64);
        // 5x approve + record => total 2.0 ETH; tx ke-6 ( spent 2.0 + 0.4 > 2.0 ) ditolak.
        for _ in 0..5 {
            let dec = check(&engine, &it, v).await;
            match dec {
                PolicyDecision::Approve(_) => {}
                other => panic!("tx dalam batas harus approve: {other:?}"),
            }
            engine.state.record_spend(Address::ZERO, 0.4).await;
        }
        let dec = check(&engine, &it, v).await;
        match dec {
            PolicyDecision::Deny(PolicyError::SpendPerDayExceeded { .. }) => {}
            other => panic!("seharusnya limit harian: {other:?}"),
        }
    }
}
