//! `Event` — kejadian domain untuk router notifikasi (blueprint §F15, §3C).
//!
//! Isi pesan **wajib bebas rahasia**: tidak ada private key, phrase, token,
//! atau tautan login. Alamat selalu disingkat via `WalletRef::short_address`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Normal,
    High,
    /// Kill switch aktif, saldo rendah kritis, dll — dikirim ke semua kanal.
    Critical,
}

impl Priority {
    pub fn at_least_high(self) -> bool {
        matches!(self, Self::High | Self::Critical)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Template WhatsApp `mint_result`: hasil mint per wallet.
    MintResult,
    WarStarted,
    PreflightFailed,
    SessionExpired,
    LowBalance,
    KillSwitchTriggered,
    ListingSold,
    Error,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub kind: EventKind,
    pub priority: Priority,
    /// Teks pesan siap ditampilkan. Wajib sudah diredaksi (bebas rahasia).
    pub message: String,
    pub at: DateTime<Utc>,
}

impl Event {
    pub fn new(kind: EventKind, priority: Priority, message: impl Into<String>) -> Self {
        Self {
            kind,
            priority,
            message: message.into(),
            at: Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_urutan() {
        assert!(Priority::Critical > Priority::High);
        assert!(Priority::High.at_least_high());
        assert!(Priority::Critical.at_least_high());
        assert!(!Priority::Normal.at_least_high());
        assert!(!Priority::Low.at_least_high());
    }
}
