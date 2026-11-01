//! `nft-llm` -- klien LLM OpenAI-compatible (blueprint SF7, S9 Fase 9).
//!
//! Satu klien untuk semua model: Claude/GPT/Ollama/custom -- semua endpoint
//! OpenAI-compatible. Model & routing diatur di `models.toml` ( tiap entri
//! memuat `id`, nama env var untuk base URL & API key, nama model ).
//!
//! Aturan:
//! - LLM **hanya mengusulkan Intent** ( SF5, S8.2 poin 3 ); output LLM tidak
//!   pernah dieksekusi langsung, selalu divalidasi oleh kode deterministik
//!   & Policy Engine.
//! - Timeout pendek ( default 3 detik dari `models.toml` ) -- LLM tidak
//!   boleh berada di jalur eksekusi mint ( S8.2 poin 3 ).
//! - Konten eksternal ( deskripsi NFT, nama koleksi ) = data tidak
//!   tepercaya ( SF7 prompt injection ) -- selalu dibungkus sbg pesan
//!   `user` yang disanitasi, tidak pernah `system` ( lihat `sanitize` ).
//! - Ganti model lewat `LlmClient::with_model` ( `/model <id>` di Telegram )
//!   tanpa ubah kode.

pub mod router;
pub mod sanitize;
pub mod tools;

pub use router::{LlmClient, LlmError};
pub use sanitize::{sanitize_untrusted, SanitizeReport};
pub use tools::{parse_tool_calls, ToolCall, ToolCallError, ToolSpec};

use std::time::Duration;

/// Konfigurasi runtime klien ( dibangun dari `models.toml` ).
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// Timeout mutlak satu request ( default 3 dtk, S8.2 poin 3 ).
    pub timeout: Duration,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(3),
        }
    }
}

impl LlmConfig {
    /// Bangun dari `plan_timeout_secs` `ModelsConfig` ( minimal 1 detik ).
    pub fn from_models(timeout_secs: u64) -> Self {
        Self {
            timeout: Duration::from_secs(timeout_secs.max(1)),
        }
    }
}

/// Pesan OpenAI-compatible.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: text.into(),
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: text.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_timeout_minimal_1_detik() {
        assert_eq!(LlmConfig::from_models(0).timeout, Duration::from_secs(1));
        assert_eq!(LlmConfig::from_models(3).timeout, Duration::from_secs(3));
    }

    #[test]
    fn pesan_system_vs_user() {
        let s = ChatMessage::system("aturan");
        let u = ChatMessage::user("kata");
        assert_eq!(s.role, "system");
        assert_eq!(u.role, "user");
    }
}
