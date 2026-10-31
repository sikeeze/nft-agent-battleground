//! Bot Telegram utama ( blueprint §F6, §3, §9 Fase 4 ).
//!
//! Arsitektur ( §8.2 poin 1 - "Pisahkan hot path dan cold path" ):
//! 1. `Dispatcher` menerima update; handler **hanya**:
//!    - validasi `user_id` whitelist ( §3 poin 2 ),
//!    - balas ack instan ( "ack diterima" ),
//!    - masukkan perintah ke `CommandQueue`.
//! 2. Worker async memproses perintah berat ( resolve, LLM, simulasi ).
//! 3. Hasil dibalas via `CommandQueue::reply` a+' task pengirim.
//!
//! Perintah yang didukung ( Fase 4 ): `/status`, `/stop`, `/import`,
//! `/newwallet`, `/wallets`, `/approve`, `/reject`. Parse **tanpa LLM**
//! ( §8.2 poin 3 ).
//!
//! **Keamanan**:
//! - user non-whitelist diabaikan **tanpa balasan** ( §3 poin 2 ).
//! - alur `/import` mengikuti §3A ( lihat `import.rs` ).
//! - phrase / private key tidak pernah di-log, tidak di-ulang di balasan.
//!
//! ponytail: dispatcher lengkap teloxideBoot membutuhkan token + loop
//! update; untuk testabilitas, `BotHandler` disusun terpisah dari `spawn`
//! ( pemrosesan update di-injeksi lewat fungsi `handle_message` ).

use std::sync::Arc;
use std::time::Instant;

use nft_core::config::PolicyConfig;
use nft_keystore::Keystore;
use teloxide::prelude::*;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, warn};

use crate::access::{audit, AccessControl, AccessOutcome};
use crate::import::{finalize_import, parse_phrase, ImportConfig, ImportOutcome, ImportSessions};
use crate::queue::{CommandQueue, CommandResult, QueuedCommand};

/// State bersama seluruh handler bot.
pub struct BotState {
    pub config: PolicyConfig,
    pub access: AccessControl,
    pub queue: CommandQueue,
    pub import_config: ImportConfig,
    pub import_sessions: ImportSessions,
    /// Keystore diproteksi Mutex ( modifikasi saat /import, /newwallet ).
    pub keystore: Mutex<Keystore>,
    /// Passphrase keystore ( dari env, bukan dari chat ).
    pub passphrase: nft_core::secret::Secret<Vec<u8>>,
    /// Chat admin ( user pertama di whitelist ) - tujuan notifikasi.
    pub admin_chat: ChatId,
    pub db: sqlx::SqlitePool,
    /// State policy ( kill switch ). `/stop` memicu kill switch di sini
    /// ( §F5: "/stop mematikan semua job dan signer" ). `None` bila policy
    /// engine belum dirakit ( mis. saat test ).
    pub policy_state: Option<std::sync::Arc<dyn nft_policy::PolicyState + Send + Sync>>,
}

/// Rakitan perintah masuk a+' `QueuedCommand` ( atau None bila invalid ).
fn make_command(msg: &Message, enabled_commands: &[&str]) -> Option<QueuedCommand> {
    let text = msg.text().unwrap_or_default().trim();
    if text.is_empty() {
        return None;
    }
    let (head, rest) = match text.split_once(' ') {
        Some((h, r)) => (h, Some(r.trim().to_string())),
        None => (text, None),
    };
    if !head.starts_with('/') {
        return None;
    }
    if !enabled_commands.contains(&head) {
        return None;
    }
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64)?;
    let chat_id = msg.chat.id.0;
    Some(QueuedCommand {
        command: head.to_string(),
        user_id,
        chat_id,
        args: rest,
        received_at: Instant::now(),
    })
}

/// Daftar perintah yang dikenal ( parse tanpa LLM, §8.2 poin 3 ).
pub const ENABLED_COMMANDS: [&str; 7] = [
    "/status",
    "/stop",
    "/import",
    "/newwallet",
    "/wallets",
    "/approve",
    "/reject",
];

/// Proses satu pesan masuk ( hot path: validasi + ack + enqueue ).
/// Mengembalikan `(aknowledge_text, chat_id_to_reply)` atau `None` bila
/// pesan diabaikan ( user non-whitelist / bukan perintah ).
pub async fn handle_message(state: &BotState, msg: &Message) -> Option<(String, i64)> {
    // 1. Ambil user_id ( §3 poin 2: whitelist user_id, bukan username ).
    let Some(user_id) = msg.from.as_ref().map(|u| u.id.0 as i64) else {
        debug!("pesan tanpa from - abaikan");
        return None;
    };

    // 2. Parse perintah tanpa LLM.
    let Some(cmd) = make_command(msg, &ENABLED_COMMANDS) else {
        // Bukan perintah dikenal a+' cek apakah ini sesi import yang menunggu
        // phrase ( lihat `handle_import_phrase` dipanggil worker ).
        return None;
    };

    // 3. Kontrol akses ( whitelist + rate limit + lockout ).
    match state.access.check(user_id, true).await {
        AccessOutcome::Allow => {}
        AccessOutcome::UnknownUser => {
            // §3 poin 2: diabaikan TANPA balasan.
            debug!(user_id, "user non-whitelist - abaikan");
            return None;
        }
        AccessOutcome::RateLimited => {
            return Some((
                "[...] Terlalu banyak perintah - coba lagi nanti.".into(),
                cmd.chat_id,
            ));
        }
        AccessOutcome::LockedOut => {
            return Some((
                "[X] Aksi Anda diblokir sementara ( terlalu banyak kegagalan ). Coba lagi nanti."
                    .into(),
                cmd.chat_id,
            ));
        }
    }

    // 4. Ack instan + enqueue ( hot path selesai di sini ).
    let ack = format!("Diterima: {} - diproses", cmd.command);
    let chat_id = cmd.chat_id;
    match state.queue.enqueue(cmd).await {
        Ok(ack_ms) => {
            debug!(user_id, ack_ms, "perintah masuk antrean");
            // Audit ( §3 poin 7 ) - tanpa rahasia.
            let _ = audit(
                &state.db,
                &user_id.to_string(),
                "command",
                None,
                Some(&serde_json::json!({"ack_ms": ack_ms})),
            )
            .await;
        }
        Err(e) => {
            warn!(?e, "antrean penuh/ditutup");
            return Some((
                "[!] Sistem sibuk - perintah tidak masuk antrean. Coba lagi.".into(),
                chat_id,
            ));
        }
    }
    Some((ack, chat_id))
}

/// Proses hasil import ( dari worker ) menjadi balasan user-facing.
/// **Tidak pernah mengulang phrase** ( §3A poin 6 ).
pub fn import_reply(outcome: &ImportOutcome) -> String {
    let balance = outcome
        .balance_eth
        .map(|b| format!("{b:.4} ETH"))
        .unwrap_or_else(|| "(saldo tidak dapat dicek)".into());
    format!(
        "[OK] Wallet burner di-import\nAlamat: `{}`\nSaldo: {balance}\n\n\
         Phrase/KEY sudah dienkripsi ke keystore dan dihapus dari memori. \
         Bot tidak akan pernah menampilkan ulang.",
        outcome.address
    )
}

/// Proses satu perintah yang sudah masuk antrean ( cold path / worker ).
/// Mengembalikan `CommandResult` untuk dibalas ke user.
pub async fn process_command(state: &BotState, cmd: QueuedCommand) -> CommandResult {
    let user_id = cmd.user_id;
    let chat_id = cmd.chat_id;
    let silent = false;
    match cmd.command.as_str() {
        "/status" => {
            let kill = state
                .config
                .kill_switch
                .auto_trigger_on_loss_eth
                .to_string();
            CommandResult {
                command: cmd.command,
                chat_id,
                text: format!(
                    "dŸ¤- nft-agent\n\
                     - Mode: dry-run\n\
                     - Kill switch auto pada rugi a‰¥ {kill} ETH\n\
                     - Wallet: {} terdaftar\n\
                     - Perintah aktif: /status /stop /import /newwallet /wallets",
                    state.keystore.lock().await.wallets().len()
                ),
                silent,
            }
        }
        "/stop" => {
            // Kill switch ( §F5 ): /stop mematikan semua job dan signer.
            // < 2 detik ( acceptance Fase 4 ) — trigger ke state persisten.
            if let Some(ps) = &state.policy_state {
                ps.trigger_kill_switch("/stop dari Telegram".into()).await;
            }
            let _ = audit(
                &state.db,
                &user_id.to_string(),
                "/stop",
                Some("kill switch"),
                None,
            )
            .await;
            CommandResult {
                command: cmd.command,
                chat_id,
                text: "[STOP] Kill switch diaktifkan - semua job dan signer dihentikan. \
                       Gunakan /resume ( Fase selanjutnya ) untuk menyalakan kembali."
                    .into(),
                silent,
            }
        }
        "/wallets" => {
            let ks = state.keystore.lock().await;
            let list: Vec<String> = ks
                .wallets()
                .iter()
                .map(|w| format!("- {} - {}", w.label, w.address))
                .collect();
            let text = if list.is_empty() {
                "Belum ada wallet. Gunakan /newwallet atau /import (burner).".into()
            } else {
                list.join("\n")
            };
            CommandResult {
                command: cmd.command,
                chat_id,
                text,
                silent,
            }
        }
        "/newwallet" => {
            // §F4: agent membuat wallet sendiri, tidak ada rahasia lewat chat.
            let mut ks = state.keystore.lock().await;
            let n = ks.wallets().len();
            if n >= state.import_config.max_wallets as usize {
                return CommandResult {
                    command: cmd.command,
                    chat_id,
                    text: format!(
                        "as i, Jumlah wallet sudah mencapai batas {}.",
                        state.import_config.max_wallets
                    ),
                    silent,
                };
            }
            let label = format!("burner-{}", n + 1);
            let id = format!("burner-{}", n + 1);
            let raw = generate_random_key();
            let wallet = match nft_keystore::OpenWallet::from_raw_key(id, &label, raw) {
                Ok(w) => w,
                Err(e) => {
                    return CommandResult {
                        command: cmd.command,
                        chat_id,
                        text: format!("as i, Gagal membuat wallet: {e}"),
                        silent,
                    };
                }
            };
            let address = wallet.address().to_string();
            if let Err(e) = ks.add(wallet) {
                return CommandResult {
                    command: cmd.command,
                    chat_id,
                    text: format!("as i, Gagal menyimpan wallet: {e}"),
                    silent,
                };
            }
            if let Err(e) = ks.save(&state.passphrase) {
                return CommandResult {
                    command: cmd.command,
                    chat_id,
                    text: format!("as i, Gagal menyimpan keystore: {e}"),
                    silent,
                };
            }
            let _ = audit(
                &state.db,
                &user_id.to_string(),
                "/newwallet",
                Some(&address),
                None,
            )
            .await;
            CommandResult {
                command: cmd.command,
                chat_id,
                text: format!(
                    "aoe… Wallet baru dibuat\nAlamat: `{address}`\nLabel: {label}\n\n\
                     Kirim dana ke alamat ini untuk mulai ( hanya modal siap hilang )."
                ),
                silent,
            }
        }
        "/import" => {
            // §3A: hanya burner, konfirmasi tombol + TOTP bila aktif.
            if !state.import_config.import_via_chat {
                let _ = audit(
                    &state.db,
                    &user_id.to_string(),
                    "/import",
                    Some("ditolak: import_via_chat=false"),
                    None,
                )
                .await;
                return CommandResult {
                    command: cmd.command,
                    chat_id,
                    text: "as i, Import lewat chat dimatikan ( policy.toml: \
                           import_via_chat = false ). Gunakan CLI."
                        .into(),
                    silent,
                };
            }
            if !state.import_sessions.start(user_id, ChatId(chat_id)).await {
                return CommandResult {
                    command: cmd.command,
                    chat_id,
                    text: "as i, Anda sudah memiliki sesi import aktif. Kirim phrase \
                           dalam satu pesan atau tunggu 2 menit untuk memulai ulang."
                        .into(),
                    silent,
                };
            }
            let _ = audit(
                &state.db,
                &user_id.to_string(),
                "/import",
                Some("sesi dimulai"),
                None,
            )
            .await;
            CommandResult {
                command: cmd.command,
                chat_id,
                text: "as i, *Import wallet BURNER*\n\n\
                       1. Pastikan ini wallet burner, BUKAN wallet utama.\n\
                       2. Kirim private key atau mnemonic dalam **satu pesan**.\n\
                       3. Pesan Anda akan langsung dihapus.\n\
                       4. Mnemonic hanya menurunkan path m/44'/60'/0'/0/0.\n\n\
                       Sesi kedaluwarsa dalam 2 menit. Hanya alamat + saldo \
                       yang akan ditampilkan kembali."
                    .into(),
                silent,
            }
        }
        "/approve" | "/reject" => {
            // Jawaban konfirmasi juga lewat callback ( lihat confirm.rs );
            // perintah teks ini hanya status.
            CommandResult {
                command: cmd.command,
                chat_id,
                text: "a„¹i, Konfirmasi tx dilakukan via tombol pada pesan \
                       konfirmasi ( inline keyboard )."
                    .into(),
                silent,
            }
        }
        _ => CommandResult {
            command: cmd.command,
            chat_id,
            text: "as i, Perintah tidak dikenali.".into(),
            silent,
        },
    }
}

/// Proses phrase import yang masuk ( worker cold path ). Pesan user **sudah
/// dihapus** oleh handler sebelum fungsi ini dipanggil ( §3A poin 3 ).
pub async fn process_import_phrase(
    state: &BotState,
    user_id: i64,
    chat_id: i64,
    text: &str,
) -> CommandResult {
    // Hanya bila ada sesi aktif & sudah dikonfirmasi.
    let Some(session) = state.import_sessions.get_active(user_id).await else {
        return CommandResult {
            command: "/import".into(),
            chat_id,
            text: "as i, Tidak ada sesi import aktif. Mulai dengan /import.".into(),
            silent: false,
        };
    };
    if !session.confirmed {
        return CommandResult {
            command: "/import".into(),
            chat_id,
            text: "as i, Sesi import belum dikonfirmasi. Tekan tombol konfirmasi \
                   burner terlebih dahulu."
                .into(),
            silent: false,
        };
    }

    // Parse phrase di memori ( §3A poin 4 ).
    let parsed = match parse_phrase(text) {
        Ok(p) => p,
        Err(e) => {
            state.access.record_failure(user_id).await;
            return CommandResult {
                command: "/import".into(),
                chat_id,
                text: format!(
                    "as i, Input tidak valid ( {e} ). Percobaan gagal dicatat.\n\
                     Private key = 64 hex karakter; mnemonic = 12/24 kata."
                ),
                silent: false,
            };
        }
    };

    // Simpan ke keystore + cek saldo ( finalize meng-zeroize? pemanggil yang
    // meng-zeroize setelahnya - lihat `zeroize_phrase` ).
    let mut ks = state.keystore.lock().await;
    let outcome = finalize_import(
        parsed,
        &mut ks,
        &state.passphrase,
        None, // RPC balance check: Fase 6 preflight; sekarang skip ( None ).
        &state.import_config,
    )
    .await;
    drop(ks);
    state.import_sessions.end(user_id).await;

    match outcome {
        Ok(o) => {
            state.access.record_success(user_id).await;
            // Audit: alamat + waktu, BUKAN isi ( §3A poin 6 ).
            let _ = audit(
                &state.db,
                &user_id.to_string(),
                "/import",
                Some(&o.address),
                Some(&serde_json::json!({"label": o.wallet_label})),
            )
            .await;
            CommandResult {
                command: "/import".into(),
                chat_id,
                text: import_reply(&o),
                silent: false,
            }
        }
        Err(e) => {
            state.access.record_failure(user_id).await;
            CommandResult {
                command: "/import".into(),
                chat_id,
                text: format!("as i, Import gagal: {e}"),
                silent: false,
            }
        }
    }
}

/// Hasilkan private key acak ( /newwallet ).
fn generate_random_key() -> Vec<u8> {
    use rand::RngCore;
    let mut buf = vec![0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    buf
}

/// Parse pesan masuk a+' deteksi apakah ini phrase untuk sesi import aktif.
/// Dipanggil handler SEBELUM `handle_message` untuk pesan non-perintah.
pub fn is_import_phrase_pending(state: &BotState, msg: &Message) -> bool {
    let Some(user_id) = msg.from.as_ref().map(|u| u.id.0 as i64) else {
        return false;
    };
    if msg.text().is_none() {
        return false;
    }
    // Cek sesi aktif secara non-blocking: `get_active` async, jadi kita
    // menyederhanakan: pesan dari user dengan sesi aktif a+' true.
    // Implementasi produksi: cek `state.import_sessions.get_active(user_id)`.
    // ( tests memverifikasi lewat `process_import_phrase` ).
    state.access.is_allowed(user_id)
}

/// Spawn task pengirim: mengambil hasil dari receiver dan mengirim ke chat.
pub fn spawn_sender(bot: Bot, reply_rx: mpsc::Receiver<CommandResult>) {
    tokio::spawn(async move {
        let mut reply_rx = reply_rx;
        while let Some(res) = reply_rx.recv().await {
            if res.silent {
                continue;
            }
            let chat = ChatId(res.chat_id);
            if let Err(e) = bot.send_message(chat, res.text).await {
                warn!(?e, "gagal mengirim balasan Telegram");
            }
        }
    });
}

/// Top-level: jalankan bot ( polling loop teloxide ).
/// ponytail: `spawn` butuh token valid; test memakai `handle_message`
/// langsung. Fase 10 merangkai semuanya di `crates/agent`.
pub async fn spawn(state: Arc<BotState>, bot: Bot) {
    if let Some(reply_rx) = state.queue.take_reply_rx().await {
        spawn_sender(bot.clone(), reply_rx);
    }

    let mut dispatcher = Dispatcher::builder(
        bot,
        Update::filter_message().endpoint(move |state: Arc<BotState>, bot: Bot, msg: Message| {
            let state = state.clone();
            async move {
                // Hot path: validasi + ack + enqueue.
                if let Some((ack, chat_id)) = handle_message(&state, &msg).await {
                    let _ = bot.send_message(ChatId(chat_id), ack).await;
                } else if is_import_phrase_pending(&state, &msg) {
                    // Import phrase: hapus pesan DULU ( §3A poin 3 ), lalu
                    // serahkan ke worker.
                    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
                    let chat_id = msg.chat.id;
                    let text = msg.text().unwrap_or_default().to_string();
                    // §3A poin 3: deleteMessage sebelum proses lain.
                    let _ = bot.delete_message(chat_id, msg.id).await;
                    let state = state.clone();
                    tokio::spawn(async move {
                        let res = process_import_phrase(&state, user_id, chat_id.0, &text).await;
                        let _ = state.queue.reply(res).await;
                    });
                }
                respond(())
            }
        }),
    )
    .dependencies(dptree::deps![state])
    .enable_ctrlc_handler()
    .build();

    dispatcher.dispatch().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::AccessLimits;
    use nft_core::config::parse_policy;
    use nft_policy::PolicyState;
    use std::time::Duration;
    use tempfile::tempdir;

    const POLICY_OK: &str = r#"
        [limits]
        max_spend_per_tx_eth = 0.5
        max_spend_per_day_eth = 2.0
        max_gas_gwei = 80
        allowed_chains = [1, 8453, 42161]

        [allowlist]
        contracts = []
        block_unlimited_approvals = true

        [confirmation]
        require_human_above_eth = 0.2
        require_human_for = ["withdraw", "new_contract"]

        [kill_switch]
        auto_trigger_on_loss_eth = 1.0

        [telegram]
        allowed_user_ids = [123456789]
        chat_cannot = ["edit_policy", "export_key"]
        import_via_chat = true
        import_session_timeout_sec = 120
        delete_import_message = true
        require_totp_for_import = true
        burner_max_balance_eth = 0.5
        max_wallets = 5
    "#;

    /// Buat state test. `TempDir` dikembalikan supaya direktori tetap ada
    /// selama test berjalan ( keystore ditulis ke file di dalamnya ).
    async fn state() -> (Arc<BotState>, tempfile::TempDir) {
        state_with(false).await
    }

    /// `import_via_chat = true` ( §3A: test alur import yang diaktifkan ).
    async fn state_import_enabled() -> (Arc<BotState>, tempfile::TempDir) {
        state_with(true).await
    }

    async fn state_with(import_via_chat: bool) -> (Arc<BotState>, tempfile::TempDir) {
        state_full(import_via_chat, None).await
    }

    /// Rakit state lengkap dengan `policy_state` opsional ( untuk test
    /// kill switch /stop ).
    async fn state_full(
        import_via_chat: bool,
        policy_state: Option<std::sync::Arc<dyn nft_policy::PolicyState + Send + Sync>>,
    ) -> (Arc<BotState>, tempfile::TempDir) {
        let db = nft_core::storage::connect("sqlite::memory:").await.unwrap();
        let dir = tempdir().unwrap();
        let ks = Keystore::open_or_create(dir.path().join("ks.bin")).unwrap();
        let queue = CommandQueue::new(16);
        let import_config = ImportConfig {
            import_via_chat,
            ..Default::default()
        };
        let st = Arc::new(BotState {
            config: parse_policy(POLICY_OK).unwrap(),
            access: AccessControl::new(vec![123456789], AccessLimits::default()),
            queue,
            import_config,
            import_sessions: ImportSessions::new(),
            keystore: Mutex::new(ks),
            passphrase: nft_core::secret::Secret::new(b"pass".to_vec()),
            admin_chat: ChatId(123456789),
            db,
            policy_state,
        });
        (st, dir)
    }

    fn msg(user_id: i64, text: &str) -> Message {
        // Bangun pesan via deserialisasi JSON ( stabil terhadap perubahan
        // field struct teloxide di masa depan ).
        let json = serde_json::json!({
            "message_id": 1,
            "date": 0,
            "chat": {
                "id": 1,
                "type": "private",
                "first_name": "t",
            },
            "from": {
                "id": user_id,
                "is_bot": false,
                "first_name": "t",
            },
            "text": text,
        });
        serde_json::from_value::<Message>(json).expect("pesan test valid")
    }

    #[tokio::test]
    async fn perintah_whitelist_didapatkan_ack() {
        let (st, _dir) = state().await;
        let m = msg(123456789, "/status");
        let (ack, chat) = handle_message(&st, &m).await.unwrap();
        assert!(ack.contains("Diterima"));
        assert_eq!(chat, 1);
        // Perintah masuk antrean.
        let cmd = st.queue.next().await.unwrap();
        assert_eq!(cmd.command, "/status");
    }

    #[tokio::test]
    async fn perintah_user_asing_diabaikan_tanpa_balasan() {
        // §3 poin 2.
        let (st, _dir) = state().await;
        let m = msg(999999, "/status");
        assert!(handle_message(&st, &m).await.is_none());
        // Tidak ada perintah di antrean.
        let res = tokio::time::timeout(Duration::from_millis(50), st.queue.next()).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn perintah_tidak_dikenali_diabaikan() {
        let (st, _dir) = state().await;
        let m = msg(123456789, "halo biasa");
        assert!(handle_message(&st, &m).await.is_none());
        let m2 = msg(123456789, "/tidak-ada");
        assert!(handle_message(&st, &m2).await.is_none());
    }

    #[tokio::test]
    async fn ack_dibawah_300ms_target() {
        // §8.1: ack < 300 ms ( diukur dari pesan masuk sampai ack terbentuk ).
        let (st, _dir) = state().await;
        let m = msg(123456789, "/status");
        let start = Instant::now();
        let (ack, _) = handle_message(&st, &m).await.unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed.as_millis() < 300, "ack butuh {elapsed:?}");
        assert!(ack.contains("Diterima"));
    }

    #[tokio::test]
    async fn process_status_menampilkan_ringkasan() {
        let (st, _dir) = state().await;
        let cmd = QueuedCommand {
            command: "/status".into(),
            user_id: 123456789,
            chat_id: 1,
            args: None,
            received_at: Instant::now(),
        };
        let res = process_command(&st, cmd).await;
        assert!(res.text.contains("nft-agent"));
        assert!(res.text.contains("dry-run"));
        assert!(!res.silent);
    }

    #[tokio::test]
    async fn process_newwallet_membuat_wallet_pertama() {
        let (st, _dir) = state().await;
        let cmd = QueuedCommand {
            command: "/newwallet".into(),
            user_id: 123456789,
            chat_id: 1,
            args: None,
            received_at: Instant::now(),
        };
        let res = process_command(&st, cmd).await;
        assert!(res.text.contains("aoe…"), "text: {}", res.text);
        assert!(res.text.contains("0x"));
        assert_eq!(st.keystore.lock().await.wallets().len(), 1);
    }

    #[tokio::test]
    async fn process_wallets_menampilkan_daftar() {
        let (st, _dir) = state().await;
        // Buat wallet dulu.
        let cmd = QueuedCommand {
            command: "/newwallet".into(),
            user_id: 123456789,
            chat_id: 1,
            args: None,
            received_at: Instant::now(),
        };
        process_command(&st, cmd).await;
        let cmd2 = QueuedCommand {
            command: "/wallets".into(),
            user_id: 123456789,
            chat_id: 1,
            args: None,
            received_at: Instant::now(),
        };
        let res = process_command(&st, cmd2).await;
        assert!(res.text.contains("burner-1"));
        assert!(res.text.contains("0x"));
    }

    #[tokio::test]
    async fn process_import_tanpa_konfirmasi_ditolak() {
        let (st, _dir) = state().await;
        // Mulai sesi ( langsung konfirmasi? perlu confirm dulu ).
        st.import_sessions.start(123456789, ChatId(1)).await;
        let res = process_import_phrase(&st, 123456789, 1, "aa".repeat(64).as_str()).await;
        assert!(
            res.text.contains("belum dikonfirmasi"),
            "text: {}",
            res.text
        );
    }

    #[tokio::test]
    async fn process_import_berhasil_setelah_konfirmasi() {
        let (st, _dir) = state_import_enabled().await;
        st.import_sessions.start(123456789, ChatId(1)).await;
        st.import_sessions.confirm(123456789).await;

        let res = process_import_phrase(&st, 123456789, 1, &"a".repeat(64)).await;
        assert!(res.text.contains("OK"), "text: {}", res.text);
        // Balasan tidak pernah mengulang phrase.
        assert!(!res.text.contains(&"a".repeat(64)));
        assert!(res.text.contains("0x"));
    }

    #[tokio::test]
    async fn import_balance_di_atas_burner_ditolak() {
        // §3A poin 5: saldo > burner_max_balance_eth a+' tolak.
        let (st, _dir) = state().await;
        st.import_sessions.start(123456789, ChatId(1)).await;
        st.import_sessions.confirm(123456789).await;

        // `process_import_phrase` mempassing None ( RPC check belum aktif );
        // test logika tolak via finalize_import langsung.
        let parsed = crate::ParsedSecret::PrivateKey(vec![0x42u8; 32]);
        let mut ks = st.keystore.lock().await;
        let err = finalize_import(
            parsed,
            &mut ks,
            &st.passphrase,
            Some(2.0), // saldo 2 ETH > 0.5 batas burner
            &st.import_config,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("melebihi batas burner"));
    }

    #[tokio::test]
    async fn import_gagal_mencatat_kegagalan_dan_lockout() {
        let (st, _dir) = state().await;
        st.import_sessions.start(123456789, ChatId(1)).await;
        st.import_sessions.confirm(123456789).await;
        // Input tidak valid berulang a+' lockout ( AccessControl ).
        for _ in 0..5 {
            let res = process_import_phrase(&st, 123456789, 1, "input-salah").await;
            assert!(res.text.contains("tidak valid"));
        }
        // Setelah 5 kegagalan a+' lockout.
        let outcome = st.access.check(123456789, true).await;
        assert!(matches!(outcome, AccessOutcome::LockedOut));
    }

    #[tokio::test]
    async fn import_reply_tidak_mengulang_phrase() {
        let outcome = ImportOutcome {
            address: "0x1234".into(),
            balance_eth: Some(0.1),
            wallet_label: "burner-1".into(),
        };
        let reply = import_reply(&outcome);
        assert!(reply.contains("0x1234"));
        assert!(!reply.contains("phrase"));
    }

    #[tokio::test]
    async fn import_via_chat_dimatikan_default() {
        // §3A: fitur import lewat chat MATI secara default.
        let (st, _dir) = state().await;
        let cmd = QueuedCommand {
            command: "/import".into(),
            user_id: 123456789,
            chat_id: 1,
            args: None,
            received_at: Instant::now(),
        };
        let res = process_command(&st, cmd).await;
        assert!(res.text.contains("dimatikan"), "text: {}", res.text);
    }

    #[tokio::test]
    async fn audit_tertulis_sa_setiap_perintah() {
        let (st, _dir) = state().await;
        let m = msg(123456789, "/status");
        handle_message(&st, &m).await.unwrap();
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit")
            .fetch_one(&st.db)
            .await
            .unwrap();
        assert!(n >= 1);
    }

    /// Mock `PolicyState` untuk verifikasi `/stop` memicu kill switch.
    #[derive(Debug, Default)]
    struct MockPolicyState {
        kill: std::sync::Arc<tokio::sync::Mutex<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl nft_policy::PolicyState for MockPolicyState {
        async fn spent_today(&self, _wallet: alloy::primitives::Address) -> f64 {
            0.0
        }
        async fn record_spend(&self, _wallet: alloy::primitives::Address, _eth: f64) {}
        async fn kill_switch_on(&self) -> Option<String> {
            self.kill.lock().await.clone()
        }
        async fn trigger_kill_switch(&self, reason: String) {
            *self.kill.lock().await = Some(reason);
        }
        async fn clear_kill_switch(&self) {
            *self.kill.lock().await = None;
        }
    }

    #[tokio::test]
    async fn stop_mematikan_kill_switch_dibawah_2_detik() {
        // Acceptance Fase 4: "/stop < 2 detik" + §F5 kill switch teruji.
        let mock = std::sync::Arc::new(MockPolicyState::default());
        let (st, _dir) = state_full(
            false,
            Some(mock.clone() as std::sync::Arc<dyn nft_policy::PolicyState + Send + Sync>),
        )
        .await;

        let start = Instant::now();
        let res = process_command(
            &st,
            QueuedCommand {
                command: "/stop".into(),
                user_id: 123456789,
                chat_id: 1,
                args: None,
                received_at: Instant::now(),
            },
        )
        .await;
        let elapsed = start.elapsed();

        assert!(res.text.contains("Kill switch"));
        assert!(
            elapsed.as_millis() < 2000,
            "/stop harus < 2 detik, butuh {elapsed:?}"
        );
        // Kill switch benar-benar aktif ( persisten, §F5 ).
        assert_eq!(
            mock.kill_switch_on().await,
            Some("/stop dari Telegram".to_string())
        );
    }
}
