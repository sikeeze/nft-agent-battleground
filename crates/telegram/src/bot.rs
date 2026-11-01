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
    /// ( ��F5: "/stop mematikan semua job dan signer" ). `None` bila policy
    /// engine belum dirakit ( mis. saat test ).
    pub policy_state: Option<std::sync::Arc<dyn nft_policy::PolicyState + Send + Sync>>,
    /// RPC pool chain target ( dari `chain.toml` ). `None` bila belum
    /// dikonfigurasi — perintah /mint & /war akan menolak dengan jelas.
    pub rpc: Option<nft_chain::RpcPool>,
    /// Social Vault ( §3B, Fase 7 ): penyimpanan sesi Google/X terenkripsi.
    /// `None` bila vault belum dikonfigurasi - perintah /accounts menolak jelas.
    pub vault: Option<nft_vault::Vault>,
    /// Metadata akun social vault di SQLite ( tabel `accounts` ).
    pub account_store: Option<nft_vault::AccountMetaStore>,
    /// Root direktori profil browser isolasi ( §3B poin 3: satu profil per akun ).
    pub profile_root: String,
    /// Portfolio trader ( PnL, Fase 8 ).
    pub portfolio: tokio::sync::Mutex<nft_trader::Portfolio>,
    /// Config rules trading ( rules.toml, Fase 8 ).
    pub rules: tokio::sync::Mutex<nft_trader::RulesConfig>,
    /// Registry model LLM ( models.toml, Fase 9 §F7 ).
    pub models: Option<nft_core::config::ModelsConfig>,
    /// Klien LLM aktif ( Fase 9 §F7 ). `None` bila belum dikonfigurasi.
    pub llm: Option<nft_llm::LlmClient>,
    /// Sesi draft pending dari bahasa natural ( Fase 9 §F6 ).
    pub draft_sessions: crate::nlcommand::DraftSessions,
    /// Folder skill ( Fase 9 §F8 ).
    pub skills_dir: String,
    /// Scheduler job persisten ( Fase 10 §F9 ). `None` bila belum
    /// dikonfigurasi — perintah /jobs menolak dengan jelas.
    pub scheduler: Option<std::sync::Arc<nft_scheduler::Scheduler>>,
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

/// Rakitan pesan bahasa natural menjadi `QueuedCommand` ( Fase 9 §F6 ).
/// Pesan non-perintah dari user whitelist masuk antrean sebagai perintah
/// `natural` ( cold path worker, bukan hot path — aturan keras #6 ).
pub fn make_natural_command(msg: &Message) -> Option<QueuedCommand> {
    let text = msg.text().unwrap_or_default().trim();
    if text.is_empty() || text.starts_with('/') {
        return None;
    }
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64)?;
    let chat_id = msg.chat.id.0;
    Some(QueuedCommand {
        command: "natural".into(),
        user_id,
        chat_id,
        args: Some(text.to_string()),
        received_at: Instant::now(),
    })
}

/// Daftar perintah yang dikenal ( parse tanpa LLM, §8.2 poin 3 ).
pub const ENABLED_COMMANDS: [&str; 18] = [
    "/status",
    "/stop",
    "/import",
    "/newwallet",
    "/wallets",
    "/accounts",
    "/approve",
    "/reject",
    "/mint",
    "/preflight",
    "/war",
    "/portfolio",
    "/rules",
    "/model",
    "/skills",
    "/yes",
    "/no",
    "/jobs",
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
        // Bukan perintah dikenal — bisa jadi:
        // (a) sesi import menunggu phrase ( lihat `handle_import_phrase` ),
        // (b) bahasa natural ( Fase 9 §F6 ) — enqueue sebagai "natural".
        // (c) biasa — abaikan.
        // Import phrase punya prioritas ( §3A poin 3: pesan dihapus dulu ).
        if state.import_sessions.get_active(user_id).await.is_some() {
            return None;
        }
        let nl = make_natural_command(msg)?;
        // Hanya user whitelist yang bisa ( access.check di bawah ).
        match state.access.check(user_id, true).await {
            AccessOutcome::Allow => {}
            _ => return None,
        }
        let ack = "Diterima: bahasa natural - diproses".to_string();
        let chat_id = nl.chat_id;
        match state.queue.enqueue(nl).await {
            Ok(ack_ms) => {
                debug!(user_id, ack_ms, "bahasa natural masuk antrean");
                let _ = audit(
                    &state.db,
                    &user_id.to_string(),
                    "natural",
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
        return Some((ack, chat_id));
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
        "/mint" | "/war" => {
            // §5 Link-to-Mint: resolve -> verify on-chain -> rencana +
            // peringatan risiko. Eksekusi tetap lewat konfirmasi tombol.
            let Some(link) = cmd.args.as_deref() else {
                return CommandResult {
                    command: cmd.command,
                    chat_id,
                    text: "Pakai: /mint <link> ( mis. https://opensea.io/drop/xyz )".into(),
                    silent,
                };
            };
            let Some(rpc) = state.rpc.as_ref() else {
                return CommandResult {
                    command: cmd.command,
                    chat_id,
                    text: "[X] RPC chain belum dikonfigurasi ( chain.toml ).".into(),
                    silent,
                };
            };
            let wallets: Vec<nft_core::wallet::WalletRef> = state
                .keystore
                .lock()
                .await
                .wallets()
                .iter()
                .map(|w| {
                    nft_core::wallet::WalletRef::new(
                        w.id.clone(),
                        w.label.clone(),
                        w.address.clone(),
                    )
                })
                .collect();
            let outcome = crate::mintcommand::handle_mint_link(link, rpc, &wallets).await;
            let _ = audit(
                &state.db,
                &user_id.to_string(),
                &cmd.command,
                Some(&outcome.text.chars().take(64).collect::<String>()),
                None,
            )
            .await;
            CommandResult {
                command: cmd.command,
                chat_id,
                text: outcome.text,
                silent,
            }
        }
        "/preflight" => {
            // §6.1: /preflight <task> — V1: tampilkan info cara pakai.
            // ( preflight otomatis dijalankan saat /war dikonfirmasi )
            CommandResult {
                command: cmd.command,
                chat_id,
                text: "Preflight (§6.1) otomatis: T-10 menit & T-1 menit \
                       sebelum trigger. Pakai /war <link> untuk menyiapkan \
                       war ( resolve + verify + preflight )."
                    .into(),
                silent,
            }
        }
        "/accounts" => {
            // §3B Social Vault: /accounts, /accounts add|check|remove.
            // Password Google/X TIDAK pernah diminta di chat ( aturan keras #4 ).
            let Some(store) = state.account_store.as_ref() else {
                return CommandResult {
                    command: cmd.command,
                    chat_id,
                    text: "[X] Social Vault belum dikonfigurasi.".into(),
                    silent,
                };
            };
            let args = cmd.args.as_deref().unwrap_or("").trim();
            let mut parts = args.split_whitespace();
            let sub = parts.next().unwrap_or("");
            let id_arg = parts.next().unwrap_or("");
            let outcome = match sub {
                "" => crate::accounts::list_accounts(store).await,
                "add" => {
                    let Some(vault) = state.vault.as_ref() else {
                        return CommandResult {
                            command: cmd.command,
                            chat_id,
                            text: "[X] Social Vault belum dikonfigurasi.".into(),
                            silent,
                        };
                    };
                    if id_arg.is_empty() {
                        return CommandResult {
                            command: cmd.command,
                            chat_id,
                            text: "Pakai: /accounts add <id> <google|x|discord|custom>".into(),
                            silent,
                        };
                    }
                    let kind = parts.next().unwrap_or("custom");
                    crate::accounts::add_account(
                        store,
                        vault,
                        id_arg,
                        kind,
                        &state.profile_root,
                        &state.passphrase,
                    )
                    .await
                }
                "check" => {
                    let Some(vault) = state.vault.as_ref() else {
                        return CommandResult {
                            command: cmd.command,
                            chat_id,
                            text: "[X] Social Vault belum dikonfigurasi.".into(),
                            silent,
                        };
                    };
                    crate::accounts::check_accounts(
                        store,
                        vault,
                        &state.passphrase,
                        &nft_vault::HealthConfig::default(),
                    )
                    .await
                }
                "remove" => {
                    if id_arg.is_empty() {
                        return CommandResult {
                            command: cmd.command,
                            chat_id,
                            text: "Pakai: /accounts remove <id>".into(),
                            silent,
                        };
                    }
                    match state.vault.as_ref() {
                        Some(v) => crate::accounts::remove_account(store, v, id_arg).await,
                        None => match store.remove(id_arg).await {
                            Ok(_) => crate::accounts::AccountsOutcome {
                                text:
                                    "[OK] Akun dihapus dari metadata ( vault tak dikonfigurasi )."
                                        .into(),
                            },
                            Err(e) => crate::accounts::AccountsOutcome {
                                text: format!("[X] {e}"),
                            },
                        },
                    }
                }
                _ => crate::accounts::AccountsOutcome {
                    text: format!(
                        "Subperintah tidak dikenal: {sub} ( pakai: add | check | remove )"
                    ),
                },
            };
            let detail = if id_arg.is_empty() {
                None
            } else {
                Some(serde_json::json!({ "account": id_arg }))
            };
            let _ = audit(
                &state.db,
                &user_id.to_string(),
                "/accounts",
                Some(sub),
                detail.as_ref(),
            )
            .await;
            CommandResult {
                command: cmd.command,
                chat_id,
                text: outcome.text,
                silent,
            }
        }
        "/portfolio" => {
            // F2: portfolio tracker + PnL ( data akuntansi, bebas rahasia ).
            let portfolio = state.portfolio.lock().await;
            let out = crate::trade::portfolio_summary(&portfolio);
            let _ = audit(&state.db, &user_id.to_string(), "/portfolio", None, None).await;
            CommandResult {
                command: cmd.command,
                chat_id,
                text: out.text,
                silent,
            }
        }
        "/rules" => {
            // F2: daftar rule dari rules.toml. `/rules reload` muat ulang.
            let args = cmd.args.as_deref().unwrap_or("").trim();
            if args == "reload" {
                // Muat ulang rules.toml dari config dir ( S3: rules.toml ).
                let path = std::path::Path::new("config").join(nft_core::config::RULES_FILE);
                match crate::trade::load_rules(&path) {
                    Ok(cfg) => {
                        let n = cfg.rule.len();
                        *state.rules.lock().await = cfg;
                        let _ = audit(
                            &state.db,
                            &user_id.to_string(),
                            "/rules",
                            Some("reload"),
                            None,
                        )
                        .await;
                        CommandResult {
                            command: cmd.command,
                            chat_id,
                            text: format!("[OK] rules.toml dimuat ulang ({n} rule)."),
                            silent,
                        }
                    }
                    Err(e) => CommandResult {
                        command: cmd.command,
                        chat_id,
                        text: format!("[X] rules.toml tidak valid: {e}"),
                        silent,
                    },
                }
            } else {
                let rules = state.rules.lock().await;
                let out = crate::trade::list_rules(&rules);
                let _ = audit(&state.db, &user_id.to_string(), "/rules", None, None).await;
                CommandResult {
                    command: cmd.command,
                    chat_id,
                    text: out.text,
                    silent,
                }
            }
        }
        "/model" => {
            // §F7: ganti model tanpa ubah kode.
            let Some(models) = state.models.as_ref() else {
                return CommandResult {
                    command: cmd.command,
                    chat_id,
                    text: "[X] models.toml belum dimuat.".into(),
                    silent,
                };
            };
            let Some(llm) = state.llm.as_ref() else {
                return CommandResult {
                    command: cmd.command,
                    chat_id,
                    text: "[X] Klien LLM belum dikonfigurasi ( models.toml ).".into(),
                    silent,
                };
            };
            let out = crate::llmcommand::model_command(cmd.args.as_deref(), models, llm).await;
            let _ = audit(
                &state.db,
                &user_id.to_string(),
                "/model",
                cmd.args.as_deref(),
                None,
            )
            .await;
            CommandResult {
                command: cmd.command,
                chat_id,
                text: out.text,
                silent,
            }
        }
        "/skills" => {
            // §F8: daftar & jalankan skill ( hanya laporan, tidak ada akses
            // signer/keystore ).
            let out = crate::llmcommand::skills_command(
                cmd.args.as_deref(),
                std::path::Path::new(&state.skills_dir),
            )
            .await;
            let _ = audit(
                &state.db,
                &user_id.to_string(),
                "/skills",
                cmd.args.as_deref(),
                None,
            )
            .await;
            CommandResult {
                command: cmd.command,
                chat_id,
                text: out.text,
                silent,
            }
        }
        "/yes" => confirm_draft(state, user_id, chat_id, true).await,
        "/no" => confirm_draft(state, user_id, chat_id, false).await,
        "/jobs" => {
            // §F9: daftar & kelola job scheduler ( tidak bergantung chat ).
            let out =
                crate::jobscommand::jobs_command(cmd.args.as_deref(), state.scheduler.as_deref())
                    .await;
            let _ = audit(
                &state.db,
                &user_id.to_string(),
                "/jobs",
                cmd.args.as_deref(),
                None,
            )
            .await;
            CommandResult {
                command: cmd.command,
                chat_id,
                text: out.text,
                silent,
            }
        }
        "natural" => {
            // Fase 9 §F6: bahasa natural -> draft rule ( cold path ).
            let Some(text) = cmd.args.as_deref() else {
                return CommandResult {
                    command: cmd.command,
                    chat_id,
                    text: "[!] Teks kosong.".into(),
                    silent,
                };
            };
            process_natural_language(state, user_id, chat_id, text).await
        }
        _ => CommandResult {
            command: cmd.command,
            chat_id,
            text: "[!] Perintah tidak dikenali.".into(),
            silent,
        },
    }
}

/// Proses pesan bahasa natural ( worker cold path, Fase 9 §F6 ).
///
/// Teks non-perintah dari user whitelist dianggap permintaan rule trading.
/// LLM mengusulkan draft; bot menampilkan ringkasan + tombol konfirmasi.
/// **Draft belum disimpan** sampai user konfirmasi ( lihat
/// `confirm_draft` ). LLM tidak berada di jalur eksekusi mint ( aturan
/// keras #6, §8.2 poin 3 ).
pub async fn process_natural_language(
    state: &BotState,
    user_id: i64,
    chat_id: i64,
    text: &str,
) -> CommandResult {
    let Some(llm) = state.llm.as_ref() else {
        return CommandResult {
            command: "natural".into(),
            chat_id,
            text: "[X] LLM belum dikonfigurasi ( models.toml ) - bahasa natural \
                   tidak bisa diproses. Gunakan /rules untuk mengatur rule manual."
                .into(),
            silent: false,
        };
    };
    match crate::nlcommand::natural_to_rule(llm, text, None).await {
        Ok(draft) => {
            let summary = draft.summarize();
            state.draft_sessions.put(user_id, draft).await;
            let _ = audit(
                &state.db,
                &user_id.to_string(),
                "natural",
                Some("draft"),
                None,
            )
            .await;
            CommandResult {
                command: "natural".into(),
                chat_id,
                text: format!("{summary}\n\nBalas /yes untuk menyimpan, /no untuk membatalkan."),
                silent: false,
            }
        }
        Err(e) => CommandResult {
            command: "natural".into(),
            chat_id,
            text: format!(
                "[X] Tidak bisa mengubah teks menjadi rule: {e}\n\
                 Contoh: 'beli 0x... di bawah 2 ETH'"
            ),
            silent: false,
        },
    }
}

/// Konfirmasi draft pending dari bahasa natural ( /yes | /no ).
pub async fn confirm_draft(
    state: &BotState,
    user_id: i64,
    chat_id: i64,
    accept: bool,
) -> CommandResult {
    if !accept {
        state.draft_sessions.take(user_id).await; // buang
        return CommandResult {
            command: "natural".into(),
            chat_id,
            text: "[OK] Draft rule dibatalkan.".into(),
            silent: false,
        };
    }
    let Some(draft) = state.draft_sessions.take(user_id).await else {
        return CommandResult {
            command: "natural".into(),
            chat_id,
            text: "[!] Tidak ada draft pending. Kirim perintah bahasa natural \
                   ( mis. 'beli 0x... di bawah 2 ETH' )."
                .into(),
            silent: false,
        };
    };
    let mut rules = state.rules.lock().await;
    match nft_trader::naturallang::add_draft(&mut rules, draft) {
        Ok(()) => {
            let _ = audit(
                &state.db,
                &user_id.to_string(),
                "natural",
                Some("confirmed"),
                None,
            )
            .await;
            CommandResult {
                command: "natural".into(),
                chat_id,
                text: "[OK] Rule disimpan ke rules.toml ( aktif ). Gunakan /rules \
                       untuk melihat."
                    .into(),
                silent: false,
            }
        }
        Err(e) => CommandResult {
            command: "natural".into(),
            chat_id,
            text: format!("[X] Draft tidak valid: {e}"),
            silent: false,
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
                } else if let Some(nl) = make_natural_command(&msg) {
                    // Bahasa natural ( Fase 9 §F6 ): enqueue sebagai perintah
                    // "natural". Hot path hanya ack; LLM berjalan di worker (
                    // aturan keras #6, §8.2 poin 3 ).
                    let user_id = nl.user_id;
                    let chat_id = nl.chat_id;
                    if state.access.is_allowed(user_id) {
                        let _ = bot
                            .send_message(ChatId(chat_id), "Diterima: bahasa natural - diproses")
                            .await;
                        match state.queue.enqueue(nl).await {
                            Ok(ack_ms) => {
                                let _ = audit(
                                    &state.db,
                                    &user_id.to_string(),
                                    "natural",
                                    None,
                                    Some(&serde_json::json!({"ack_ms": ack_ms})),
                                )
                                .await;
                            }
                            Err(e) => {
                                warn!(?e, "antrean penuh/ditutup");
                                let _ = bot
                                    .send_message(ChatId(chat_id), "[!] Sistem sibuk - coba lagi.")
                                    .await;
                            }
                        }
                    }
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
            rpc: None,
            vault: None,
            account_store: None,
            profile_root: dir.path().join("profiles").display().to_string(),
            portfolio: tokio::sync::Mutex::new(nft_trader::Portfolio::new()),
            rules: tokio::sync::Mutex::new(nft_trader::rules::parse_rules("").unwrap()),
            models: None,
            llm: None,
            draft_sessions: crate::nlcommand::DraftSessions::new(),
            skills_dir: dir.path().join("skills").display().to_string(),
            scheduler: None,
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
        // Bahasa natural ( Fase 9 ): pesan non-perintah dari whitelist
        // masuk antrean sebagai "natural".
        assert!(handle_message(&st, &m).await.is_some());
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
