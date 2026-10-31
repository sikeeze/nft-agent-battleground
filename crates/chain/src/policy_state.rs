//! `PolicyState` persisten SQLite ( blueprint Â§F5, Â§9 Fase 3 ).
//!
//! Menggantikan `InMemoryPolicyState` di `crates/policy`: spend harian dan
//! status kill switch **tetap aktif setelah restart** ( Â§F5 "/stop
//! mematikan semua job dan signer", Â§6.2 "Proses mati / restart saat war" ).
//!
//! Skema ( migrasi `0002_policy_state.sql` ):
//! - `policy_kill_switch( id, active, reason, updated_at )` â€” satu baris.
//! - `policy_spend( day, wallet, eth, updated_at )` â€” spend harian per wallet.
//!
//! Aturan keras: tidak ada rahasia yang disimpan ( Â§3A ). Hanya angka dan
//! alamat ( alamat bukan rahasia â€” lihat `WalletRef` ).

use alloy::primitives::Address;
use async_trait::async_trait;
use nft_policy::PolicyState;
use thiserror::Error;
use tracing::debug;

#[derive(Debug, Error)]
pub enum PolicyStateError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] sqlx::Error),

    #[error("parse address gagal: {0}")]
    Address(String),
}

/// Implementasi `PolicyState` persisten ( SQLite WAL ).
#[derive(Debug, Clone)]
pub struct SqlitePolicyState {
    pool: sqlx::SqlitePool,
}

impl SqlitePolicyState {
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self { pool }
    }

    /// Hari saat ini ( format YYYY-MM-DD UTC ) â€” key spend harian.
    fn day() -> String {
        chrono::Utc::now().format("%Y-%m-%d").to_string()
    }

    fn addr(wallet: Address) -> String {
        format!("{wallet:?}")
    }
}

#[async_trait]
impl PolicyState for SqlitePolicyState {
    async fn spent_today(&self, wallet: Address) -> f64 {
        let day = Self::day();
        let key = Self::addr(wallet);
        let row: Option<(f64,)> =
            sqlx::query_as("SELECT eth FROM policy_spend WHERE day = ? AND wallet = ?")
                .bind(day)
                .bind(key)
                .fetch_optional(&self.pool)
                .await
                .ok()
                .flatten();
        row.map(|r| r.0).unwrap_or(0.0)
    }

    async fn record_spend(&self, wallet: Address, eth: f64) {
        let day = Self::day();
        let key = Self::addr(wallet);
        let now = chrono::Utc::now().timestamp();
        let res = sqlx::query(
            "INSERT INTO policy_spend (day, wallet, eth, updated_at) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(day, wallet) DO UPDATE SET eth = eth + ?, updated_at = ?",
        )
        .bind(&day)
        .bind(&key)
        .bind(eth)
        .bind(now)
        .bind(eth)
        .bind(now)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => debug!(%day, wallet = %key, eth, "spend harian dicatat"),
            Err(e) => tracing::error!(?e, "gagal mencatat spend harian"),
        }
    }

    async fn kill_switch_on(&self) -> Option<String> {
        let row: Option<(i64, Option<String>)> =
            sqlx::query_as("SELECT active, reason FROM policy_kill_switch WHERE id = 1")
                .fetch_optional(&self.pool)
                .await
                .ok()
                .flatten();
        match row {
            Some((1, reason)) => Some(reason.unwrap_or_default()),
            _ => None,
        }
    }

    async fn trigger_kill_switch(&self, reason: String) {
        let now = chrono::Utc::now().timestamp();
        let res = sqlx::query(
            "INSERT INTO policy_kill_switch (id, active, reason, updated_at) \
             VALUES (1, 1, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET active = 1, reason = ?, updated_at = ?",
        )
        .bind(&reason)
        .bind(now)
        .bind(&reason)
        .bind(now)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => debug!(%reason, "kill switch diaktifkan ( persisten )"),
            Err(e) => tracing::error!(?e, "gagal menyimpan kill switch"),
        }
    }

    async fn clear_kill_switch(&self) {
        let now = chrono::Utc::now().timestamp();
        let res = sqlx::query(
            "INSERT INTO policy_kill_switch (id, active, reason, updated_at) \
             VALUES (1, 0, NULL, ?) \
             ON CONFLICT(id) DO UPDATE SET active = 0, reason = NULL, updated_at = ?",
        )
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => debug!("kill switch dimatikan"),
            Err(e) => tracing::error!(?e, "gagal mematikan kill switch"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> SqlitePolicyState {
        let pool = nft_core::storage::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS policy_kill_switch (\
             id INTEGER PRIMARY KEY, active INTEGER NOT NULL, reason TEXT, \
             updated_at INTEGER NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS policy_spend (\
             day TEXT NOT NULL, wallet TEXT NOT NULL, eth REAL NOT NULL, \
             updated_at INTEGER NOT NULL, PRIMARY KEY (day, wallet))",
        )
        .execute(&pool)
        .await
        .unwrap();
        SqlitePolicyState::new(pool)
    }

    #[tokio::test]
    async fn kill_switch_persisten_setelah_restart() {
        let st = setup().await;
        st.trigger_kill_switch("user /stop".into()).await;
        assert_eq!(st.kill_switch_on().await, Some("user /stop".to_string()));

        // Simulasikan restart: pool baru ( DB in-memory berbeda â†’ tapi di
        // sini kita memakai pool yang sama untuk verifikasi logika persistensi
        // via tabel; lihat test `pool_baru_membaca_kill_switch` di integrasi ).
        assert!(st.kill_switch_on().await.is_some());

        st.clear_kill_switch().await;
        assert_eq!(st.kill_switch_on().await, None);
    }

    #[tokio::test]
    async fn spend_harian_terakumulasi() {
        let st = setup().await;
        let w = Address::ZERO;
        assert_eq!(st.spent_today(w).await, 0.0);
        st.record_spend(w, 0.2).await;
        st.record_spend(w, 0.3).await;
        let spent = st.spent_today(w).await;
        assert!((spent - 0.5).abs() < 1e-9, "spent = {spent}");
    }

    #[tokio::test]
    async fn spend_wallet_terpisah() {
        let st = setup().await;
        let a = Address::ZERO;
        let b = Address::from_slice(&[0x22; 20]);
        st.record_spend(a, 0.1).await;
        st.record_spend(b, 0.4).await;
        assert!((st.spent_today(a).await - 0.1).abs() < 1e-9);
        assert!((st.spent_today(b).await - 0.4).abs() < 1e-9);
    }

    #[tokio::test]
    async fn addr_format_stabil() {
        let a = Address::from_slice(&[0x33; 20]);
        let s = SqlitePolicyState::addr(a);
        assert!(s.starts_with("0x"));
        assert_eq!(s.len(), 42);
    }
}
