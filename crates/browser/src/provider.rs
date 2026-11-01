//! `Eip1193Provider` -- provider `window.ethereum` buatan agent (blueprint 3B).
//!
//! Setiap permintaan dari halaman web masuk ke `handle_request`, lalu:
//! 1. Method read-only ( eth_chainId, eth_call, ... ) dijawab langsung.
//! 2. `eth_requestAccounts` minta konfirmasi user ( tombol Telegram ) sebelum
//!    alamat dibagikan ke halaman.
//! 3. Sign request ( personal_sign, eth_signTypedData_v4, eth_sendTransaction,
//!    eth_sign ) **wajib lewat Policy Engine** -- aturan keras #2.
//! 4. `eth_sendTransaction` dengan approval tak terbatas ke alamat asing
//!    ditolak otomatis ( 3B ).
//! 5. SIWE: pesan personal_sign yang berisi login SIWE divalidasi domain,
//!    nonce, dan chain sebelum diteruskan.
//!
//! ponytail: sinkronisasi ke browser nyata ( Chromium/CDP ) menyusul saat
//! VPS siap; provider ini diuji melalui policy gate murni.
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use nft_core::intent::{Intent, IntentKind, Originator};
use nft_policy::{PolicyDecision, PolicyEngine};
use serde_json::json;

use crate::eip1193::{
    codes, parse_send_transaction_params, BrowserTx, Eip1193Method, Eip1193Request,
    Eip1193Response, OriginAccounts,
};
use crate::siwe::{SiweError, SiweMessage};

/// Hasil keputusan provider untuk satu sign request.
#[derive(Debug, Clone)]
pub enum SignVerdict {
    /// Diteruskan ke policy engine.
    Forward(Intent),
    /// Ditolak sebelum policy ( validasi SIWE / approval tak terbatas ).
    Reject(String),
}

/// Konfigurasi provider.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// Alamat wallet yang tersedia di provider ini ( 1 per browser worker ).
    pub address: String,
    /// Chain id provider ini.
    pub chain_id: u64,
    /// Origin halaman yang sedang dibuka.
    pub origin: String,
    /// Daftar chain yang diizinkan untuk SIWE.
    pub siwe_allowed_chains: Vec<u64>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            address: "0x0000000000000000000000000000000000000000".into(),
            chain_id: 1,
            origin: "https://example.com".into(),
            siwe_allowed_chains: vec![1, 8453, 42161],
        }
    }
}

/// Provider EIP-1193 dengan gate policy.
pub struct Eip1193Provider {
    config: ProviderConfig,
    policy: Arc<PolicyEngine>,
    origins: Mutex<OriginAccounts>,
    used_nonces: Mutex<BTreeSet<String>>,
    /// Nonce SIWE yang sedang dikeluarkan untuk sesi login ini ( 3B: "nonce segar" ).
    pending_siwe_nonce: Mutex<Option<String>>,
}
impl Eip1193Provider {
    pub fn new(config: ProviderConfig, policy: Arc<PolicyEngine>) -> Self {
        Self {
            config,
            policy,
            origins: Mutex::new(OriginAccounts::default()),
            used_nonces: Mutex::new(BTreeSet::new()),
            pending_siwe_nonce: Mutex::new(None),
        }
    }

    /// Keluarkan nonce segar untuk halaman ( dipanggil saat halaman memuat ).
    pub fn issue_nonce(&self, nonce: impl Into<String>) {
        *self.pending_siwe_nonce.lock().unwrap() = Some(nonce.into());
    }

    /// Apakah origin sudah diotorisasi melihat alamat?
    fn is_authorized(&self, origin_domain: &str) -> bool {
        let origins = self.origins.lock().unwrap();
        origins.is_authorized(origin_domain, &self.config.address)
    }

    /// Handle permintaan EIP-1193 dari halaman web.
    pub async fn handle_request(&self, req: &Eip1193Request) -> Eip1193Response {
        let method = req.method_enum();
        let origin_domain = req.origin_domain().unwrap_or_default();

        // 1. Read-only: jawab langsung.
        if method.is_readonly() {
            return self.answer_readonly(&method);
        }

        // 2. eth_requestAccounts: minta otorisasi user.
        if method == Eip1193Method::EthRequestAccounts {
            if self.is_authorized(&origin_domain) {
                return Eip1193Response::ok(json!([self.config.address]));
            }
            // Otorisasi default menolak ( 3B: user harus menyetujui ).
            return Eip1193Response::denied(
                codes::UNAUTHORIZED,
                "origin belum diotorisasi - setujui dulu via tombol konfirmasi",
            );
        }

        // 3. Sign request: wajib lewat policy ( aturan keras #2 ).
        if method.requires_policy() {
            return self.guarded_sign(req, &method, &origin_domain).await;
        }

        Eip1193Response::denied(
            codes::UNSUPPORTED_METHOD,
            format!("method '{}' tidak didukung", method.as_str()),
        )
    }

    fn answer_readonly(&self, method: &Eip1193Method) -> Eip1193Response {
        match method {
            Eip1193Method::EthChainId => {
                Eip1193Response::ok(json!(format!("0x{:x}", self.config.chain_id)))
            }
            Eip1193Method::EthAccounts => {
                let origins = self.origins.lock().unwrap();
                let domain = self
                    .config
                    .origin
                    .strip_prefix("https://")
                    .or_else(|| self.config.origin.strip_prefix("http://"))
                    .map(|s| s.split('/').next().unwrap_or(s))
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if origins.is_authorized(&domain, &self.config.address) {
                    Eip1193Response::ok(json!([self.config.address]))
                } else {
                    Eip1193Response::ok(json!([]))
                }
            }
            Eip1193Method::EthBlockNumber => Eip1193Response::ok(json!("0x0")),
            _ => Eip1193Response::denied(
                codes::UNSUPPORTED_METHOD,
                "method read-only belum diimplementasikan",
            ),
        }
    }

    async fn guarded_sign(
        &self,
        req: &Eip1193Request,
        method: &Eip1193Method,
        origin_domain: &str,
    ) -> Eip1193Response {
        // personal_sign: mungkin pesan SIWE -> validasi ketat dulu ( 3B ).
        if method == &Eip1193Method::PersonalSign {
            if let Some(msg) = extract_personal_sign_message(&req.params) {
                // Cek apakah ini pesan SIWE ( header khas ).
                if is_siwe_message(&msg) {
                    match self.validate_siwe(&msg, origin_domain) {
                        Ok(_) => {}
                        Err(e) => {
                            return Eip1193Response::denied(
                                codes::POLICY_DENIED,
                                format!("SIWE ditolak: {e}"),
                            );
                        }
                    }
                }
            }
        }

        // eth_sendTransaction: tolak approval tak terbatas ke alamat asing.
        if method == &Eip1193Method::EthSendTransaction {
            if let Some(tx) = parse_send_transaction_params(&req.params) {
                if tx.is_unlimited_approval() {
                    return Eip1193Response::denied(
                        codes::POLICY_DENIED,
                        "approval tak terbatas ( approve(max) / setApprovalForAll ) \
                         ke dosis asing ditolak",
                    );
                }
                return self.forward_tx(tx, origin_domain).await;
            }
            return Eip1193Response::denied(
                codes::UNSUPPORTED_METHOD,
                "params eth_sendTransaction tidak valid",
            );
        }

        // personal_sign / eth_signTypedData_v4 / eth_sign: intent Sign.
        let intent = Intent::new(IntentKind::Sign, Originator::User)
            .with_chain(self.config.chain_id)
            .with_params(json!({
                "method": method.as_str(),
                "origin": origin_domain,
                "browser_worker": true,
            }));
        self.policy_forward(intent).await
    }

    async fn forward_tx(&self, tx: BrowserTx, origin_domain: &str) -> Eip1193Response {
        let intent = Intent::new(IntentKind::Sign, Originator::User)
            .with_chain(self.config.chain_id)
            .with_contract(tx.to.clone())
            .with_params(json!({
                "method": "eth_sendTransaction",
                "origin": origin_domain,
                "from": tx.from,
                "to": tx.to,
                "data": tx.data,
                "browser_worker": true,
            }));
        self.policy_forward(intent).await
    }

    async fn policy_forward(&self, intent: Intent) -> Eip1193Response {
        let decision = self
            .policy
            .policy_check(
                &intent,
                self.config.address.parse().unwrap_or_default(),
                intent
                    .params
                    .get("to")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_default(),
                alloy::primitives::U256::ZERO,
                &[],
                100_000,
                0,
                None,
                0,
                None,
            )
            .await;
        match decision {
            PolicyDecision::Approve(approved) => {
                // Beri hash dummy ke halaman ( signer sebenarnya menangani
                // pengiriman di lapisan browser worker ).
                Eip1193Response::ok(json!(format!(
                    "0x{}",
                    approved.intent_id().replace('-', "")
                )))
            }
            PolicyDecision::Deny(e) => {
                Eip1193Response::denied(codes::POLICY_DENIED, format!("policy menolak: {e}"))
            }
        }
    }

    /// Validasi pesan SIWE terhadap konteks provider ( 3B ).
    fn validate_siwe(&self, msg: &str, _origin_domain: &str) -> Result<SiweMessage, SiweError> {
        let parsed = SiweMessage::parse(msg)?;
        let issued = self.pending_siwe_nonce.lock().unwrap().clone();
        let issued = issued.ok_or(SiweError::NonceMismatch)?;
        let used = self.used_nonces.lock().unwrap().clone();
        parsed.validate(
            &self.config.origin,
            &issued,
            &self.config.siwe_allowed_chains,
            &used,
        )?;
        // Catat nonce supaya tidak bisa dipakai ulang.
        self.used_nonces
            .lock()
            .unwrap()
            .insert(parsed.nonce.clone());
        Ok(parsed)
    }

    /// Tandai origin diotorisasi ( setelah user menyetujui di Telegram ).
    pub fn authorize_origin(&self, origin_domain: impl Into<String>) {
        let mut origins = self.origins.lock().unwrap();
        origins.grant(origin_domain, &self.config.address);
    }

    /// Cabut otorisasi origin.
    pub fn revoke_origin(&self, origin_domain: &str) {
        let mut origins = self.origins.lock().unwrap();
        origins.revoke(origin_domain);
    }

    pub fn config(&self) -> &ProviderConfig {
        &self.config
    }
}

/// Ambil pesan dari params `personal_sign` ( array [ message, address ] ).
fn extract_personal_sign_message(params: &serde_json::Value) -> Option<String> {
    let arr = params.as_array()?;
    let first = arr.first()?;
    // Pesan bisa string UTF-8 atau hex-encoded.
    if let Some(s) = first.as_str() {
        return Some(s.to_string());
    }
    None
}

/// Deteksi apakah pesan adalah SIWE ( header EIP-4361 ).
fn is_siwe_message(msg: &str) -> bool {
    msg.contains("wants you to sign in with your Ethereum account:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nft_chain::policy_state::SqlitePolicyState;
    use nft_core::config::parse_policy;
    use nft_policy::{NoSimulator, PolicyEngine};

    const POLICY_TOML: &str = r#"
[limits]
max_spend_per_tx_eth = 0.5
max_spend_per_day_eth = 2.0
max_gas_gwei = 80
allowed_chains = [1, 8453, 42161]

[allowlist]
contracts = []
block_unlimited_approvals = true

[confirmation]
require_human_above_eth = 0.2
require_human_for = []

[kill_switch]
auto_trigger_on_loss_eth = 1.0

[telegram]
allowed_user_ids = [123456789]
chat_cannot = ["edit_policy", "export_key"]
import_via_chat = false
import_session_timeout_sec = 120
delete_import_message = true
require_totp_for_import = true
burner_max_balance_eth = 0.5
max_wallets = 5
"#;

    async fn policy_engine() -> Arc<PolicyEngine> {
        let pool = nft_core::storage::connect("sqlite::memory:").await.unwrap();
        let state = SqlitePolicyState::new(pool);
        let config = parse_policy(POLICY_TOML).unwrap();
        Arc::new(PolicyEngine::new(
            config,
            Box::new(state),
            Box::new(NoSimulator),
            Box::new(nft_policy::AlwaysDenyConfirmation),
        ))
    }

    fn provider_config() -> ProviderConfig {
        ProviderConfig {
            address: "0xAbCdEf0123456789AbCdEf0123456789AbCdEf01".into(),
            chain_id: 1,
            origin: "https://service.xyz/login".into(),
            siwe_allowed_chains: vec![1, 8453],
        }
    }

    fn req(method: &str, params: serde_json::Value, origin: &str) -> Eip1193Request {
        Eip1193Request {
            method: method.into(),
            params,
            origin: Some(origin.into()),
        }
    }

    #[tokio::test]
    async fn readonly_dijawab_langsung() {
        let p = Eip1193Provider::new(provider_config(), policy_engine().await);
        let r = p
            .handle_request(&req("eth_chainId", json!([]), "https://service.xyz"))
            .await;
        assert_eq!(r.result, Some(json!("0x1")));
        assert!(!r.is_error());
    }

    #[tokio::test]
    async fn request_accounts_tanpa_otorisasi_ditolak() {
        let p = Eip1193Provider::new(provider_config(), policy_engine().await);
        let r = p
            .handle_request(&req(
                "eth_requestAccounts",
                json!([]),
                "https://service.xyz",
            ))
            .await;
        assert!(r.is_error());
        assert_eq!(r.error.unwrap().code, codes::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn request_accounts_setelah_otorisasi_mengembalikan_alamat() {
        let p = Eip1193Provider::new(provider_config(), policy_engine().await);
        p.authorize_origin("service.xyz");
        let r = p
            .handle_request(&req(
                "eth_requestAccounts",
                json!([]),
                "https://service.xyz",
            ))
            .await;
        assert!(!r.is_error());
        assert_eq!(
            r.result,
            Some(json!(["0xAbCdEf0123456789AbCdEf0123456789AbCdEf01"]))
        );
    }

    #[tokio::test]
    async fn send_transaction_approval_tak_terbatas_ditolak() {
        let p = Eip1193Provider::new(provider_config(), policy_engine().await);
        // approve(address,uint256 max) ke alamat asing.
        let data = format!("0x095ea7b3{}{}", "0".repeat(64), "f".repeat(64));
        let params = json!([{
            "from": "0xAbCdEf0123456789AbCdEf0123456789AbCdEf01",
            "to": "0x000000000000000000000000000000000000dEaD",
            "data": data
        }]);
        let r = p
            .handle_request(&req("eth_sendTransaction", params, "https://service.xyz"))
            .await;
        assert!(r.is_error());
        let err = r.error.unwrap();
        assert_eq!(err.code, codes::POLICY_DENIED);
        assert!(err.message.contains("approval tak terbatas"));
    }

    #[tokio::test]
    async fn setapprovalforall_ke_asing_ditolak() {
        let p = Eip1193Provider::new(provider_config(), policy_engine().await);
        let data = format!("0xa22cb465{}{}1", "0".repeat(64), "0".repeat(63));
        let params = json!([{
            "from": "0xAbCdEf0123456789AbCdEf0123456789AbCdEf01",
            "to": "0x000000000000000000000000000000000000dEaD",
            "data": data
        }]);
        let r = p
            .handle_request(&req("eth_sendTransaction", params, "https://service.xyz"))
            .await;
        assert!(r.is_error());
        assert!(r.error.unwrap().message.contains("approval tak terbatas"));
    }

    #[tokio::test]
    async fn siwe_domain_salah_ditolak() {
        let p = Eip1193Provider::new(provider_config(), policy_engine().await);
        p.issue_nonce("32891756");
        let msg = "evil.xyz wants you to sign in with your Ethereum account:\n\
0xAbCdEf0123456789AbCdEf0123456789AbCdEf01\n\
\n\
URI: https://evil.xyz/login\n\
Version: 1\n\
Chain ID: 1\n\
Nonce: 32891756\n\
Issued At: 2026-10-31T12:00:00Z";
        let r = p
            .handle_request(&req(
                "personal_sign",
                json!([msg, "0xAbCd"]),
                "https://evil.xyz",
            ))
            .await;
        assert!(r.is_error());
        let err = r.error.unwrap();
        assert_eq!(err.code, codes::POLICY_DENIED);
        assert!(err.message.contains("domain"));
    }

    #[tokio::test]
    async fn siwe_nonce_tidak_dikeluarkan_ditolak() {
        let p = Eip1193Provider::new(provider_config(), policy_engine().await);
        let msg = siwe_valid_msg("service.xyz", "https://service.xyz/login", "hacked");
        let r = p
            .handle_request(&req(
                "personal_sign",
                json!([msg, "0xAbCd"]),
                "https://service.xyz",
            ))
            .await;
        assert!(r.is_error());
        assert!(r.error.unwrap().message.contains("nonce"));
    }

    fn siwe_valid_msg(domain: &str, uri: &str, nonce: &str) -> String {
        format!(
            "{domain} wants you to sign in with your Ethereum account:\n\
0xAbCdEf0123456789AbCdEf0123456789AbCdEf01\n\
\n\
I accept the Terms.\n\
\n\
URI: {uri}\n\
Version: 1\n\
Chain ID: 1\n\
Nonce: {nonce}\n\
Issued At: 2026-10-31T12:00:00Z"
        )
    }

    #[tokio::test]
    async fn siwe_valid_lulus_ke_policy() {
        let p = Eip1193Provider::new(provider_config(), policy_engine().await);
        p.issue_nonce("32891756");
        let msg = siwe_valid_msg("service.xyz", "https://service.xyz/login", "32891756");
        let r = p
            .handle_request(&req(
                "personal_sign",
                json!([msg, "0xAbCd"]),
                "https://service.xyz",
            ))
            .await;
        // Policy engine memakai AlwaysDenyConfirmation & require_human_above_eth=0.2
        // dengan value 0 -> approve jalur simulation ( NoSimulator ok ).
        assert!(!r.is_error());
    }

    #[tokio::test]
    async fn siwe_nonce_tidak_bisa_dipakai_dua_kali() {
        let p = Eip1193Provider::new(provider_config(), policy_engine().await);
        p.issue_nonce("32891756");
        let msg = siwe_valid_msg("service.xyz", "https://service.xyz/login", "32891756");
        let r1 = p
            .handle_request(&req(
                "personal_sign",
                json!([msg.clone(), "0xAbCd"]),
                "https://service.xyz",
            ))
            .await;
        assert!(!r1.is_error());
        // Re-issue nonce baru untuk percobaan kedua dengan nonce sama.
        p.issue_nonce("32891756");
        let r2 = p
            .handle_request(&req(
                "personal_sign",
                json!([msg, "0xAbCd"]),
                "https://service.xyz",
            ))
            .await;
        assert!(r2.is_error());
        assert!(r2.error.unwrap().message.contains("nonce"));
    }

    #[tokio::test]
    async fn method_tidak_dikenal_ditolak() {
        let p = Eip1193Provider::new(provider_config(), policy_engine().await);
        let r = p
            .handle_request(&req("wallet_permision", json!([]), "https://service.xyz"))
            .await;
        assert!(r.is_error());
        assert_eq!(r.error.unwrap().code, codes::UNSUPPORTED_METHOD);
    }

    #[tokio::test]
    async fn send_transaction_valid_diteruskan_ke_policy() {
        let p = Eip1193Provider::new(provider_config(), policy_engine().await);
        let params = json!([{
            "from": "0xAbCdEf0123456789AbCdEf0123456789AbCdEf01",
            "to": "0x000000000000000000000000000000000000dEaD",
            "data": "0x"
        }]);
        let r = p
            .handle_request(&req("eth_sendTransaction", params, "https://service.xyz"))
            .await;
        // allowlist kosong -> kontrak apapun diizinkan; value 0; gas 0.
        assert!(!r.is_error());
    }

    #[test]
    fn extract_personal_sign_message_ambil_string() {
        let msg = extract_personal_sign_message(&json!(["halo", "0xAb"]));
        assert_eq!(msg.as_deref(), Some("halo"));
        assert!(extract_personal_sign_message(&json!([42])).is_none());
    }

    #[test]
    fn is_siwe_message_mendeteksi_header() {
        assert!(is_siwe_message(
            "service.xyz wants you to sign in with your Ethereum account:\n0x"
        ));
        assert!(!is_siwe_message("halo"));
    }

    #[tokio::test]
    async fn revoke_origin_mencabut_akses() {
        let p = Eip1193Provider::new(provider_config(), policy_engine().await);
        p.authorize_origin("service.xyz");
        assert!(p.is_authorized("service.xyz"));
        p.revoke_origin("service.xyz");
        assert!(!p.is_authorized("service.xyz"));
    }

    #[tokio::test]
    async fn kill_switch_memblokir_sign_request() {
        let p = Eip1193Provider::new(provider_config(), policy_engine().await);
        p.policy.trigger_kill_switch("test".into()).await;
        let r = p
            .handle_request(&req(
                "eth_sendTransaction",
                json!([{
                    "from": "0xAbCdEf0123456789AbCdEf0123456789AbCdEf01",
                    "to": "0x000000000000000000000000000000000000dEaD",
                    "data": "0x"
                }]),
                "https://service.xyz",
            ))
            .await;
        assert!(r.is_error());
        assert!(r.error.unwrap().message.contains("kill switch"));
    }
}
