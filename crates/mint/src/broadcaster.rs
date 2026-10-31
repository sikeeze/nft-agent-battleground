//! Broadcaster tx ( blueprint §1 F1, §7, §9 Fase 3 ).
//!
//! Mengirim `SignedTx` ( hasil `WalletSigner::sign_approved` ) ke:
//! - **Public RPC** ( `eth_sendRawTransaction` ) via `RpcPool` ( hedged ).
//! - **Private relay** ( Flashbots Protect `eth_sendRawTransaction` ke
//!   `https://rpc.flashbots.net` / `https://rpc-flashbots.flashbots.net` ),
//!   mengurangi frontrun ( §1 F1 "Kirim lewat private relay" ).
//!
//! Aturan keras #3: **default dry-run**. `Broadcaster` dibuat dengan
//! `dry_run = true` ( dari `agent.toml` ); `broadcast()` hanya menulis tx
//! hash ke log + database tanpa mengirim. Pengiriman asli hanya bila
//! `dry_run = false` — diaktifkan manual setelah uji bertahap ( §9 Fase 13 ).
//!
//! ponytail: `race_broadcast` ( §8.3 : kirim ke beberapa relay paralel,
//! ambil yang pertama OK ) belum diimplementasikan — butuh ExecPlan dari
//! Fase 5. Naikkan ke sini saat itu.

use alloy::consensus::transaction::SignerRecoverable;
use alloy::eips::eip2718::Encodable2718;
use alloy::primitives::hex;
use nft_chain::{RpcError, RpcPool};
use nft_signer::SignedTx;
use thiserror::Error;
use tracing::{debug, info, warn};

/// Mode broadcast ( blueprint §1 F1, §7 ). Default: `PrivateRelay` ( lihat
/// `#[default]` — aman untuk mint, fallback public ).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BroadcastMode {
    /// Kirim ke public RPC ( `eth_sendRawTransaction` via pool ).
    Public,
    /// Kirim ke private relay ( Flashbots Protect ) — mengurangi frontrun.
    #[default]
    PrivateRelay,
}

/// Konfigurasi broadcaster.
#[derive(Debug, Clone)]
pub struct BroadcasterConfig {
    /// Aturan keras #3: default true — TIDAK mengirim tx asli.
    pub dry_run: bool,
    /// Mode pengiriman ( public / private relay ).
    pub mode: BroadcastMode,
    /// URL Flashbots Protect ( mainnet ). Untuk testnet pakai public.
    pub flashbots_url: String,
}

impl Default for BroadcasterConfig {
    fn default() -> Self {
        Self {
            dry_run: true,
            mode: BroadcastMode::Public,
            flashbots_url: "https://rpc-flashbots.flashbots.net".to_string(),
        }
    }
}

#[derive(Debug, Error)]
pub enum BroadcastError {
    #[error("dry-run aktif: tx tidak dikirim ( hash {hash} )")]
    DryRun { hash: String },

    #[error("RPC error saat broadcast: {0}")]
    Rpc(#[from] RpcError),

    #[error("gagal meng-enkode tx untuk pengiriman: {0}")]
    Encode(String),

    #[error("pengiriman tx ditolak node: {0}")]
    Rejected(String),
}

/// Hasil satu broadcast.
#[derive(Debug, Clone)]
pub struct BroadcastOutcome {
    /// Tx hash ( keccak256 envelope ), selalu ada walau dry-run.
    pub hash: String,
    /// Mode yang dipakai.
    pub mode: BroadcastMode,
    /// True bila tx benar-benar dikirim ke jaringan ( false = dry-run ).
    pub sent: bool,
    /// Intent asal ( audit trail ).
    pub intent_id: String,
}

/// Hasil submit bundle privat ( Flashbots ).
#[derive(Debug, Clone)]
pub struct SubmitBundleOutcome {
    pub target_block: u64,
    /// Bundle dikirim? ( false = dry-run ).
    pub sent: bool,
    pub note: String,
}

/// Broadcaster tx. Memegang `RpcPool` ( public ) dan klien HTTP untuk relay.
#[derive(Debug, Clone)]
pub struct Broadcaster {
    pool: RpcPool,
    config: BroadcasterConfig,
    http: reqwest::Client,
}

impl Broadcaster {
    pub fn new(pool: RpcPool, config: BroadcasterConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        Self { pool, config, http }
    }

    /// Konfigurasi broadcaster ( untuk inspeksi / test ).
    pub fn config(&self) -> &BroadcasterConfig {
        &self.config
    }

    /// Broadcast satu tx yang sudah ditandatangani.
    ///
    /// **Aturan keras #3**: bila `dry_run` aktif, tx TIDAK dikirim —
    /// hanya dicatat ( audit ) dengan hash-nya. Mengembalikan
    /// `BroadcastOutcome` dengan `sent = false`.
    pub async fn broadcast(&self, tx: &SignedTx) -> Result<BroadcastOutcome, BroadcastError> {
        let hash = format!("{:?}", tx.hash());
        let intent = tx.intent_id().to_string();

        if self.config.dry_run {
            info!(%hash, %intent, "DRY-RUN: tx tidak dikirim ( aturan keras #3 )");
            return Ok(BroadcastOutcome {
                hash,
                mode: self.config.mode,
                sent: false,
                intent_id: intent,
            });
        }

        let raw = encode_tx(tx)?;
        let mode = self.config.mode;
        let sent_hash = match mode {
            BroadcastMode::Public => self.send_raw_via_pool(&raw).await?,
            BroadcastMode::PrivateRelay => self.send_raw_to_relay(&raw).await?,
        };

        // Verifikasi hash yang dikembalikan node = hash lokal ( integritas ).
        if !sent_hash.is_empty() && sent_hash != hash {
            warn!(local = %hash, node = %sent_hash, "hash node ≠ hash lokal");
        }

        Ok(BroadcastOutcome {
            hash,
            mode,
            sent: true,
            intent_id: intent,
        })
    }

    /// Submit bundle privat ke block target ( §7 "kirim sebagai bundle ke
    /// block target N+1 (Flashbots) sehingga bila bundle belum memenuhi
    /// syarat atau tidak masuk, tidak ada gas yang terbuang" ).
    ///
    /// ponytail: implementasi penuh bundle ( `eth_sendBundle` /
    /// `eth_callBundle` via Flashbots matchmaker ) menyusul Fase 5 bersama
    /// `ExecPlan` multi-tx. Sekarang tx tunggal dikirim ke Flashbots
    /// Protect dengan `eth_sendRawTransaction` ( Protect = bundle otomatis
    /// ke block berikutnya ).
    pub async fn submit_bundle(
        &self,
        tx: &SignedTx,
        target_block: u64,
    ) -> Result<SubmitBundleOutcome, BroadcastError> {
        let hash = format!("{:?}", tx.hash());
        if self.config.dry_run {
            info!(%hash, target_block, "DRY-RUN: bundle tidak dikirim");
            return Ok(SubmitBundleOutcome {
                target_block,
                sent: false,
                note: "dry-run aktif ( aturan keras #3 )".into(),
            });
        }

        let raw = encode_tx(tx)?;
        // Flashbots Protect menerima raw tx dan memasukkannya ke bundle
        // privat menuju block berikutnya ( revert protection ).
        let _ = self.send_raw_to_relay(&raw).await?;
        Ok(SubmitBundleOutcome {
            target_block,
            sent: true,
            note: "dikirim ke Flashbots Protect ( bundle privat )".into(),
        })
    }

    // -------------------------------------------------------------------
    // internal
    // -------------------------------------------------------------------

    async fn send_raw_via_pool(&self, raw: &str) -> Result<String, BroadcastError> {
        let params = serde_json::json!([raw]);
        let hash = self
            .pool
            .request::<String>("eth_sendRawTransaction", &params)
            .await
            .map_err(BroadcastError::Rpc)?;
        debug!(%hash, "tx dikirim via public RPC");
        Ok(hash)
    }

    async fn send_raw_to_relay(&self, raw: &str) -> Result<String, BroadcastError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "eth_sendRawTransaction",
            "params": [raw],
        });
        let rsp = self
            .http
            .post(&self.config.flashbots_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| BroadcastError::Rejected(e.to_string()))?;
        let status = rsp.status();
        let text = rsp
            .text()
            .await
            .map_err(|e| BroadcastError::Rejected(e.to_string()))?;
        if !status.is_success() {
            return Err(BroadcastError::Rejected(format!(
                "relay HTTP {status}: {}",
                &text[..text.len().min(200)]
            )));
        }
        #[derive(serde::Deserialize)]
        struct Raw {
            #[serde(default)]
            result: Option<String>,
            #[serde(default)]
            error: Option<RawErr>,
        }
        #[derive(serde::Deserialize)]
        struct RawErr {
            message: String,
        }
        let raw_rsp: Raw = serde_json::from_str(&text)
            .map_err(|e| BroadcastError::Rejected(format!("parse relay: {e}")))?;
        if let Some(err) = raw_rsp.error {
            return Err(BroadcastError::Rejected(err.message));
        }
        let hash = raw_rsp.result.unwrap_or_default();
        debug!(%hash, "tx dikirim ke private relay");
        Ok(hash)
    }
}

/// Enkode `SignedTx` menjadi raw hex `0x...` ( EIP-2718 typed envelope ).
fn encode_tx(tx: &SignedTx) -> Result<String, BroadcastError> {
    let envelope = tx.envelope();
    // Sanity: envelope harus bisa recover ( signature valid ).
    if envelope.recover_signer().is_err() {
        return Err(BroadcastError::Encode(
            "signature tx tidak valid ( recover gagal )".into(),
        ));
    }
    // Enkode sebagai EIP-2718: `type_byte || rlp(tx)` — bukan RLP string
    // wrapper ( `Encodable::encode` menghasilkan string wrapper yang ditolak
    // node: "Failed to decode transaction" ).
    let buf = envelope.encoded_2718();
    Ok(format!("0x{}", hex::encode(&buf)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, U256};
    use nft_signer::{ApprovedTx, WalletSigner};
    use rand::RngCore;

    fn random_signer() -> WalletSigner {
        let mut buf = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf);
        let pk = alloy::signers::local::PrivateKeySigner::from_slice(&buf).unwrap();
        WalletSigner::new("burner-1", pk)
    }

    fn signed() -> SignedTx {
        let s = random_signer();
        let approved = ApprovedTx::new_checked(
            11_155_111,
            s.address(),
            Address::from_slice(&[0x22; 20]),
            U256::from(1_000_000_000_000_000_000u64),
            &[0x70, 0xa0, 0x82, 0x31],
            210_000,
            30_000_000_000u128,
            Some(1_000_000_000u128),
            0,
            "intent-test",
        )
        .unwrap();
        s.sign_approved(&approved).unwrap()
    }

    fn pool() -> RpcPool {
        RpcPool::new(nft_chain::RpcPoolConfig {
            http_urls: vec!["http://127.0.0.1:9".into()],
            request_timeout_ms: 300,
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn dry_run_tidak_mengirim_tx() {
        // Aturan keras #3: default dry-run.
        let bc = Broadcaster::new(pool(), BroadcasterConfig::default());
        assert!(bc.config().dry_run);
        let out = bc.broadcast(&signed()).await.unwrap();
        assert!(!out.sent);
        assert!(out.hash.starts_with("0x"));
        assert_eq!(out.intent_id, "intent-test");
    }

    #[tokio::test]
    async fn dry_run_submit_bundle_tidak_mengirim() {
        let bc = Broadcaster::new(pool(), BroadcasterConfig::default());
        let out = bc.submit_bundle(&signed(), 42).await.unwrap();
        assert!(!out.sent);
        assert_eq!(out.target_block, 42);
        assert!(out.note.contains("dry-run"));
    }

    #[tokio::test]
    async fn encode_tx_menghasilkan_raw_hex() {
        let raw = encode_tx(&signed()).unwrap();
        assert!(raw.starts_with("0x"));
        assert!(
            raw.len() > 100,
            "raw tx harus ter-enkode RLP, len={}",
            raw.len()
        );
    }

    #[tokio::test]
    async fn mode_default_relay_atau_public_tergantung_konfigurasi() {
        let cfg = BroadcasterConfig {
            mode: BroadcastMode::PrivateRelay,
            ..Default::default()
        };
        let bc = Broadcaster::new(pool(), cfg);
        assert_eq!(bc.config().mode, BroadcastMode::PrivateRelay);
    }

    #[tokio::test]
    async fn broadcast_non_dry_run_ke_relay_mati_error_jelas() {
        // dry_run = false tapi relay mati ( URL invalid port ) → error jelas.
        let cfg = BroadcasterConfig {
            dry_run: false,
            mode: BroadcastMode::PrivateRelay,
            flashbots_url: "http://127.0.0.1:9".into(),
        };
        let bc = Broadcaster::new(pool(), cfg);
        let res = bc.broadcast(&signed()).await;
        assert!(res.is_err(), "relay mati harus error, dapat: {res:?}");
    }
}
