//! Kontrol akses Telegram ( blueprint §3 "Aturan Keamanan Chat Telegram" ).
//!
//! 1. **Whitelist `user_id`** ( bukan username ). Pesan dari ID lain
//!    **diabaikan tanpa balasan** ( §3 poin 2 ). Akun yang diambil alih
//!    tetap tidak bisa mengontrol agent selama user_id-nya tidak di-whitelist.
//! 2. **Rate limit perintah** ( §3 poin 7 ) — batas per user per menit.
//! 3. **Lockout** setelah beberapa percobaan gagal ( §3A "Percobaan import
//!    dibatasi ( rate limit + lockout setelah beberapa kali gagal parse )" ).
//! 4. **Audit log** untuk setiap aksi dari chat ( §3 poin 7, §F5 ) —
//!    ditulis ke tabel `audit` ( tanpa rahasia; alamat disingkat ).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

/// behavior akses dari user Telegram.
#[derive(Debug, Clone, Copy)]
pub enum AccessOutcome {
    /// user_id ada di whitelist, rate limit belum terlampaui → layani.
    Allow,
    /// user_id TIDAK di whitelist → **abaikan tanpa balasan** ( §3 poin 2 ).
    UnknownUser,
    /// Rate limit terlampaui → balas pesan "terlalu banyak perintah".
    RateLimited,
    /// user dalam masa lockout ( terlalu banyak kegagalan ) → tolak.
    LockedOut,
}

/// Pengaturan rate limit & lockout.
#[derive(Debug, Clone, Copy)]
pub struct AccessLimits {
    /// Maksimum perintah per user dalam `window`.
    pub max_commands_per_window: u32,
    /// Window rate limit.
    pub window: Duration,
    /// Berapa kegagalan ( mis. parse import ) sebelum lockout.
    pub max_failures_before_lockout: u32,
    /// Durasi lockout.
    pub lockout: Duration,
}

impl Default for AccessLimits {
    fn default() -> Self {
        Self {
            max_commands_per_window: 20,
            window: Duration::from_secs(60),
            max_failures_before_lockout: 5,
            lockout: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Default)]
struct UserState {
    /// Timestamp perintah dalam window saat ini.
    hits: Vec<Instant>,
    /// Hitungan kegagalan ( reset setelah lockout selesai ).
    failures: u32,
    /// Kapan lockout dimulai ( bila Some ).
    locked_at: Option<Instant>,
}

/// Pemeriksa akses: whitelist + rate limit + lockout. Clone-able ( Arc ).
#[derive(Debug, Clone)]
pub struct AccessControl {
    allowed: Arc<Vec<i64>>,
    limits: AccessLimits,
    state: Arc<Mutex<HashMap<i64, UserState>>>,
}

impl AccessControl {
    pub fn new(allowed_user_ids: Vec<i64>, limits: AccessLimits) -> Self {
        Self {
            allowed: Arc::new(allowed_user_ids),
            limits,
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Periksa satu perintah dari `user_id`. `is_command` = true bila ini
    /// perintah ( menghabiskan kuota ), false bila callback/answer ( bebas ).
    pub async fn check(&self, user_id: i64, is_command: bool) -> AccessOutcome {
        if !self.allowed.contains(&user_id) {
            return AccessOutcome::UnknownUser;
        }
        let mut st = self.state.lock().await;
        let s = st.entry(user_id).or_default();

        // Lockout masih aktif?
        if let Some(at) = s.locked_at {
            if at.elapsed() < self.limits.lockout {
                return AccessOutcome::LockedOut;
            }
            // Lockout selesai → reset.
            s.locked_at = None;
            s.failures = 0;
        }

        if is_command {
            // Garis besarnya window.
            let cutoff = Instant::now() - self.limits.window;
            s.hits.retain(|&t| t > cutoff);
            if s.hits.len() >= self.limits.max_commands_per_window as usize {
                return AccessOutcome::RateLimited;
            }
            s.hits.push(Instant::now());
        }
        AccessOutcome::Allow
    }

    /// Catat kegagalan user ( mis. parse phrase import gagal ).
    /// Setelah `max_failures_before_lockout` → lockout otomatis.
    pub async fn record_failure(&self, user_id: i64) {
        let mut st = self.state.lock().await;
        let s = st.entry(user_id).or_default();
        s.failures += 1;
        if s.failures >= self.limits.max_failures_before_lockout {
            s.locked_at = Some(Instant::now());
            tracing::warn!(user_id, failures = s.failures, "user ter-lockout");
        }
    }

    /// Catat keberhasilan ( reset hitungan kegagalan tanpa menghapus kuota ).
    pub async fn record_success(&self, user_id: i64) {
        let mut st = self.state.lock().await;
        if let Some(s) = st.get_mut(&user_id) {
            s.failures = 0;
        }
    }

    /// Apakah user ini di-whitelist?
    pub fn is_allowed(&self, user_id: i64) -> bool {
        self.allowed.contains(&user_id)
    }

    /// Reset state satu user ( untuk test / admin ).
    pub async fn reset(&self, user_id: i64) {
        self.state.lock().await.remove(&user_id);
    }
}

/// Tulis entri audit ke tabel `audit` ( §3 poin 7 ).
/// `subject` wajib bebas rahasia ( alamat disingkat `0xAb..12` ).
pub async fn audit(
    pool: &sqlx::SqlitePool,
    actor: &str,
    action: &str,
    subject: Option<&str>,
    detail: Option<&serde_json::Value>,
) -> Result<(), sqlx::Error> {
    let now = chrono::Utc::now().timestamp();
    let detail = detail.map(|v| v.to_string());
    sqlx::query("INSERT INTO audit (at, actor, action, subject, detail) VALUES (?, ?, ?, ?, ?)")
        .bind(now)
        .bind(actor)
        .bind(action)
        .bind(subject)
        .bind(detail)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ac() -> AccessControl {
        AccessControl::new(vec![123, 456], AccessLimits::default())
    }

    #[tokio::test]
    async fn user_whitelist_diizinkan() {
        let a = ac();
        assert!(matches!(a.check(123, true).await, AccessOutcome::Allow));
        assert!(matches!(a.check(456, true).await, AccessOutcome::Allow));
    }

    #[tokio::test]
    async fn user_asing_diabaikan() {
        // §3 poin 2: pesan dari ID lain diabaikan tanpa balasan.
        let a = ac();
        assert!(matches!(
            a.check(999, true).await,
            AccessOutcome::UnknownUser
        ));
        assert!(!a.is_allowed(999));
    }

    #[tokio::test]
    async fn rate_limit_memblokir_setelah_batas() {
        let limits = AccessLimits {
            max_commands_per_window: 3,
            ..Default::default()
        };
        let a = AccessControl::new(vec![1], limits);
        for _ in 0..3 {
            assert!(matches!(a.check(1, true).await, AccessOutcome::Allow));
        }
        assert!(matches!(a.check(1, true).await, AccessOutcome::RateLimited));
        // Callback ( bukan perintah ) tetap bebas —— answerCallbackQuery instan.
        assert!(matches!(a.check(1, false).await, AccessOutcome::Allow));
    }

    #[tokio::test]
    async fn rate_limit_reset_setelah_window() {
        let limits = AccessLimits {
            max_commands_per_window: 1,
            window: Duration::from_millis(50),
            ..Default::default()
        };
        let a = AccessControl::new(vec![1], limits);
        assert!(matches!(a.check(1, true).await, AccessOutcome::Allow));
        assert!(matches!(a.check(1, true).await, AccessOutcome::RateLimited));
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(matches!(a.check(1, true).await, AccessOutcome::Allow));
    }

    #[tokio::test]
    async fn lockout_setelah_kegagalan_beruntun() {
        let limits = AccessLimits {
            max_failures_before_lockout: 2,
            lockout: Duration::from_millis(100),
            ..Default::default()
        };
        let a = AccessControl::new(vec![1], limits);
        a.record_failure(1).await;
        assert!(matches!(a.check(1, true).await, AccessOutcome::Allow));
        a.record_failure(1).await;
        // Sekarang lockout.
        assert!(matches!(a.check(1, true).await, AccessOutcome::LockedOut));
        // Setelah lockout selesai → reset.
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(matches!(a.check(1, true).await, AccessOutcome::Allow));
    }

    #[tokio::test]
    async fn record_success_mereset_kegagalan() {
        let limits = AccessLimits {
            max_failures_before_lockout: 2,
            ..Default::default()
        };
        let a = AccessControl::new(vec![1], limits);
        a.record_failure(1).await;
        a.record_success(1).await;
        a.record_failure(1).await;
        assert!(matches!(a.check(1, true).await, AccessOutcome::Allow));
    }

    #[tokio::test]
    async fn audit_menulis_ke_tabel() {
        let pool = nft_core::storage::connect("sqlite::memory:").await.unwrap();
        let actor = "123";
        audit(
            &pool,
            actor,
            "/stop",
            Some("kill switch"),
            Some(&serde_json::json!({"via": "telegram"})),
        )
        .await
        .unwrap();
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn reset_user_menghapus_state() {
        let a = ac();
        a.record_failure(123).await;
        a.reset(123).await;
        let st = a.state.lock().await;
        assert!(!st.contains_key(&123));
    }
}
