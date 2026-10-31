//! Antrean perintah + pengukuran ack ( blueprint -§8.2 poin 1, -§8.3 target
//! latensi "Ack perintah Telegram < 300 ms" ).
//!
//! Arsitektur hot path ( -§8.2 poin 1 ):
//! - Handler Telegram **hanya** memvalidasi `user_id`, membalas ack, dan
//!   memasukkan perintah ke antrean `tokio::mpsc`.
//! - Pekerjaan berat ( resolve, LLM, simulasi ) berjalan di worker async
//!   terpisah, tidak memblokir handler.
//!
//! `CommandQueue` menyatukan keduanya: kirim perintah ( non-blocking ) dan
//! salurkan balikan hasil ke channel `reply` ( -§8.2 poin 2 "LLM tidak berada
//! di jalur eksekusi" berlaku juga untuk command berat ).

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{mpsc, Mutex};

/// Perintah dari Telegram yang masuk antrean ( setelah ak cepat diterima ).
#[derive(Debug, Clone)]
pub struct QueuedCommand {
    /// `/status`, `/stop`, `/import`, dll ( sudah diparsing tanpa LLM,
    /// -§8.2 poin 3 ).
    pub command: String,
    pub user_id: i64,
    pub chat_id: i64,
    /// Argumen setelah nama perintah ( mis. `<link>` dari `/mint <link>` ).
    pub args: Option<String>,
    /// Waktu pesan diterima ( untuk mengukur ack ).
    pub received_at: Instant,
}

/// Hasil eksekusi perintah dari worker ( dibalas ke user ).
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub command: String,
    pub chat_id: i64,
    /// Balasan user-facing ( bebas rahasia ).
    pub text: String,
    /// True bila perintah diabaikan ( user non-whitelist ) -€” tidak dibalas.
    pub silent: bool,
}

/// Antrean perintah: producer ( handler Telegram ) -†’ consumer ( worker ).
#[derive(Debug)]
pub struct CommandQueue {
    tx: mpsc::Sender<QueuedCommand>,
    rx: Arc<Mutex<mpsc::Receiver<QueuedCommand>>>,
    /// Channel hasil dari worker -†’ pengirim balasan Telegram.
    reply_tx: mpsc::Sender<CommandResult>,
    /// Receiver hasil ( diambil sekali oleh `spawn_sender` lewat
    /// `take_reply_rx` ).
    reply_rx: Mutex<Option<mpsc::Receiver<CommandResult>>>,
    /// Latensi ack terakhir ( ms ) -€” untuk metrics acceptance < 300 ms.
    last_ack_ms: Arc<Mutex<u64>>,
}

impl CommandQueue {
    /// Buat antrean dengan kapasitas tertentu. `reply_rx` diambil
    /// kemudian oleh `take_reply_rx` ( task pengirim balasan Telegram ).
    pub fn new(capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel::<QueuedCommand>(capacity);
        let (reply_tx, reply_rx) = mpsc::channel::<CommandResult>(capacity);
        Self {
            tx,
            rx: Arc::new(Mutex::new(rx)),
            reply_tx,
            reply_rx: Mutex::new(Some(reply_rx)),
            last_ack_ms: Arc::new(Mutex::new(0)),
        }
    }

    /// Masukkan perintah ke antrean ( dari handler Telegram ).
    /// Mengembalikan ack ( dalam ms ) -€” waktu dari `received_at` sampai
    /// perintah berhasil masuk antrean + ack tercatat.
    pub async fn enqueue(&self, cmd: QueuedCommand) -> Result<u64, QueueError> {
        let ack_ms = cmd.received_at.elapsed().as_millis() as u64;
        self.tx
            .send(cmd)
            .await
            .map_err(|_| QueueError::ChannelClosed)?;
        *self.last_ack_ms.lock().await = ack_ms;
        Ok(ack_ms)
    }

    /// Ambil perintah berikutnya ( dipanggil worker ).
    pub async fn next(&self) -> Option<QueuedCommand> {
        self.rx.lock().await.recv().await
    }

    /// Ambil receiver hasil ( dipanggil satu kali oleh `spawn_sender` ).
    /// Pemanggilan kedua mengembalikan `None`.
    pub async fn take_reply_rx(&self) -> Option<mpsc::Receiver<CommandResult>> {
        self.reply_rx.lock().await.take()
    }

    /// Kirim hasil dari worker ( dibalas ke user oleh pengirim ).
    pub async fn reply(&self, result: CommandResult) -> Result<(), QueueError> {
        self.reply_tx
            .send(result)
            .await
            .map_err(|_| QueueError::ChannelClosed)
    }

    /// Latensi ack terakhir ( ms ).
    pub async fn last_ack_ms(&self) -> u64 {
        *self.last_ack_ms.lock().await
    }
}

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("channel antrean ditutup")]
    ChannelClosed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Duration;

    #[tokio::test]
    async fn enqueue_lalu_next_mengembalikan_perintah() {
        let q = CommandQueue::new(8);
        let cmd = QueuedCommand {
            command: "/status".into(),
            user_id: 1,
            chat_id: 100,
            args: None,
            received_at: Instant::now(),
        };
        let ack = q.enqueue(cmd.clone()).await.unwrap();
        assert!(ack < 100, "ack harus kecil ( <100 ms ), dapat: {ack} ms");

        let got = q.next().await.unwrap();
        assert_eq!(got.command, "/status");
        assert_eq!(got.user_id, 1);
        assert_eq!(got.chat_id, 100);
    }

    #[tokio::test]
    async fn last_ack_ms_tercatat() {
        let q = CommandQueue::new(8);
        assert_eq!(q.last_ack_ms().await, 0);
        let cmd = QueuedCommand {
            command: "/stop".into(),
            user_id: 1,
            chat_id: 100,
            args: None,
            received_at: Instant::now() - Duration::from_millis(10),
        };
        let ack = q.enqueue(cmd).await.unwrap();
        assert!(ack >= 10);
        assert_eq!(q.last_ack_ms().await, ack);
    }

    #[tokio::test]
    async fn reply_mengirim_hasil_ke_receiver() {
        let q = CommandQueue::new(8);
        let mut reply_rx = q.take_reply_rx().await.unwrap();
        let res = CommandResult {
            command: "/status".into(),
            chat_id: 100,
            text: "OK".into(),
            silent: false,
        };
        q.reply(res.clone()).await.unwrap();
        let got = reply_rx.recv().await.unwrap();
        assert_eq!(got.text, "OK");
        assert!(!got.silent);
    }

    #[tokio::test]
    async fn antrean_penuh_tidak_hang() {
        // Kapasitas 1: enqueue kedua menunggu, tapi drop receiver tidak
        // memblokir enqueue pertama. Test dasar: enqueue tetap bekerja.
        let q = CommandQueue::new(1);
        let cmd = QueuedCommand {
            command: "/x".into(),
            user_id: 1,
            chat_id: 2,
            args: None,
            received_at: Instant::now(),
        };
        assert!(q.enqueue(cmd).await.is_ok());
    }

    #[tokio::test]
    async fn next_setelah_channel_kosong_menunggu() {
        let q = CommandQueue::new(2);
        // Tidak ada perintah -†’ next() menunggu; test dengan timeout singkat.
        let res = tokio::time::timeout(Duration::from_millis(50), q.next()).await;
        assert!(res.is_err(), "next() harus menunggu bila kosong");
    }
}
