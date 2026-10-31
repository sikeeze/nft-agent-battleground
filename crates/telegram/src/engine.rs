//! Perangkai PolicyEngine production ( blueprint §9 Fase 4: "Wire
//! SqlitePolicyState + EthCallSimulator ke PolicyEngine saat startup" ).
//!
//! Fase 2 memakai `PolicyEngine::with_defaults` ( InMemoryPolicyState +
//! NoSimulator + AlwaysDenyConfirmation ). Fase 4 menggantikan ketiganya
//! dengan implementasi production:
//! - `SqlitePolicyState` ( kill switch & spend harian persisten ),
//! - `EthCallSimulator` ( `eth_call` revert = tolak ),
//! - `TelegramConfirmation` ( inline keyboard + timeout default-tolak ).
//!
//! **Catatan penting**: `PolicyEngine::new` meminta `Box<dyn PolicyState>`
//! dll. — tipe trait object. Fungsi ini menyembunyikan boilerplate tersebut.
//!
//! ponytail: `RaceBroadcaster`/`GasOracle` di-suntikkan di Fase 5/6 saat
//! `ExecPlan` ada. Sekarang cukup state + simulator + confirmation.

use nft_chain::{EthCallSimulator, RpcPool};
use nft_core::config::PolicyConfig;
use nft_policy::PolicyEngine;
use sqlx::SqlitePool;

use crate::confirm::{ConfirmationConfig, TelegramConfirmation};

/// Error saat merakit policy engine.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("konfigurasi policy tidak valid: {0}")]
    Policy(String),
}

/// Rakit `PolicyEngine` production: state persisten SQLite + simulator
/// `eth_call`via pool RPC + konfirmasi Telegram.
///
/// `bot` = Bot Telegram, `chat_id` = chat admin ( whitelist ), `pool` =
/// pool RPC chain yang aktif, `db` = SQLite ( state policy ).
pub async fn build_policy_engine(
    config: PolicyConfig,
    bot: teloxide::Bot,
    chat_id: teloxide::types::ChatId,
    rpc_pool: RpcPool,
    db: SqlitePool,
    confirmation_config: ConfirmationConfig,
) -> Result<PolicyEngine, EngineError> {
    // State persisten ( kill switch tetap aktif setelah restart ).
    let state = nft_chain::SqlitePolicyState::new(db);
    // Simulator eth_call ( revert = tolak ).
    let simulator = EthCallSimulator::new(rpc_pool);
    // Konfirmasi manusia via inline keyboard.
    let confirmation = TelegramConfirmation::new(bot, chat_id, confirmation_config);

    Ok(PolicyEngine::new(
        config,
        Box::new(state),
        Box::new(simulator),
        Box::new(confirmation),
    ))
}
