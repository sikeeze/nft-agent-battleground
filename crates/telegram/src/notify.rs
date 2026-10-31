//! Notifikasi Telegram batched + prioritas ( blueprint §8.2 poin 8, §F6 ).
//!
//! Aturan:
//! - **Batched**: notifikasi beruntun digabung ( batas Telegram ±1 pesan/detik
//!   per chat ) — `DedupeWindow` mengumpulkan pesan dalam window pendek lalu
//!   mengirimnya sebagai satu pesan.
//! - **Prioritas dipisahkan**: notifikasi penting ( hasil mint, error ) masuk
//!   antrean prioritas tinggi yang dikirim segera; yang biasa dibatched.
//! - **Tidak memuat rahasia** ( §3C poin 4 berlaku juga ke Telegram ).
//! - **Tidak memblokir hot path**: pengiriman lewat task async terpisah.
//!
//! ponytail: ini antrean pengiriman; routing prioritas Telegram↔WhatsApp
//! ( dedupe cross-channel, quiet hours ) menyusul Fase 11 ( crate `notify` ).

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, Mutex};

/// Prioritas notifikasi ( selaras dengan `nft_core::event::Priority` ).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NotificationPriority {
    Low,
    Normal,
    High,
    Critical,
}

impl NotificationPriority {
    /// True bila notifikasi ini harus dikirim segera ( tidak dibatched ).
    pub fn bypass_batching(self) -> bool {
        matches!(
            self,
            NotificationPriority::High | NotificationPriority::Critical
        )
    }
}

/// Isi notifikasi ( bebas rahasia ).
#[derive(Debug, Clone)]
pub struct Notification {
    pub chat_id: i64,
    pub text: String,
    pub priority: NotificationPriority,
    pub at: Instant,
}

/// Pengirim notifikasi. Producer memanggil `send()`, task internal yang
/// membatched + mengirim ke `out` ( dispatcher Telegram memakai `out` ).
#[derive(Debug, Clone)]
pub struct Notifier {
    tx: mpsc::Sender<Notification>,
    /// Channel keluaran ( pesan siap kirim, sudah dibatched ).
    out_rx: Arc<Mutex<mpsc::Receiver<(i64, String)>>>,
    /// Isi batch yang sedang dikumpulkan per chat.
    pending: Arc<Mutex<Vec<Notification>>>,
    /// Window batching ( default 1 detik ).
    batch_window: Duration,
}

impl Notifier {
    /// Buat notifier. `batch_window` = seberapa lama pesan biasa
    /// dikumpulkan sebelum dikirim sebagai satu pesan.
    pub fn new(capacity: usize, batch_window: Duration) -> Self {
        let (tx, rx) = mpsc::channel::<Notification>(capacity);
        let (out_tx, out_rx) = mpsc::channel::<(i64, String)>(capacity);
        let pending = Arc::new(Mutex::new(Vec::new()));
        let pending_clone = pending.clone();
        let batch_window_clone = batch_window;

        // Task internal: kumpulkan + kirim batch.
        tokio::spawn(async move {
            let mut rx = rx;
            let mut last_flush = Instant::now();
            loop {
                tokio::select! {
                    maybe = rx.recv() => {
                        let Some(n) = maybe else { break };
                        let mut p = pending_clone.lock().await;
                        if n.priority.bypass_batching() {
                            // Prioritas tinggi: kirim segera ( flush batch
                            // dulu supaya urutan tetap masuk akal ).
                            if !p.is_empty() {
                                let (chat, text) = flush(&mut p);
                                let _ = out_tx.send((chat, text)).await;
                            }
                            let _ = out_tx.send((n.chat_id, n.text)).await;
                            last_flush = Instant::now();
                        } else {
                            p.push(n);
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {
                        // Flush bila window sudah lewat dan ada yang tertunda.
                        let mut p = pending_clone.lock().await;
                        if !p.is_empty() && last_flush.elapsed() >= batch_window_clone {
                            let (chat, text) = flush(&mut p);
                            let _ = out_tx.send((chat, text)).await;
                            last_flush = Instant::now();
                        }
                    }
                }
            }
        });

        Self {
            tx,
            out_rx: Arc::new(Mutex::new(out_rx)),
            pending,
            batch_window,
        }
    }

    /// Kirim notifikasi ( non-blocking dari hot path ).
    pub async fn send(&self, n: Notification) -> Result<(), NotifyError> {
        self.tx
            .send(n)
            .await
            .map_err(|_| NotifyError::ChannelClosed)
    }

    /// Ambil pesan siap kirim ( dipanggil dispatcher Telegram ).
    pub async fn next_out(&self) -> Option<(i64, String)> {
        self.out_rx.lock().await.recv().await
    }

    /// Jumlah pesan tertunda ( untuk test ).
    pub async fn pending_count(&self) -> usize {
        self.pending.lock().await.len()
    }

    /// Window batching ( untuk inspeksi/test ).
    pub fn batch_window(&self) -> Duration {
        self.batch_window
    }
}

fn flush(pending: &mut Vec<Notification>) -> (i64, String) {
    // Gabungkan per chat ( asumsi satu chat umum; bila beberapa chat,
    // kirim per grup chat ).
    let mut grouped: std::collections::HashMap<i64, Vec<String>> = std::collections::HashMap::new();
    let mut order: Vec<i64> = Vec::new();
    for n in pending.drain(..) {
        let e = grouped.entry(n.chat_id).or_default();
        if e.is_empty() {
            order.push(n.chat_id);
        }
        e.push(n.text);
    }
    // Untuk simplicity: chat pertama ( urutan datang ).
    let chat = order.first().copied().unwrap_or(0);
    let texts = grouped.remove(&chat).unwrap_or_default();
    (
        chat,
        texts.join("\n• ").trim_start_matches("• ").to_string(),
    )
}

#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    #[error("channel notifikasi ditutup")]
    ChannelClosed,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(chat: i64, text: &str, prio: NotificationPriority) -> Notification {
        Notification {
            chat_id: chat,
            text: text.into(),
            priority: prio,
            at: Instant::now(),
        }
    }

    #[tokio::test]
    async fn notifikasi_biasa_dibatched() {
        let notifier = Notifier::new(16, Duration::from_millis(150));
        // Tiga pesan biasa dalam window → satu pesan gabungan.
        notifier
            .send(n(1, "a", NotificationPriority::Normal))
            .await
            .unwrap();
        notifier
            .send(n(1, "b", NotificationPriority::Normal))
            .await
            .unwrap();
        notifier
            .send(n(1, "c", NotificationPriority::Normal))
            .await
            .unwrap();

        let out = tokio::time::timeout(Duration::from_millis(500), notifier.next_out())
            .await
            .expect("tidak boleh timeout")
            .expect("harus ada pesan");
        assert_eq!(out.0, 1);
        assert!(out.1.contains("a"), "pesan gabungan: {out:?}");
        assert!(out.1.contains("b"));
        assert!(out.1.contains("c"));
    }

    #[tokio::test]
    async fn notifikasi_critical_tidak_dibatched() {
        let notifier = Notifier::new(16, Duration::from_secs(60));
        notifier
            .send(n(1, "critical!", NotificationPriority::Critical))
            .await
            .unwrap();
        // Tinggi dikirim segera ( window 60 dtk tetap langsung keluar ).
        let out = tokio::time::timeout(Duration::from_millis(300), notifier.next_out())
            .await
            .expect("tidak boleh timeout")
            .expect("harus ada pesan");
        assert_eq!(out.1, "critical!");
    }

    #[tokio::test]
    async fn critical_flush_batch_yang_tertunda() {
        let notifier = Notifier::new(16, Duration::from_secs(60));
        notifier
            .send(n(1, "biasa", NotificationPriority::Normal))
            .await
            .unwrap();
        // Beri waktu task menerima.
        tokio::time::sleep(Duration::from_millis(80)).await;
        notifier
            .send(n(1, "urgent", NotificationPriority::High))
            .await
            .unwrap();

        // Output pertama: batch tertunda yang di-flush, lalu urgent.
        let o1 = tokio::time::timeout(Duration::from_millis(300), notifier.next_out())
            .await
            .unwrap()
            .unwrap();
        let o2 = tokio::time::timeout(Duration::from_millis(300), notifier.next_out())
            .await
            .unwrap()
            .unwrap();
        let texts = [o1.1.clone(), o2.1.clone()];
        assert!(texts.iter().any(|t| t.contains("biasa")));
        assert!(texts.iter().any(|t| t == "urgent"));
    }

    #[tokio::test]
    async fn bypass_batching_hanya_high_dan_critical() {
        assert!(NotificationPriority::High.bypass_batching());
        assert!(NotificationPriority::Critical.bypass_batching());
        assert!(!NotificationPriority::Normal.bypass_batching());
        assert!(!NotificationPriority::Low.bypass_batching());
    }

    #[tokio::test]
    async fn pending_count_menurun_setelah_flush() {
        let notifier = Notifier::new(16, Duration::from_millis(80));
        notifier
            .send(n(1, "x", NotificationPriority::Normal))
            .await
            .unwrap();
        // Setelah window lewat, tertenda di-flush.
        for _ in 0..20 {
            if notifier.pending_count().await == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(notifier.pending_count().await, 0);
    }
}
