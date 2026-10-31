//! `nft-chain` — Chain Layer ( blueprint §9 Fase 3, §8.2, §8.3 ).
//!
//! Isi: RPC pool multi-provider dengan hedged requests + failover, koneksi
//! WebSocket `newHeads` ( persisten, auto-reconnect ), nonce manager atomik
//! per wallet ( reservasi nonce ), gas EIP-1559 berbasis persentil dari block
//! terbaru, `measure_latency` ( §8.3 ), `Simulator` berbasis `eth_call`, serta
//! `PolicyState` persisten SQLite ( spend harian + kill switch tetap aktif
//! setelah restart ) — menggantikan `InMemoryPolicyState` / `NoSimulator`.
//!
//! Aturan keras yang dipatuhi: default **dry-run** ( broadcaster tidak
//! mengirim tx asli tanpa flag eksplisit; §1 "Mode awal" ); simulasi revert
//! = tx tidak dikirim ( §F5 ).

pub mod gas;
pub mod latency;
pub mod nonce;
pub mod policy_state;
pub mod pool;
pub mod simulator;

pub use gas::{
    gas_percentile, FeeHistoryEntry, GasError, GasEstimate, GasMode, GasOracle, GasStats,
};
pub use latency::{
    measure_latency, persist as persist_latency, Endpoint, LatencyError, LatencySample,
    LatencyStats, LatencyTracker,
};
pub use nonce::{NonceManager, NonceReservation};
pub use policy_state::SqlitePolicyState;
pub use pool::{NewHeads, RpcError, RpcPool, RpcPoolConfig};
pub use simulator::{EthCallSimulator, SimError, SimOutcome};

// Trait `Simulator` ( dari nft-policy ) re-export supaya caller dapat
// memakai `EthCallSimulator` sebagai `policy::Simulator` tanpa import ganda.
#[doc(hidden)]
pub use nft_policy::Simulator;
