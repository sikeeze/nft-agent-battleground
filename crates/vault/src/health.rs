//! `session_health` -- pantau kedaluwarsa sesi (blueprint 3B poin 5).
//!
//! Job ini mengecek sesi tiap beberapa jam. Kalau sesi mati/akan mati,
//! kirim notifikasi minta login ulang **sebelum** hari mint, bukan saat
//! war ( 3B: "bot mengirim notifikasi minta login ulang sebelum hari mint" ).
//!
//! Aturan: tidak ada solver captcha -- jika sesi mati, agent hanya
//! melaporkan dan menunggu login manual via remote browser ( 3B poin 6 ).

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::meta::{AccountMeta, MetaError};
use crate::{AccountKind, Vault};

/// Ambang batas default: sesi dianggap "akan mati" dalam 24 jam ke depan.
pub const DEFAULT_EXPIRING_WITHIN_HOURS: i64 = 24;
/// Interval pengecekan default: setiap 6 jam.
pub const DEFAULT_CHECK_INTERVAL_HOURS: i64 = 6;

#[derive(Debug, Error)]
pub enum HealthError {
    #[error("error meta store: {0}")]
    Meta(#[from] MetaError),

    #[error("error vault: {0}")]
    Vault(#[from] crate::VaultError),
}

/// Status satu sesi akun.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    /// `last_verified` baru ( dalam `max_age_hours` ) dan ada file vault.
    Healthy,
    /// Sesi ada tapi belum pernah diverifikasi / verifikasi sudah lama.
    Stale,
    /// Metadata ada tapi file vault hilang.
    Missing,
    /// Cookie inti kedaluwarsa dalam `expiring_within_hours` ke depan.
    ExpiringSoon,
    /// Cookie inti sudah kedaluwarsa --> butuh login ulang.
    Expired,
}

impl SessionStatus {
    /// 3B poin 5: yang butuh notifikasi login ulang.
    pub fn needs_relogin(&self) -> bool {
        matches!(self, Self::Missing | Self::ExpiringSoon | Self::Expired)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Stale => "stale",
            Self::Missing => "missing",
            Self::ExpiringSoon => "expiring_soon",
            Self::Expired => "expired",
        }
    }
}

/// Hasil cek satu akun.
#[derive(Debug, Clone)]
pub struct SessionHealthReport {
    pub account_id: String,
    pub kind: AccountKind,
    pub status: SessionStatus,
    /// Pesan siap tampilkan ( bebas rahasia ).
    pub message: String,
}

impl SessionHealthReport {
    /// Apakah laporan ini perlu notifikasi login ulang.
    pub fn needs_relogin(&self) -> bool {
        self.status.needs_relogin()
    }

    /// Ringkasan baris untuk pesan Telegram/WhatsApp ( bebas rahasia ).
    pub fn summarize(&self) -> String {
        format!(
            "- {} ({}): {}",
            self.account_id,
            self.kind.as_str(),
            self.message
        )
    }
}

/// Konfigurasi `session_health`.
#[derive(Debug, Clone, Copy)]
pub struct HealthConfig {
    /// Sesi dianggap usang bila `last_verified` lebih lama dari ini.
    pub max_age_hours: i64,
    /// Cookie kedaluwarsa dalam rentang ini = `ExpiringSoon`.
    pub expiring_within_hours: i64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            max_age_hours: DEFAULT_CHECK_INTERVAL_HOURS * 2,
            expiring_within_hours: DEFAULT_EXPIRING_WITHIN_HOURS,
        }
    }
}

/// Cek kesehatan satu akun.
///
/// - `meta`: metadata akun ( dari SQLite ).
/// - `vault`: Social Vault ( file sesi terenkripsi ).
/// - `passphrase`: passphrase vault ( dari env, bukan chat ).
///
/// **Tidak pernah mengembalikan isi sesi** -- hanya status.
pub async fn check_account(
    meta: &AccountMeta,
    vault: &Vault,
    passphrase: &nft_core::secret::Secret<Vec<u8>>,
    config: &HealthConfig,
) -> Result<SessionHealthReport, HealthError> {
    let kind = meta.kind_enum()?;
    let id = meta.id.clone();

    if !vault.has_account(&id) {
        return Ok(SessionHealthReport {
            account_id: id,
            kind,
            status: SessionStatus::Missing,
            message: "file sesi tidak ada - login ulang lewat /accounts add".into(),
        });
    }

    // Buka sesi ( dekripsi ) untuk periksa cookie.
    let acc = vault.open_account_with(meta, passphrase)?;
    let state: crate::SessionState = serde_json::from_slice(acc.session_bytes())
        .map_err(|e| HealthError::Vault(crate::VaultError::InvalidState(e)))?;

    // Cookie inti kedaluwarsa?
    let now_ts = Utc::now().timestamp();
    let soon_ts = now_ts + config.expiring_within_hours * 3600;
    let expire_limit = now_ts - config.max_age_hours * 3600;

    let mut expired = false;
    let mut expiring = false;
    for c in &state.cookies {
        if c.name.contains("session") || c.name.contains("auth") || c.name.contains("token") {
            match c.expires_at {
                Some(t) if t <= now_ts => expired = true,
                Some(t) if t <= soon_ts => expiring = true,
                _ => {}
            }
        }
    }

    if expired {
        return Ok(SessionHealthReport {
            account_id: id,
            kind,
            status: SessionStatus::Expired,
            message: "sesi sudah kedaluwarsa - login ulang diperlukan".into(),
        });
    }
    if expiring {
        return Ok(SessionHealthReport {
            account_id: id,
            kind,
            status: SessionStatus::ExpiringSoon,
            message: format!(
                "sesi akan kedaluwarsa dalam <{} jam - login ulang sebelum hari mint",
                config.expiring_within_hours
            ),
        });
    }

    // last_verified terbaru?
    let stale = match meta.last_verified {
        Some(t) => t < expire_limit,
        None => true,
    };
    if stale {
        Ok(SessionHealthReport {
            account_id: id,
            kind,
            status: SessionStatus::Stale,
            message: "sesi belum diverifikasi dalam selang waktu lama".into(),
        })
    } else {
        Ok(SessionHealthReport {
            account_id: id,
            kind,
            status: SessionStatus::Healthy,
            message: "sesi aktif".into(),
        })
    }
}

/// Cek semua akun di meta store. Akun yang sehat dikembalikan juga; pemanggil
/// bisa saring dengan `.needs_relogin()`.
pub async fn session_health(
    store: &crate::meta::AccountMetaStore,
    vault: &Vault,
    passphrase: &nft_core::secret::Secret<Vec<u8>>,
    config: &HealthConfig,
) -> Result<Vec<SessionHealthReport>, HealthError> {
    let metas = store.list().await?;
    let mut reports = Vec::with_capacity(metas.len());
    for m in metas {
        // Kind tidak dikenali di DB lama -> lewat sebagai custom.
        let m = if m.kind_enum().is_err() {
            AccountMeta {
                kind: AccountKind::Custom.as_str().into(),
                ..m
            }
        } else {
            m
        };
        match check_account(&m, vault, passphrase, config).await {
            Ok(r) => reports.push(r),
            Err(e) => {
                tracing::warn!(account = %m.id, error = %e, "gagal cek sesi akun");
                reports.push(SessionHealthReport {
                    account_id: m.id.clone(),
                    kind: m.kind_enum().unwrap_or(AccountKind::Custom),
                    status: SessionStatus::Missing,
                    message: format!("error saat mengecek sesi: {e}"),
                });
            }
        }
    }
    Ok(reports)
}

/// Waktu `last_verified` terbaru sebagai `DateTime` ( untuk tampilan ).
pub fn last_verified_at(meta: &AccountMeta) -> Option<DateTime<Utc>> {
    meta.last_verified
        .and_then(|t| DateTime::<Utc>::from_timestamp(t, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::AccountMetaStore;
    use tempfile::tempdir;

    fn pass() -> nft_core::secret::Secret<Vec<u8>> {
        nft_core::secret::Secret::new(b"vault-pass".to_vec())
    }

    fn state_with_cookie(expires_at: Option<i64>) -> crate::SessionState {
        crate::SessionState {
            cookies: vec![crate::VaultCookie {
                name: "session".into(),
                value: "v".into(),
                domain: ".example.com".into(),
                path: "/".into(),
                expires_at,
                secure: true,
                http_only: true,
            }],
            local_storage: Default::default(),
            user_agent: None,
        }
    }

    async fn setup() -> (AccountMetaStore, Vault, tempfile::TempDir) {
        let pool = nft_core::storage::connect("sqlite::memory:").await.unwrap();
        let dir = tempdir().unwrap();
        let vault = Vault::open(dir.path().join("vault")).unwrap();
        (AccountMetaStore::new(pool), vault, dir)
    }

    fn meta(id: &str, kind: AccountKind, last_verified: Option<i64>) -> AccountMeta {
        AccountMeta {
            id: id.into(),
            kind: kind.as_str().into(),
            profile_dir: format!("vault/profiles/{id}"),
            last_verified,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[tokio::test]
    async fn akun_tanpa_file_vault_missing() {
        let (store, vault, _d) = setup().await;
        store.add("x-1", AccountKind::X, "p").await.unwrap();
        let m = store.get("x-1").await.unwrap().unwrap();
        let r = check_account(&m, &vault, &pass(), &HealthConfig::default())
            .await
            .unwrap();
        assert_eq!(r.status, SessionStatus::Missing);
        assert!(r.needs_relogin());
    }

    #[tokio::test]
    async fn sesi_sehat_dengan_verifikasi_baru() {
        let (store, vault, _d) = setup().await;
        vault
            .save_account(
                "x-1",
                &state_with_cookie(Some(Utc::now().timestamp() + 7 * 86400)),
                &pass(),
            )
            .unwrap();
        store.add("x-1", AccountKind::X, "p").await.unwrap();
        store.touch_verified("x-1").await.unwrap();
        let m = store.get("x-1").await.unwrap().unwrap();
        let r = check_account(&m, &vault, &pass(), &HealthConfig::default())
            .await
            .unwrap();
        assert_eq!(r.status, SessionStatus::Healthy);
        assert!(!r.needs_relogin());
    }

    #[tokio::test]
    async fn cookie_kedaluwarsa_expired() {
        let (store, vault, _d) = setup().await;
        vault
            .save_account(
                "x-1",
                &state_with_cookie(Some(Utc::now().timestamp() - 3600)),
                &pass(),
            )
            .unwrap();
        store.add("x-1", AccountKind::X, "p").await.unwrap();
        store.touch_verified("x-1").await.unwrap();
        let m = store.get("x-1").await.unwrap().unwrap();
        let r = check_account(&m, &vault, &pass(), &HealthConfig::default())
            .await
            .unwrap();
        assert_eq!(r.status, SessionStatus::Expired);
        assert!(r.needs_relogin());
    }

    #[tokio::test]
    async fn cookie_akan_kedaluwarsa_expiring_soon() {
        let (store, vault, _d) = setup().await;
        let within = Utc::now().timestamp() + 2 * 3600; // 2 jam < 24 jam
        vault
            .save_account("x-1", &state_with_cookie(Some(within)), &pass())
            .unwrap();
        store.add("x-1", AccountKind::X, "p").await.unwrap();
        store.touch_verified("x-1").await.unwrap();
        let m = store.get("x-1").await.unwrap().unwrap();
        let r = check_account(&m, &vault, &pass(), &HealthConfig::default())
            .await
            .unwrap();
        assert_eq!(r.status, SessionStatus::ExpiringSoon);
        assert!(r.needs_relogin());
    }

    #[tokio::test]
    async fn verifikasi_lama_stale() {
        let (store, vault, _d) = setup().await;
        vault
            .save_account(
                "x-1",
                &state_with_cookie(Some(Utc::now().timestamp() + 7 * 86400)),
                &pass(),
            )
            .unwrap();
        store.add("x-1", AccountKind::X, "p").await.unwrap();
        let m = store.get("x-1").await.unwrap().unwrap();
        // last_verified None -> Stale
        let r = check_account(&m, &vault, &pass(), &HealthConfig::default())
            .await
            .unwrap();
        assert_eq!(r.status, SessionStatus::Stale);
        assert!(!r.needs_relogin());
    }

    #[tokio::test]
    async fn session_health_mengumpulkan_semua_akun() {
        let (store, vault, _d) = setup().await;
        // akun sehat
        vault
            .save_account(
                "a-1",
                &state_with_cookie(Some(Utc::now().timestamp() + 7 * 86400)),
                &pass(),
            )
            .unwrap();
        store.add("a-1", AccountKind::Google, "p").await.unwrap();
        store.touch_verified("a-1").await.unwrap();
        // akun tanpa file
        store.add("b-2", AccountKind::X, "p").await.unwrap();

        let reports = session_health(&store, &vault, &pass(), &HealthConfig::default())
            .await
            .unwrap();
        assert_eq!(reports.len(), 2);
        let needing: Vec<_> = reports.iter().filter(|r| r.needs_relogin()).collect();
        assert_eq!(needing.len(), 1);
        assert_eq!(needing[0].account_id, "b-2");
    }

    #[tokio::test]
    async fn ringkasan_bebas_rahasia() {
        let r = SessionHealthReport {
            account_id: "x-1".into(),
            kind: AccountKind::X,
            status: SessionStatus::Expired,
            message: "sesi sudah kedaluwarsa - login ulang diperlukan".into(),
        };
        let s = r.summarize();
        assert!(s.contains("x-1"));
        assert!(s.contains("x"));
        assert!(!s.contains("v"));
    }

    #[tokio::test]
    async fn kind_tidak_dikenal_dianggap_custom() {
        let (store, vault, _d) = setup().await;
        // Sisipkan baris dengan kind tidak standar langsung ke DB.
        sqlx::query(
            "INSERT INTO accounts (id, kind, profile_dir, created_at, updated_at) \
             VALUES (?, 'facebook', 'p', 0, 0)",
        )
        .bind("f-1")
        .execute(store.pool())
        .await
        .unwrap();
        let reports = session_health(&store, &vault, &pass(), &HealthConfig::default())
            .await
            .unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].kind, AccountKind::Custom);
    }

    #[test]
    fn status_needs_relogin_hanya_yang_bermasalah() {
        assert!(!SessionStatus::Healthy.needs_relogin());
        assert!(!SessionStatus::Stale.needs_relogin());
        assert!(SessionStatus::Missing.needs_relogin());
        assert!(SessionStatus::ExpiringSoon.needs_relogin());
        assert!(SessionStatus::Expired.needs_relogin());
    }

    #[test]
    fn last_verified_at_konversi() {
        let m = meta("x-1", AccountKind::X, Some(1_700_000_000));
        assert!(last_verified_at(&m).is_some());
        let m2 = meta("x-1", AccountKind::X, None);
        assert!(last_verified_at(&m2).is_none());
    }
}
