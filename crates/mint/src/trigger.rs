//! `wait_for_trigger` ( blueprint §8.3, §7 "War Mint Tepat Sasaran" ).
//!
//! Model waktu ( §7 ):
//! ```text
//! T0  = waktu mulai sale ( timestamp / block / perubahan state )
//! Tsend = T0 - ( latency_rpc_p95 + margin_jam + waktu_propagasi_block )
//! ```
//! - **Timestamp**:deadline unix; Tsend dihitung dari `latency` ( p95 ) +
//!   `margin_ms` ( offset jam ).
//! - **Block**: tunggu sampai `newHeads` memberi block number >= target.
//! - **State on-chain**: dengarkan `newHeads`, baca `saleActive()` tiap
//!   block baru, picu saat jadi `true` ( §7 "Trigger berbasis state lebih
//!   disukai daripada jam bila tersedia" ).
//!
//! Tiga varian trigger ini saling bebas; `MintExecutor` memilih satu sesuai
//! `MintPlan`. Semua memakai `newHeads` bila tersedia ( pre-warm WS sudah
//! dibuka oleh RPC pool, §8.2 poin 4 ).
//!
//! ponytail: trigger timestamp memakai sleep sampai Tsend. Offset NTP (
//! §6.1 "jam server offset < 50 ms (chrony)" ) adalah tanggung jawab host,
//! bukan crates Rust — margin_ms disediakan untuk menyerap sisanya.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::primitives::Address;
use thiserror::Error;
use tokio::time::Instant;

use nft_chain::{NewHeads, RpcError};

use alloy::primitives::hex;

use crate::sale::SaleReader;

#[derive(Debug, Error)]
pub enum TriggerError {
    #[error("RPC error saat menunggu trigger: {0}")]
    Rpc(#[from] RpcError),

    #[error("timeout menunggu trigger setelah {0:?}")]
    Timeout(Duration),
}

/// Jenis trigger ( §7 ). Dipilih dari `MintPlan` — state paling disukai.
#[derive(Debug, Clone)]
pub enum Trigger {
    /// Tunggu sampai unix timestamp `t0` dikurangi latensi + margin ( Tsend ).
    Timestamp {
        t0: u64,
        latency_p95_ms: u64,
        margin_ms: u64,
    },
    /// Tunggu sampai block number >= `block` ( via `newHeads` ).
    Block { block: u64 },
    /// Tunggu sampai `saleActive()` menjadi true ( tiap block baru ).
    State { contract: Address },
}

/// Waktu sekarang ( unix detik ).
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Trigger {
    /// Tsend ( §7 ): `T0 - ( latency_p95 + margin + propagasi block )`.
    /// Propagasi block ( ~1 block ) ditambahkan sebagai konstanta 12 detik
    /// untuk Ethereum L1; untuk L2 nilainya lebih kecil tapi aman sebagai
    /// margin atas.
    pub fn tsend(&self) -> Option<u64> {
        match self {
            Trigger::Timestamp {
                t0,
                latency_p95_ms,
                margin_ms,
            } => Some(t0.saturating_sub(latency_p95_ms / 1000 + margin_ms / 1000 + 12)),
            _ => None,
        }
    }

    /// Tunggu sampai trigger terjadi. Mengembalikan block number saat trigger
    /// ( bila diketahui dari `newHeads` ) atau 0.
    pub async fn wait(
        &self,
        heads: &mut NewHeads,
        reader: &SaleReader,
        timeout: Duration,
    ) -> Result<u64, TriggerError> {
        let deadline = Instant::now() + timeout;
        match self {
            Trigger::Timestamp { .. } => {
                let Some(tsend) = self.tsend() else {
                    return Err(TriggerError::Timeout(Duration::ZERO));
                };
                let secs = tsend.saturating_sub(now_unix());
                if secs > 0 {
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                }
                // Ambil block number terbaru yang terlihat.
                Ok(heads.next().await.unwrap_or(0))
            }
            Trigger::Block { block } => {
                while Instant::now() < deadline {
                    match tokio::time::timeout_at(deadline, heads.next()).await {
                        Ok(Some(n)) if n >= *block => return Ok(n),
                        Ok(None) => return Err(TriggerError::Timeout(timeout)),
                        Ok(Some(_)) => continue,
                        Err(_) => return Err(TriggerError::Timeout(timeout)),
                    }
                }
                Err(TriggerError::Timeout(timeout))
            }
            Trigger::State { contract } => {
                while Instant::now() < deadline {
                    match tokio::time::timeout_at(deadline, heads.next()).await {
                        Ok(Some(_n)) => {
                            if let Ok(Some(true)) = reader.sale_active(*contract).await {
                                return Ok(_n);
                            }
                        }
                        Ok(None) => return Err(TriggerError::Timeout(timeout)),
                        Err(_) => return Err(TriggerError::Timeout(timeout)),
                    }
                }
                Err(TriggerError::Timeout(timeout))
            }
        }
    }
}

impl SaleReader {
    /// Baca `saleActive()` ( bila ada ) — pembantu trigger state.
    async fn sale_active(&self, contract: Address) -> Result<Option<bool>, RpcError> {
        let sel = crate::sale::selector("saleActive()");
        match self
            .pool()
            .request::<String>(
                "eth_call",
                &serde_json::json!(
                    [{ "to": contract, "data": format!("0x{}", hex::encode(sel)) }, "latest"]
                ),
            )
            .await
        {
            Ok(res) => {
                let hex = res.trim_start_matches("0x");
                let bytes = hex::decode(hex).unwrap_or_default();
                if bytes.len() >= 32 {
                    Ok(Some(bytes[31] != 0))
                } else {
                    Ok(None)
                }
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsend_mengurangi_latensi_dan_margin() {
        let t = Trigger::Timestamp {
            t0: 2_000_000_000,
            latency_p95_ms: 500,
            margin_ms: 1000,
        };
        // 2_000_000_000 - (0 + 1 + 12)
        assert_eq!(t.tsend(), Some(1_999_999_987));
    }

    #[test]
    fn tsend_tidak_negatif() {
        let t = Trigger::Timestamp {
            t0: 5,
            latency_p95_ms: 10_000,
            margin_ms: 0,
        };
        assert_eq!(t.tsend(), Some(0));
    }

    #[test]
    fn trigger_block_tanpa_tsend() {
        let t = Trigger::Block { block: 100 };
        assert_eq!(t.tsend(), None);
    }
}
