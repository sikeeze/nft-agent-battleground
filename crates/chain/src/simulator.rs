//! Simulator berbasis `eth_call` ( blueprint Â§F5, Â§9 Fase 3; Â§3 aturan 8 ).
//!
//! Tx yang **revert di simulasi tidak dikirim**. Implementasi ini menggantikan
//! `NoSimulator` di `crates/policy` ( Â§9 Fase 3: "Implementasi `Simulator`
//! asli ( `eth_call`/`eth_simulateV1` ) yang menggantikan `NoSimulator`" ).
//!
//! Cara kerja:
//! 1. Tx dari `ApprovedTx` dienkode menjadi request `eth_call` ( from, to,
//!    data, value ) dengan `latest` state.
//! 2. RPC dipanggil melalui `RpcPool` ( hedged + failover ).
//! 3. Bila RPC mengembalikan error revert ( kode 3 / pesan "revert" ) atau
//!    precheck gagal, kembalikan `Err(reason)` â†’ policy menolak tx.
//!
//! ponytail: `eth_simulateV1` ( multi-tx, state override ) belum dipakai;
//! bila Fase 12 butuh simulasi bundle utuh, tambahkan method `simulate_bundle`
//! di sini. Sekarang `eth_call` cukup untuk validasi tx tunggal.

use alloy::primitives::{Address, Bytes, U256};
use async_trait::async_trait;
use nft_policy::Simulator;
use nft_signer::ApprovedTx;
use thiserror::Error;
use tracing::{debug, warn};

use crate::pool::{RpcError, RpcPool};

#[derive(Debug, Error)]
pub enum SimError {
    #[error("RPC error saat eth_call: {0}")]
    Rpc(#[from] RpcError),

    #[error("simulasi revert: {0}")]
    Reverted(String),

    #[error("precheck gagal: {0}")]
    BadInput(String),
}

/// Hasil simulasi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimOutcome {
    /// Tx berhasil di simulasi ( tidak revert ).
    Ok,
    /// Tx revert di simulasi. `reason` = pesan revert ( dibersihkan ).
    Reverted(String),
}

/// Simulator `eth_call` production. `RpcPool` dibawa sebagai handle ( hedging
/// otomatis berlaku untuk simulasi juga ).
#[derive(Debug, Clone)]
pub struct EthCallSimulator {
    pool: RpcPool,
}

impl EthCallSimulator {
    pub fn new(pool: RpcPool) -> Self {
        Self { pool }
    }

    /// Jalankan `eth_call` untuk satu tx. Mengembalikan outcome mentah.
    pub async fn simulate_call(
        &self,
        from: Address,
        to: Address,
        value: U256,
        data: Bytes,
    ) -> Result<SimOutcome, SimError> {
        let data_hex = format!("0x{}", alloy::primitives::hex::encode(&data));
        let value_hex = format!("0x{:x}", value);
        let params = serde_json::json!([
            {
                "from": from,
                "to": to,
                "value": value_hex,
                "data": data_hex,
            },
            "latest",
        ]);
        let res = self
            .pool
            .request::<serde_json::Value>("eth_call", &params)
            .await;
        match res {
            // `eth_call` sukses â†’ result = "0x" atau data hasil.
            Ok(_v) => Ok(SimOutcome::Ok),
            Err(e) => {
                // Error revert: JSON-RPC code 3 atau pesan berisi "revert".
                if let RpcError::JsonRpc { code, ref message } = e {
                    if code == 3 || message.to_lowercase().contains("revert") {
                        warn!(code, %message, "eth_call revert");
                        return Ok(SimOutcome::Reverted(clean_revert_msg(message)));
                    }
                }
                if let RpcError::BadResponse(ref msg) = e {
                    if msg.to_lowercase().contains("revert") {
                        return Ok(SimOutcome::Reverted(clean_revert_msg(msg)));
                    }
                }
                Err(SimError::from(e))
            }
        }
    }
}

#[async_trait]
impl Simulator for EthCallSimulator {
    /// Tx revert = `Err(reason)` â†’ PolicyEngine menolak ( Â§F5 ).
    async fn simulate(&self, tx: &ApprovedTx) -> Result<(), String> {
        let from = tx.from();
        let to = tx.to();
        let value = tx.value();
        let data: Bytes = tx.data_hex().parse().unwrap_or_default();
        match self.simulate_call(from, to, value, data).await {
            Ok(SimOutcome::Ok) => {
                debug!(intent = tx.intent_id(), "simulasi eth_call OK");
                Ok(())
            }
            Ok(SimOutcome::Reverted(reason)) => {
                warn!(intent = tx.intent_id(), %reason, "simulasi revert â†’ tolak");
                Err(reason)
            }
            Err(e) => Err(format!("simulator error: {e}")),
        }
    }
}

fn clean_revert_msg(msg: &str) -> String {
    msg.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approved() -> ApprovedTx {
        ApprovedTx::new_checked(
            11_155_111,
            Address::ZERO,
            Address::from_slice(&[0x11; 20]),
            U256::ZERO,
            &[0x70, 0xa0, 0x82, 0x31],
            210_000,
            30_000_000_000u128,
            Some(1_000_000_000u128),
            0,
            "intent-test",
        )
        .unwrap()
    }

    #[test]
    fn outcome_equality() {
        assert_eq!(SimOutcome::Ok, SimOutcome::Ok);
        assert_ne!(
            SimOutcome::Reverted("x".into()),
            SimOutcome::Reverted("y".into())
        );
    }

    #[test]
    fn clean_revert_memangkas_spasi() {
        assert_eq!(
            clean_revert_msg("  execution reverted  "),
            "execution reverted"
        );
    }

    #[test]
    fn approved_dibuat_ok() {
        let a = approved();
        assert_eq!(a.chain_id(), 11_155_111);
        assert_eq!(a.intent_id(), "intent-test");
    }

    #[tokio::test]
    async fn simulator_tanpa_rpc_error_jelas() {
        // Pool dengan endpoint mati: eth_call harus error ( bukan panic ).
        let pool = RpcPool::new(crate::pool::RpcPoolConfig {
            http_urls: vec!["http://127.0.0.1:9".into()],
            request_timeout_ms: 300,
            ..Default::default()
        })
        .unwrap();
        let sim = EthCallSimulator::new(pool);
        let res = sim.simulate(&approved()).await;
        assert!(res.is_err(), "endpoint mati harus error, dapat: {res:?}");
        let msg = res.unwrap_err();
        assert!(
            msg.contains("simulator error") || msg.contains("revert"),
            "pesan harus jelas, dapat: {msg}"
        );
        // Policy menerima ini sebagai Deny(SimulationReverted) atau error.
    }
}
