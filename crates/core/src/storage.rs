//! Storage SQLite mode WAL + migrasi (blueprint §4, §9 Fase 1).
//!
//! - WAL: tulis dibatch di luar hot path (§8.2 poin 9).
//! - Foreign keys aktif; skema di `migrations/`.
//! - Tidak ada kolom penyimpanan rahasia — lihat komentar di `0001_init.sql`.

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;

/// Migrasi di-embed saat compile supaya binary tetap membawa skema.
// `migrate!()` tanpa argumen memakai `migrations/` di root crate (CARGO_MANIFEST_DIR).
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Buka pool SQLite (WAL + foreign keys) dan jalankan migrasi.
/// `url` memakai format SQLite URL, mis. `sqlite::memory:` atau `sqlite://path/agent.db`.
pub async fn connect(url: &str) -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::from_str(url)?
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .create_if_missing(true);
    open_with(options).await
}

/// Sama dengan `connect`, tetapi menerima `Path` langsung — aman untuk path
/// Windows yang mengandung spasi/karakter khusus (tidak perlu URL-encoding).
pub async fn connect_file(path: &std::path::Path) -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .create_if_missing(true);
    open_with(options).await
}

async fn open_with(options: SqliteConnectOptions) -> Result<SqlitePool, sqlx::Error> {
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;

    run_migrations(&pool).await?;
    Ok(pool)
}

pub async fn run_migrations(pool: &SqlitePool) -> Result<(), sqlx::migrate::MigrateError> {
    MIGRATOR.run(pool).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    #[tokio::test]
    async fn migrasi_dan_crud_tasks() {
        let pool = connect("sqlite::memory:").await.unwrap();

        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO tasks (id, idempotency_key, kind, payload, status, scheduled_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, NULL, ?, ?)",
        )
        .bind(&id)
        .bind("idem-1")
        .bind("mint")
        .bind("{}")
        .bind("pending")
        .bind(now())
        .bind(now())
        .execute(&pool)
        .await
        .unwrap();

        let (kind, status): (String, String) =
            sqlx::query_as("SELECT kind, status FROM tasks WHERE id = ?")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(kind, "mint");
        assert_eq!(status, "pending");
    }
    #[tokio::test]
    async fn idempotency_key_mencegah_dobel_insert() {
        let pool = connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "INSERT INTO tasks (id, idempotency_key, kind, payload, status, created_at, updated_at) \
             VALUES (?, 'war-1', 'mint', '{}', 'pending', ?, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(now())
        .bind(now())
        .execute(&pool)
        .await
        .unwrap();

        let second = sqlx::query(
            "INSERT INTO tasks (id, idempotency_key, kind, payload, status, created_at, updated_at) \
             VALUES (?, 'war-1', 'mint', '{}', 'pending', ?, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(now())
        .bind(now())
        .execute(&pool)
        .await;
        assert!(
            second.is_err(),
            "insert kedua dengan idempotency key sama harus gagal"
        );
    }

    #[tokio::test]
    async fn foreign_key_txs_ke_intents_diaktifkan() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let res = sqlx::query(
            "INSERT INTO txs (id, intent_id, chain_id, wallet_id, status, payload, created_at, updated_at) \
             VALUES ('tx-1', 'intent-tidak-ada', 8453, 'burner-1', 'prepared', '{}', ?, ?)",
        )
        .bind(now())
        .bind(now())
        .execute(&pool)
        .await;
        assert!(res.is_err(), "FK harus menolak tx tanpa intent yang valid");
    }

    #[tokio::test]
    async fn semua_tabel_ada_setelah_migrasi() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        let names: Vec<String> = rows.into_iter().map(|r| r.0).collect();
        for expected in ["accounts", "audit", "intents", "rules", "tasks", "txs"] {
            assert!(
                names.contains(&expected.to_string()),
                "tabel {expected} hilang: {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn migrasi_idempoten() {
        let pool = connect("sqlite::memory:").await.unwrap();
        // Jalankan ulang: migrasi kedua tidak boleh error.
        run_migrations(&pool).await.unwrap();
    }
    #[tokio::test]
    async fn file_wal_diterima() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.db");
        let pool = connect_file(&path).await.unwrap();
        let cnt: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM tasks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(cnt.0, 0);
        assert!(path.exists());
    }
}
