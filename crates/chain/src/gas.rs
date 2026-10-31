//! Gas EIP-1559 berbasis persentil ( blueprint §8.3, §9 Fase 3; §6.2
//! "Gas terlalu rendah, tidak masuk block" ).
//!
//! Strategi: ambil `eth_feeHistory` untuk `block_count` block terakhir,
//! kumpulkan `baseFee` dan *effective priority fee* ( dari `rewards` field
//! persentil ), lalu hitung:
//! - `base_fee` berikutnya ( estimasi dari baseFee terakhir ).
//! - `priority_fee` pada persentil yang diminta ( mis. p60 mode "normal",
//!   p90 "aggressive" ) - diambil dari data nyata, bukan tebakan konstan.
//! - `max_fee_per_gas` = base_fee + priority_fee, dibatasi `max_gas_gwei`
//!   dari policy ( §F5 "max_gas_gwei" ).
//!
//! ponytail: persentil dihitung dari `rewards` yang dikembalikan node.
//! Estimasi bayangan ( dari `baseFee` saja ) belum dipakai; naikkan ke
//! sini bila node hanya mengembalikan `base_fee_per_gas`.

use std::sync::Arc;

use serde::Deserialize;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::pool::{RpcError, RpcPool};

#[derive(Debug, Error)]
pub enum GasError {
    #[error("RPC error saat eth_feeHistory: {0}")]
    Rpc(#[from] RpcError),

    #[error("eth_feeHistory mengembalikan data kosong / tidak valid")]
    Empty,

    #[error("baseFee tidak terurai dari feeHistory")]
    BadBaseFee,
}

/// Entri `eth_feeHistory` ( field yang dipakai saja ).
#[derive(Debug, Clone, Deserialize)]
pub struct FeeHistoryEntry {
    /// Base fee per block ( wei ), panjang = block_count + 1.
    pub base_fee_per_gas: Option<Vec<String>>,
    /// Priority fee pada persentil yang diminta, per block ( wei ).
    pub rewards: Option<Vec<Vec<String>>>,
    #[serde(default)]
    pub oldest_block: String,
}

/// Estimasi gas EIP-1559.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GasEstimate {
    /// Wei. Base fee estimasi untuk block berikutnya.
    pub base_fee: u128,
    /// Wei. Priority fee pada persentil diminta.
    pub priority_fee: u128,
    /// Wei. `maxFeePerGas` = base + priority ( dibatasi cap policy ).
    pub max_fee_per_gas: u128,
}

impl GasEstimate {
    /// Gwei ( untuk tampilan / pengecekan policy `max_gas_gwei` ).
    pub fn max_fee_gwei(&self) -> u64 {
        (self.max_fee_per_gas / 1_000_000_000) as u64
    }

    pub fn priority_gwei(&self) -> u64 {
        (self.priority_fee / 1_000_000_000) as u64
    }
}

/// Statistik gas terkumpul ( untuk metrics / debug ).
#[derive(Debug, Clone, Default)]
pub struct GasStats {
    pub samples: u64,
    pub last_base_fee_gwei: f64,
    pub last_priority_fee_gwei: f64,
}

/// Mode gas ( §1 F1: "gas EIP-1559 mode normal / aggressive ( dibatasi
/// policy )" ).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GasMode {
    /// Persentil ~35: hemat, untuk operasi tidak urgent.
    Normal,
    /// Persentil ~60: seimbang.
    Fast,
    /// Persentil ~90: untuk war mint ( tapi tetap di bawah cap policy ).
    Aggressive,
}

impl GasMode {
    pub fn percentile(&self) -> f64 {
        match self {
            GasMode::Normal => 35.0,
            GasMode::Fast => 60.0,
            GasMode::Aggressive => 90.0,
        }
    }
}

/// Oracle gas berbasis `eth_feeHistory`. Cache ringan ( RwLock ) supaya
/// tidak memukul RPC setiap pemanggilan ( §8.2 poin 9 "cache" ).
#[derive(Debug, Clone)]
pub struct GasOracle {
    pool: RpcPool,
    state: Arc<RwLock<GasOracleState>>,
    block_count: u64,
}

#[derive(Debug, Clone)]
struct GasOracleState {
    last_estimate: Option<GasEstimate>,
    samples: u64,
}

impl GasOracle {
    /// `block_count` = jumlah block terakhir yang dianalisis ( mis. 20 ).
    pub fn new(pool: RpcPool, block_count: u64) -> Self {
        Self {
            pool,
            state: Arc::new(RwLock::new(GasOracleState {
                last_estimate: None,
                samples: 0,
            })),
            block_count,
        }
    }

    /// Estimasi gas baru. `mode` menentukan persentil priority fee.
    /// `max_gas_gwei` dari policy membatasi `max_fee_per_gas` ( §F5 ).
    pub async fn estimate(
        &self,
        mode: GasMode,
        max_gas_gwei: u64,
    ) -> Result<GasEstimate, GasError> {
        let params = serde_json::json!([self.block_count, "latest", [mode.percentile() as u64],]);
        let hist = self
            .pool
            .request::<FeeHistoryResponse>("eth_feeHistory", &params)
            .await?;

        let base_fees = hist.base_fee_per_gas.ok_or(GasError::Empty)?;
        if base_fees.is_empty() {
            return Err(GasError::Empty);
        }
        // Base fee berikutnya = entri terakhir ( Ethereum menyertakan
        // next block base fee sebagai elemen terakhir ).
        let next_base =
            parse_wei(base_fees.last().ok_or(GasError::Empty)?).ok_or(GasError::BadBaseFee)?;

        // Priority fee pada persentil: ambil dari `rewards` ( per-block ),
        // nilai terbaru ( elemen terakhir = block terbaru ).
        let priority = hist
            .rewards
            .as_ref()
            .and_then(|r| r.last())
            .and_then(|b| b.first())
            .and_then(|s| parse_wei(s))
            .unwrap_or(FALLBACK_PRIORITY_WEI);

        let mut max_fee = next_base.saturating_add(priority);
        // Batas policy ( §F5 max_gas_gwei ).
        let cap = (max_gas_gwei as u128).saturating_mul(1_000_000_000);
        if max_fee > cap {
            warn!(
                ?mode,
                cap_gwei = max_gas_gwei,
                "max_fee melebihi cap policy"
            );
            max_fee = cap;
        }
        // Priority fee tidak boleh melebihi max_fee.
        let priority = priority.min(max_fee);

        let est = GasEstimate {
            base_fee: next_base,
            priority_fee: priority,
            max_fee_per_gas: max_fee,
        };
        debug!(
            ?mode,
            gwei = est.max_fee_gwei(),
            prio_gwei = est.priority_gwei(),
            "estimasi gas"
        );
        let mut st = self.state.write().await;
        st.last_estimate = Some(est);
        st.samples += 1;
        Ok(est)
    }

    /// Estimasi terakhir ( cache ), bila ada.
    pub async fn last(&self) -> Option<GasEstimate> {
        self.state.read().await.last_estimate
    }

    /// Statistik untuk metrics.
    pub async fn stats(&self) -> GasStats {
        let st = self.state.read().await;
        match &st.last_estimate {
            Some(e) => GasStats {
                samples: st.samples,
                last_base_fee_gwei: e.base_fee as f64 / 1e9,
                last_priority_fee_gwei: e.priority_fee as f64 / 1e9,
            },
            None => GasStats::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct FeeHistoryResponse {
    #[serde(default)]
    base_fee_per_gas: Option<Vec<String>>,
    #[serde(default)]
    rewards: Option<Vec<Vec<String>>>,
}

/// Fallback priority fee bila `rewards` kosong ( node lama ).
/// 1 gwei = nilai aman yang lazim di L2/mainnet.
const FALLBACK_PRIORITY_WEI: u128 = 1_000_000_000;

/// Parse nilai wei dari string JSON-RPC: hex (`0x...`) atau desimal.
/// String desimal murni ( tanpa `0x` ) TIDAK boleh diinterpretasikan hex.
fn parse_wei(s: &str) -> Option<u128> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u128::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<u128>().ok()
    }
}

/// Persentil dari kumpulan nilai ( untuk metrics & analisis gas ).
pub fn gas_percentile(values: &[u128], percentile: f64) -> Option<u128> {
    if values.is_empty() || !(0.0..=100.0).contains(&percentile) {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let idx = ((sorted.len() - 1) as f64 * percentile / 100.0).round() as usize;
    Some(sorted[idx])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_empty() {
        assert_eq!(gas_percentile(&[], 50.0), None);
        assert_eq!(gas_percentile(&[1], 150.0), None);
    }

    #[test]
    fn percentile_median() {
        assert_eq!(gas_percentile(&[10, 20, 30, 40, 50], 50.0), Some(30));
        assert_eq!(gas_percentile(&[10, 20, 30, 40, 50], 90.0), Some(50));
        assert_eq!(gas_percentile(&[10], 60.0), Some(10));
    }

    #[test]
    fn parse_wei_hex_dan_desimal() {
        // 0x3b9aca00 = 1_000_000_000 ( 1 gwei ).
        assert_eq!(parse_wei("0x3b9aca00"), Some(1_000_000_000));
        // String desimal tidak diinterpretasikan sebagai hex.
        assert_eq!(parse_wei("1000000000"), Some(1_000_000_000));
        assert_eq!(parse_wei("0x0"), Some(0));
    }

    #[test]
    fn mode_percentile_urut() {
        assert!(GasMode::Normal.percentile() < GasMode::Fast.percentile());
        assert!(GasMode::Fast.percentile() < GasMode::Aggressive.percentile());
    }
}
