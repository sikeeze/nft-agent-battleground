//! `nft-trader` — rules buy/sell, watcher, PnL (blueprint §F2, §9 Fase 8).
//!
//! - `rules.rs` — parser `rules.toml` ( `[[rule]]` ) + validasi ketat.
//! - `engine.rs` — engine trading: evaluasi rule terhadap floor/listings,
//!   take profit / stop loss, auto-cancel listing basi.
//! - `pnl.rs` — portfolio tracker: harga beli, nilai sekarang, PnL.
//!
//! **Aturan**: setiap aksi jual/beli/list/cancel yang dihasilkan engine
//! adalah `TradeAction` — **belum ditandatangani**. Pemanggil wajib
//! melewatkannya ke Policy Engine ( aturan keras #2 ). Trader tidak
//! memanggil marketplace adapter secara langsung untuk side-effect.

pub mod engine;
pub mod naturallang;
pub mod pnl;
pub mod rules;

pub use engine::{CollectionMarket, ListingInfo, TradeAction, TradeEngine, TradeEngineConfig};
pub use naturallang::{
    add_draft, parse_draft, parse_pattern, DraftError, RuleDraft, NL_SYSTEM_PROMPT,
};
pub use pnl::{PnLEntry, Portfolio};
pub use rules::{parse_rules, Rule, RuleAction, RuleKind, RulesConfig, RulesError};
