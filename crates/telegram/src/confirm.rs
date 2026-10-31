//! `HumanConfirmation` via Telegram inline keyboard ( blueprint §F5, §F6,
//! §9 Fase 4 ).
//!
//! Policy engine memanggil hook ini untuk tx yang membutuhkan konfirmasi
//! manusia ( nominal besar, kontrak/koleksi baru, penarikan ). Implementasi
//! ini menggantikan `AlwaysDenyConfirmation` di `crates/policy`.
//!
//! Alur ( §F6 "Konfirmasi manusia ( tombol Telegram ) untuk nominal besar" ):
//! 1. `confirm()` mengirim pesan dengan tombol `✅ Konfirmasi` / `❌ Tolak`
//!    ke chat user yang di-whitelist.
//! 2. Menunggu jawaban ( callback query ) selama `timeout` ( default 120 dtk,
//!    sesuai `import_session_timeout_sec` ).
//! 3. `answerCallbackQuery` dibalas **instan** ( §8.2 poin 8 ), pesan
//!    keyòa diedit ( bukan pesan baru ).
//! 4. Timeout tanpa jawaban = **tolak** ( default aman ).
//!
//! **Aturan keras**: implementasi ini TIDAK pernah memanggil signer. Ia hanya
//! memberi tahu policy engine apakah user menyetujui — keputusan akhir tetap
//! di `PolicyEngine` ( §F5 "LLM hanya mengusulkan Intent; kode deterministik
//! yang memutuskan" ).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nft_policy::HumanConfirmation;
use nft_signer::ApprovedTx;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, MessageId};
use tokio::sync::{oneshot, Mutex, RwLock};
use tracing::{debug, warn};

/// ID unik callback ( payload tombol inline ).
fn callback_id(intent_id: &str, suffix: &str) -> String {
    // Telegram callback_data maks 64 byte; potong supaya muat.
    let base: String = intent_id.chars().take(48).collect();
    format!("{base}:{suffix}")
}

/// Permintaan konfirmasi tertunda ( menunggu jawaban user ).
struct Pending {
    responder: oneshot::Sender<bool>,
    /// Chat tujuan pesan konfirmasi ( future: multi-chat support ).
    #[allow(dead_code)]
    chat_id: ChatId,
    message_id: Option<MessageId>,
}

/// Konfigurasi konfirmasi.
#[derive(Debug, Clone, Copy)]
pub struct ConfirmationConfig {
    /// Berapa lama menunggu jawaban sebelum default-tolak.
    pub timeout: Duration,
}

impl Default for ConfirmationConfig {
    fn default() -> Self {
        // Default aman: 120 detik ( sesuai import_session_timeout_sec ).
        Self {
            timeout: Duration::from_secs(120),
        }
    }
}

/// `HumanConfirmation` via Telegram. Mengirim pesan + tombol, lalu menunggu
/// jawaban dari `TelegramConfirmation::answer_callback`.
#[derive(Clone)]
pub struct TelegramConfirmation {
    bot: Bot,
    chat_id: ChatId,
    config: ConfirmationConfig,
    pending: Arc<RwLock<HashMap<String, Pending>>>,
    timeout_tasks: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
}
impl TelegramConfirmation {
    pub fn new(bot: Bot, chat_id: ChatId, config: ConfirmationConfig) -> Self {
        Self {
            bot,
            chat_id,
            config,
            pending: Arc::new(RwLock::new(HashMap::new())),
            timeout_tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Dipanggil oleh dispatcher Telegram saat callback query masuk.
    /// `data` = payload tombol ( `callback_id` ). Mengembalikan true bila
    /// callback dikenali ( supaya dispatcher membalas answerCallbackQuery ).
    pub async fn answer_callback(&self, data: &str, query_id: &str) -> bool {
        let Some((intent_id, verdict)) = data.split_once(':') else {
            return false;
        };
        let approved = match verdict {
            "ok" => true,
            "no" => false,
            _ => return false,
        };

        let mut pending = self.pending.write().await;
        let p = pending.remove(intent_id);
        drop(pending);
        let Some(p) = p else {
            return false;
        };

        // answerCallbackQuery instan ( §8.2 poin 8 ).
        let _ = self.bot.answer_callback_query(query_id).await;

        // Edit pesan lama ( bukan pesan baru, §8.2 poin 8 ).
        if let Some(msg_id) = p.message_id {
            let text = if approved {
                "✅ Konfirmasi diterima — melanjutkan ( policy engine yang memutuskan )."
            } else {
                "❌ Ditolak — tx tidak dieksekusi."
            };
            let edit = self.bot.edit_message_text(self.chat_id, msg_id, text).await;
            if let Err(e) = edit {
                warn!(?e, "gagal edit pesan konfirmasi");
            }
        }

        // Batalkan task timeout.
        if let Some(t) = self.timeout_tasks.lock().await.remove(intent_id) {
            t.abort();
        }

        let _ = p.responder.send(approved);
        debug!(%intent_id, approved, "jawaban konfirmasi diterima");
        true
    }

    /// Apakah ada permintaan konfirmasi tertunda untuk intent ini?
    pub async fn has_pending(&self, intent_id: &str) -> bool {
        self.pending.read().await.contains_key(intent_id)
    }
}

#[async_trait]
impl HumanConfirmation for TelegramConfirmation {
    async fn confirm(&self, tx: &ApprovedTx, reason: &str) -> bool {
        let intent_id = tx.intent_id().to_string();
        let (tx1, rx1) = oneshot::channel::<bool>();

        // Susun pesan + tombol ( §5 MintPlan-style ringkasan aman ).
        let from = tx.from();
        let to = tx.to();
        let value_eth = wei_to_eth(tx.value());
        let text = format!(
            "⚠️ *Konfirmasi diperlukan*\n\
             Intent: `{intent_id}`\n\
             Dari: `{from}`\n\
             Ke: `{to}`\n\
             Nilai: {value_eth:.6} ETH\n\
             Alasan: {reason}\n\n\
             Tx hanya dieksekusi setelah Anda menekan tombol."
        );
        let approve_btn =
            InlineKeyboardButton::callback("✅ Konfirmasi", callback_id(&intent_id, "ok"));
        let reject_btn = InlineKeyboardButton::callback("❌ Tolak", callback_id(&intent_id, "no"));
        let keyboard = InlineKeyboardMarkup::new(vec![[approve_btn, reject_btn]]);

        // Kirim pesan.
        let send = self
            .bot
            .send_message(self.chat_id, text)
            .parse_mode(teloxide::types::ParseMode::MarkdownV2)
            .reply_markup(keyboard)
            .await;
        let message_id = match send {
            Ok(m) => Some(m.id),
            Err(e) => {
                warn!(?e, %intent_id, "gagal mengirim pesan konfirmasi → tolak");
                return false;
            }
        };

        // Daftarkan permintaan tertunda.
        self.pending.write().await.insert(
            intent_id.clone(),
            Pending {
                responder: tx1,
                chat_id: self.chat_id,
                message_id,
            },
        );

        // Timeout: default tolak ( §F5 default aman ).
        let pending_clone = self.pending.clone();
        let timeouts = self.timeout_tasks.clone();
        let intent_timeout = intent_id.clone();
        let timeout = self.config.timeout;
        let handle = tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            let mut p = pending_clone.write().await;
            if let Some(entry) = p.remove(&intent_timeout) {
                warn!(%intent_timeout, "konfirmasi timeout → tolak");
                let _ = entry.responder.send(false);
            }
            timeouts.lock().await.remove(&intent_timeout);
        });
        self.timeout_tasks.lock().await.insert(intent_id, handle);

        // Tunggu jawaban ( atau timeout → false ).
        rx1.await.unwrap_or(false)
    }
}

/// Konversi wei → ETH ( untuk tampilan; presisi 6 desimal cukup ).
fn wei_to_eth(wei: alloy::primitives::U256) -> f64 {
    let s = wei.to_string();
    let bytes = s.as_bytes();
    if bytes.len() <= 18 {
        let v: f64 = s.parse().unwrap_or(0.0);
        v / 1e18
    } else {
        let whole = &s[..bytes.len() - 18];
        let frac = &s[bytes.len() - 18..];
        let w: f64 = whole.parse().unwrap_or(0.0);
        let f: f64 = format!("0.{}", frac).parse().unwrap_or(0.0);
        w + f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_id_dengan_suffix() {
        let id = callback_id("intent-123", "ok");
        assert_eq!(id, "intent-123:ok");
        assert!(callback_id("intent-123", "no").ends_with(":no"));
    }

    #[test]
    fn callback_id_dipotong_sampai_48_karakter() {
        let long = "x".repeat(100);
        let id = callback_id(&long, "ok");
        // 48 + 1 ( : ) + 2 ( ok ) = 51 <= 64 ( limit Telegram ).
        assert!(id.len() <= 51);
    }

    #[test]
    fn wei_to_eth_konversi() {
        let one = alloy::primitives::U256::from(1_000_000_000_000_000_000u64);
        assert!((wei_to_eth(one) - 1.0).abs() < 1e-12);
        // 0.05 ETH = 5e16 wei.
        let half = alloy::primitives::U256::from(50_000_000_000_000_000u64);
        assert!((wei_to_eth(half) - 0.05).abs() < 1e-12);
        assert_eq!(wei_to_eth(alloy::primitives::U256::ZERO), 0.0);
    }

    #[tokio::test]
    async fn telegram_confirmation_dibuat_tanpa_bot_valid() {
        // Bot dengan token dummy ( tidak pernah mengirim — confirm akan
        // gagal kirim tapi tidak panic ).
        let bot = Bot::new("123456:ABC-DEF");
        let conf = TelegramConfirmation::new(bot, ChatId(1), ConfirmationConfig::default());
        assert_eq!(conf.config.timeout, Duration::from_secs(120));
        assert!(!conf.has_pending("x").await);
    }

    #[tokio::test]
    async fn answer_callback_yang_tidak_dikenal_dihindari() {
        let bot = Bot::new("123456:ABC-DEF");
        let conf = TelegramConfirmation::new(bot, ChatId(1), ConfirmationConfig::default());
        // Payload tanpa ':' → tidak dikenali.
        assert!(!conf.answer_callback("tanpa-colon", "query").await);
        // Payload dengan suffix asing → tidak dikenali.
        assert!(!conf.answer_callback("intent:maybe", "query").await);
    }
}
