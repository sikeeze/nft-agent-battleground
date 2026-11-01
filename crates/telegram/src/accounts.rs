//! Perintah `/accounts` (blueprint 3B, F12 - Social Vault).
//!
//! `- /accounts` — daftar akun + status sesi.
//! `- /accounts add <kind>` — mulai sesi browser terisolasi untuk login
//!   manual lewat remote browser ( Tailscale/noVNC ). Agent **tidak mengetik
//!   password user**; setelah user login ( termasuk 2FA ), sesi disimpan
//!   terenkripsi ( Argon2id + XChaCha20-Poly1305 ).
//! `- /accounts check` — jalankan `session_health` ( 3B poin 5 ).
//! `- /accounts remove <id>` — hapus akun + file sesi.
//!
//! **Aturan keamanan**:
//! - Tidak ada password yang disimpan ( 3B poin 1 ).
//! - Hanya `user_id` whitelist yang bisa memakai perintah ini.
//! - Isi pesan bebas rahasia: hanya id akun, kind, dan status.
//! - Captcha/2FA tidak di-bypass ( aturan keras #5 ): bila tantangan muncul,
//!   agent berhenti dan meminta user menyelesaikan via remote browser.

use nft_vault::{
    session_health, AccountKind, AccountMetaStore, HealthConfig, SessionHealthReport, Vault,
};

/// Hasil pemrosesan `/accounts ...` untuk dikirim ke chat.
#[derive(Debug, Clone)]
pub struct AccountsOutcome {
    /// Pesan siap kirim ( bebas rahasia ).
    pub text: String,
}

/// Daftar akun + status sesi ( `/accounts` tanpa argumen ).
pub async fn list_accounts(store: &AccountMetaStore) -> AccountsOutcome {
    let metas = match store.list().await {
        Ok(m) => m,
        Err(e) => {
            return AccountsOutcome {
                text: format!("[X] Gagal membaca daftar akun: {e}"),
            };
        }
    };
    if metas.is_empty() {
        return AccountsOutcome {
            text: "Belum ada akun sosial. Gunakan /accounts add <google|x|discord|custom> \
                   untuk memulai login ( dilakukan manual via remote browser )"
                .into(),
        };
    }
    let mut lines = Vec::with_capacity(metas.len());
    for m in metas {
        let verified = match m.last_verified {
            Some(t) => format!("diverifikasi {t}"),
            None => "belum diverifikasi".to_string(),
        };
        lines.push(format!("- {} ({}): {}", m.id, m.kind, verified));
    }
    AccountsOutcome {
        text: format!(
            "Akun sosial:\n{}\n\n/accounts check - cek kesehatan sesi",
            lines.join("\n")
        ),
    }
}

/// Mulai penambahan akun ( `/accounts add <kind>` ).
///
/// Agent **tidak meminta password di chat**. Yang terjadi: akun dibuatkan
/// profil browser isolasi + tautan remote browser ( 3B poin 2 ), lalu user
/// login sendiri. Sesi lalu disimpan terenkripsi.
pub async fn add_account(
    store: &AccountMetaStore,
    vault: &Vault,
    account_id: &str,
    kind_str: &str,
    profile_root: &str,
    passphrase: &nft_core::secret::Secret<Vec<u8>>,
) -> AccountsOutcome {
    let kind = match AccountKind::parse(kind_str) {
        Ok(k) => k,
        Err(e) => {
            return AccountsOutcome {
                text: format!("[X] {e}"),
            };
        }
    };
    let profile_dir = format!("{profile_root}/{account_id}");
    match store.add(account_id, kind, &profile_dir).await {
        Ok(_) => {}
        Err(e) => {
            return AccountsOutcome {
                text: format!("[X] Gagal menambah akun: {e}"),
            };
        }
    }
    // Buat file sesi kosong terenkripsi ( inisialisasi profil ).
    let empty = nft_vault::SessionState {
        cookies: Vec::new(),
        local_storage: Default::default(),
        user_agent: None,
    };
    if let Err(e) = vault.save_account(account_id, &empty, passphrase) {
        tracing::warn!(error = %e, account = account_id, "gagal inisialisasi file sesi");
        return AccountsOutcome {
            text: format!("[X] Gagal menyimpan sesi awal: {e}"),
        };
    }
    AccountsOutcome {
        text: format!(
            "[OK] Akun '{account_id}' ({}) dibuat.\n\
             Login dilakukan manual lewat remote browser ( Tailscale/noVNC ) - \
             agent tidak meminta password.\n\
             Setelah login ( termasuk 2FA ) selesai, sesi disimpan terenkripsi.",
            kind.as_str()
        ),
    }
}

/// Cek kesehatan semua sesi ( `/accounts check` ) - 3B poin 5.
pub async fn check_accounts(
    store: &AccountMetaStore,
    vault: &Vault,
    passphrase: &nft_core::secret::Secret<Vec<u8>>,
    config: &HealthConfig,
) -> AccountsOutcome {
    let reports = match session_health(store, vault, passphrase, config).await {
        Ok(r) => r,
        Err(e) => {
            return AccountsOutcome {
                text: format!("[X] Gagal mengecek sesi: {e}"),
            };
        }
    };
    if reports.is_empty() {
        return AccountsOutcome {
            text: "Tidak ada akun untuk dicek.".into(),
        };
    }
    let mut lines = Vec::with_capacity(reports.len());
    for r in &reports {
        lines.push(r.summarize());
    }
    let needing: Vec<&SessionHealthReport> = reports.iter().filter(|r| r.needs_relogin()).collect();
    let tail = if needing.is_empty() {
        "\n\n[OK] Semua sesi aktif."
    } else {
        "\n\n[!] Sesi bermasalah - login ulang sebelum hari mint ( bukan saat war )."
    };
    AccountsOutcome {
        text: format!("Cek sesi ( session_health ):\n{}{tail}", lines.join("\n")),
    }
}

/// Hapus akun ( `/accounts remove <id>` ).
pub async fn remove_account(
    store: &AccountMetaStore,
    vault: &Vault,
    account_id: &str,
) -> AccountsOutcome {
    let mut msgs = Vec::new();
    match vault.remove_account(account_id) {
        Ok(_) => msgs.push("[OK] File sesi dihapus.".into()),
        Err(e) => msgs.push(format!("file sesi: {e}")),
    }
    match store.remove(account_id).await {
        Ok(_) => msgs.push("[OK] Akun dihapus dari metadata.".into()),
        Err(e) => msgs.push(format!("metadata: {e}")),
    }
    AccountsOutcome {
        text: format!("Akun '{account_id}':\n{}", msgs.join("\n")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn pass() -> nft_core::secret::Secret<Vec<u8>> {
        nft_core::secret::Secret::new(b"vault-pass".to_vec())
    }

    async fn setup() -> (AccountMetaStore, Vault, tempfile::TempDir) {
        let pool = nft_core::storage::connect("sqlite::memory:").await.unwrap();
        let dir = tempdir().unwrap();
        let vault = Vault::open(dir.path().join("vault")).unwrap();
        (AccountMetaStore::new(pool), vault, dir)
    }

    #[tokio::test]
    async fn list_kosong_memberi_petunjuk() {
        let (store, _v, _d) = setup().await;
        let out = list_accounts(&store).await;
        assert!(out.text.contains("Belum ada akun"));
    }

    #[tokio::test]
    async fn list_menampilkan_akun() {
        let (store, _v, _d) = setup().await;
        store
            .add("x-1", AccountKind::X, "vault/profiles/x-1")
            .await
            .unwrap();
        let out = list_accounts(&store).await;
        assert!(out.text.contains("x-1"));
        assert!(out.text.contains("x"));
    }

    #[tokio::test]
    async fn add_akun_baru_berhasil() {
        let (store, vault, dir) = setup().await;
        let out = add_account(
            &store,
            &vault,
            "google-1",
            "google",
            dir.path().join("profiles").to_str().unwrap(),
            &pass(),
        )
        .await;
        assert!(out.text.contains("[OK]"));
        assert!(store.get("google-1").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn add_kind_tidak_dikenal_ditolak() {
        let (store, vault, dir) = setup().await;
        let out = add_account(
            &store,
            &vault,
            "a-1",
            "fbook",
            dir.path().join("profiles").to_str().unwrap(),
            &pass(),
        )
        .await;
        assert!(out.text.contains("[X]"));
    }

    #[tokio::test]
    async fn add_duplikat_ditolak() {
        let (store, vault, dir) = setup().await;
        let root = dir.path().join("profiles").to_str().unwrap().to_string();
        add_account(&store, &vault, "x-1", "x", &root, &pass()).await;
        let out = add_account(&store, &vault, "x-1", "x", &root, &pass()).await;
        assert!(out.text.contains("[X]"));
    }

    #[tokio::test]
    async fn check_akun_tanpa_sesi_menunjukkan_missing() {
        let (store, vault, _d) = setup().await;
        store.add("x-1", AccountKind::X, "p").await.unwrap();
        let out = check_accounts(&store, &vault, &pass(), &HealthConfig::default()).await;
        assert!(out.text.contains("file sesi tidak ada"));
        assert!(out.text.contains("login ulang"));
    }

    #[tokio::test]
    async fn check_tanpa_akun_memberi_pesan_jelas() {
        let (store, vault, _d) = setup().await;
        let out = check_accounts(&store, &vault, &pass(), &HealthConfig::default()).await;
        assert!(out.text.contains("Tidak ada akun"));
    }

    #[tokio::test]
    async fn remove_akun_menghapus_metadata_dan_file() {
        let (store, vault, _d) = setup().await;
        vault
            .save_account(
                "x-1",
                &nft_vault::SessionState {
                    cookies: Vec::new(),
                    local_storage: Default::default(),
                    user_agent: None,
                },
                &pass(),
            )
            .unwrap();
        store.add("x-1", AccountKind::X, "p").await.unwrap();
        let out = remove_account(&store, &vault, "x-1").await;
        assert!(out.text.contains("[OK]"));
        assert!(!vault.has_account("x-1"));
        assert!(store.get("x-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn remove_akun_tidak_ada_tetap_melapor() {
        let (store, vault, _d) = setup().await;
        let out = remove_account(&store, &vault, "hantu").await;
        assert!(out.text.contains("hantu"));
    }
}
