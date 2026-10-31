//! `race_broadcast` ( blueprint §8.3, §7 "L2 (Base/Arbitrum/Optimism) ...
//! kirim lewat beberapa endpoint secara paralel", §8.2 poin 6 "Hedged
//! requests" ).
//!
//! Mengirim tx yang SUDAH ditandatangani ke beberapa target ( public RPC
//! pool + relay privat ) secara **paralel** — yang pertama berhasil menang;
//! yang kalah dibatalkan ( drop = cancel request ). Berbeda dari
//! `Broadcaster::broadcast` ( satu target ): di sini tx di-race ke semua
//! target sekaligus, meminimalkan latensi trigger -> tx terkirim ( §8.1
//! target < 100 ms ).
//!
//! ponytail: kemenangan = `eth_sendRawTransaction` OK dari satu target.
//! Tx duplikat tidak mungkin terjadi: tx yang sama ( nonce + signature
//! identik ) ke node manapun menghasilkan tx hash yang sama — node
//! membroadcast ulang tx yang sama, bukan meng-gandakan eksekusinya.

use std::time::Duration;

use futures_util::future::select_ok;
use thiserror::Error;

use nft_chain::{RpcError, RpcPool};
use nft_signer::SignedTx;

/// Target race: public RPC ( lewat pool ) atau relay privat.
#[derive(Debug, Clone)]
pub enum RaceTarget {
    /// `eth_sendRawTransaction` via `RpcPool` ( hedged + failover ).
    PublicPool,
    /// Relay privat ( Flashbots Protect / MEV-Blocker ) — URL explicit.
    Relay(String),
}

#[derive(Debug, Error)]
pub enum RaceError {
    #[error("semua target gagal: {0}")]
    AllFailed(String),

    #[error("tidak ada target race")]
    Empty,

    #[error("timeout race setelah {0:?}")]
    Timeout(Duration),
}

/// Hasil race: tx hash + target yang menang.
#[derive(Debug, Clone)]
pub struct RaceOutcome {
    pub hash: String,
    pub winner: String,
}

/// Race tx ke beberapa target paralel. `timeout` = batas mutlak seluruh
/// race ( §8.3 `race_broadcast(txs, targets)` ).
pub async fn race_broadcast(
    tx: &SignedTx,
    pool: &RpcPool,
    targets: &[RaceTarget],
    timeout: Duration,
) -> Result<RaceOutcome, RaceError> {
    if targets.is_empty() {
        return Err(RaceError::Empty);
    }

    let raw = encode(tx)?;

    let mut futures = Vec::with_capacity(targets.len());
    for t in targets {
        let pool = pool.clone();
        let t = t.clone();
        let raw = raw.clone();
        futures.push(Box::pin(async move {
            let started = std::time::Instant::now();
            let res = match &t {
                RaceTarget::PublicPool => {
                    pool.request_one::<String>(
                        pool.config()
                            .http_urls
                            .first()
                            .map(|u| u.as_str())
                            .unwrap_or(""),
                        "eth_sendRawTransaction",
                        &serde_json::json!([raw]),
                    )
                    .await
                }
                RaceTarget::Relay(url) => send_to_relay(url, &raw).await,
            };
            res.map(|hash| (hash, t.label(), started.elapsed()))
        }));
    }

    let ((hash, winner, elapsed), _losers) = tokio::time::timeout(timeout, select_ok(futures))
        .await
        .map_err(|_| RaceError::Timeout(timeout))?
        .map_err(|e| RaceError::AllFailed(e.to_string()))?;

    tracing::info!(%hash, winner = %winner, ?elapsed, "race broadcast menang");

    Ok(RaceOutcome { hash, winner })
}

impl RaceTarget {
    fn label(&self) -> String {
        match self {
            RaceTarget::PublicPool => "public-rpc".into(),
            RaceTarget::Relay(url) => format!("relay:{url}"),
        }
    }
}

async fn send_to_relay(url: &str, raw: &str) -> Result<String, RpcError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| RpcError::BadResponse(e.to_string()))?;
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "eth_sendRawTransaction",
        "params": [raw],
    });
    let rsp = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| RpcError::BadResponse(e.to_string()))?;
    let status = rsp.status();
    let text = rsp
        .text()
        .await
        .map_err(|e| RpcError::BadResponse(e.to_string()))?;
    if !status.is_success() {
        return Err(RpcError::BadResponse(format!("relay HTTP {status}")));
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
        .map_err(|e| RpcError::BadResponse(format!("parse relay: {e}")))?;
    if let Some(err) = raw_rsp.error {
        return Err(RpcError::BadResponse(err.message));
    }
    Ok(raw_rsp.result.unwrap_or_default())
}

fn encode(tx: &SignedTx) -> Result<String, RaceError> {
    use alloy::consensus::transaction::SignerRecoverable;
    use alloy::eips::eip2718::Encodable2718;
    let envelope = tx.envelope();
    if envelope.recover_signer().is_err() {
        return Err(RaceError::AllFailed(
            "signature tx tidak valid ( recover gagal )".into(),
        ));
    }
    // EIP-2718: `type_byte || rlp(tx)` — bukan RLP string wrapper.
    let buf = envelope.encoded_2718();
    Ok(format!("0x{}", alloy::primitives::hex::encode(&buf)))
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
    async fn race_target_label() {
        assert_eq!(RaceTarget::PublicPool.label(), "public-rpc");
        assert_eq!(
            RaceTarget::Relay("http://x".into()).label(),
            "relay:http://x"
        );
    }

    #[tokio::test]
    async fn race_tanpa_target_error() {
        let p = pool();
        let res = race_broadcast(&signed(), &p, &[], Duration::from_secs(1)).await;
        assert!(res.is_err());
        assert!(matches!(res.unwrap_err(), RaceError::Empty));
    }

    #[tokio::test]
    async fn race_semua_target_mati_error_jelas() {
        let p = pool();
        let res = race_broadcast(
            &signed(),
            &p,
            &[
                RaceTarget::PublicPool,
                RaceTarget::Relay("http://127.0.0.1:9".into()),
            ],
            Duration::from_secs(8),
        )
        .await;
        let err = res.unwrap_err();
        // Kedua target mati: AllFailed bila connection-refused cepat, atau
        // Timeout bila OS menahan SYN. Pesan error harus jelas dua-duanya.
        assert!(
            matches!(err, RaceError::AllFailed(_) | RaceError::Timeout(_)),
            "dapat: {err:?}"
        );
    }

    #[tokio::test]
    async fn encode_menghasilkan_raw_hex() {
        let raw = encode(&signed()).unwrap();
        assert!(raw.starts_with("0x"));
        assert!(raw.len() > 100);
    }
}
