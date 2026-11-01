//! `nft-telegram` -- Bot Telegram ( blueprint F6, 3, 9 Fase 4 ).
//!
//! Kanal kontrol utama agent ( 1: "Kanal kontrol: Telegram Bot ( teloxide )" ).
//! Arsitektur hot path ( 8.2 poin 1 ): handler hanya validasi `user_id` +
//! ack instan + masukkan perintah ke antrean `tokio::mpsc`; pekerjaan berat
//! jalan di worker async.
//!
//! Modul:
//! - `access` -- whitelist `user_id`, rate limit, lockout, audit ( 3 ).
//! - `queue` -- antrean perintah + pengukuran ack ( 8.2 poin 1, 8.1 ).
//! - `confirm` -- `HumanConfirmation` via inline keyboard ( F5 ).
//! - `import` -- alur `/import` wallet burner ( 3A ).
//! - `notify` -- notifikasi batched + prioritas ( 8.2 poin 8 ).
//! - `accounts` -- `/accounts` social vault ( 3B, Fase 7 ).
//! - `trade` -- `/portfolio`, `/rules` ( F2, Fase 8 ).
//! - `bot` -- dispatcher utama + pemrosesan perintah.

pub mod access;
pub mod accounts;
pub mod bot;
pub mod confirm;
pub mod engine;
pub mod import;
pub mod llmcommand;
pub mod mintcommand;
pub mod nlcommand;
pub mod notify;
pub mod queue;
pub mod trade;

pub use access::{audit, AccessControl, AccessLimits, AccessOutcome};
pub use accounts::{add_account, check_accounts, list_accounts, remove_account, AccountsOutcome};
pub use bot::{handle_message, process_command, process_import_phrase, spawn, BotState};
pub use confirm::{ConfirmationConfig, TelegramConfirmation};
pub use engine::{build_policy_engine, EngineError};
pub use import::{
    finalize_import, parse_phrase, verify_totp, ImportConfig, ImportError, ImportSessions,
    ParsedSecret,
};
pub use notify::{Notification, NotificationPriority, Notifier};
pub use queue::{CommandQueue, CommandResult, QueueError, QueuedCommand};
pub use trade::{list_rules, load_rules, portfolio_summary, TradeOutcome};
