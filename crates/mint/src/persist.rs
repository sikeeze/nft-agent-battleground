//! Idempotency & resume ( blueprint §6.2 "Dobel mint tak sengaja",
//! "Proses mati / restart saat war", §9 Fase 5 acceptance ).
//!
//! Idempotency key per **( task, wallet, nonce )** disimpan di tabel `txs`
//! ( sudah ada dari Fase 1 ) — insert kedua dengan key yang sama ditolak
//! SQLite ( UNIQUE ), sehingga proses yang di-restart di tengah war tidak
//! mengirim tx yang sudah pernah dikirim.
//!
//! Resume: saat agent start, tx dengan status `sent` tapi belum `confirmed`
//! dicek ulang ( receipt ) — bila sudah masuk block, ditandai confirmed;
//! bila masih pending, ditunggu ( atau di-bump, lihat `exec.rs` ).
//!
//! ponytail: ini lapisan persistensi tipis di atas `txs`. Scheduler Fase 10
//! yang membangunkan job tertunda menyusul; sekarang `resume_pending` cukup
//! untuk acceptance "tidak ada dobel mint saat proses di-restart".

use sqlx::SqlitePool;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PersistError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] sqlx::Error),

    #[error("tx dengan idempotency key {0} sudah pernah dikirim — dobel mint dicegah")]
    Duplicate(String),
}

/// Status tx di database ( sesuai migrasi 0001: prepared | signed | sent |
/// confirmed | failed ).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxStatus {
    Prepared,
    Signed,
    Sent,
    Confirmed,
    Failed,
}

impl TxStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TxStatus::Prepared => "prepared",
            TxStatus::Signed => "signed",
            TxStatus::Sent => "sent",
            TxStatus::Confirmed => "confirmed",
            TxStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "prepared" => Some(Self::Prepared),
            "signed" => Some(Self::Signed),
            "sent" => Some(Self::Sent),
            "confirmed" => Some(Self::Confirmed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// Catat tx baru. `idempotency_key` = `(task, wallet, nonce)` — insert
/// duplikat memunculkan `Duplicate` ( UNIQUE constraint `txs.id`, kita pakai
/// key sebagai PK di sini ).
pub async fn record_tx(
    pool: &SqlitePool,
    idempotency_key: &str,
    chain_id: u64,
    wallet_id: &str,
    nonce: u64,
    tx_hash: Option<&str>,
    status: TxStatus,
) -> Result<(), PersistError> {
    let now = chrono::Utc::now().timestamp();
    // Idempotency: INSERT OR IGNORE — bila key sudah ada, ini bukan error
    // fatal, tapi pemanggil harus tahu. Kita SELECT dulu untuk membedakan
    // "sudah ada" vs "baru ditulis".
    let existing: Option<(String,)> = sqlx::query_as("SELECT id FROM txs WHERE id = ?")
        .bind(idempotency_key)
        .fetch_optional(pool)
        .await?;
    if existing.is_some() {
        return Err(PersistError::Duplicate(idempotency_key.to_string()));
    }

    let intent_id = idempotency_key.to_string();
    // `txs` butuh intent_id ( FK ); buat intent placeholder per key bila
    // belum ada — intent ini bebas rahasia ( hanya task/wallet/nonce ).
    sqlx::query(
        "INSERT OR IGNORE INTO intents (id, kind, chain_id, params, originator, created_at) \
         VALUES (?, 'mint', ?, ?, 'user', ?)",
    )
    .bind(&intent_id)
    .bind(chain_id as i64)
    .bind(format!("{{\"key\":\"{idempotency_key}\"}}"))
    .bind(now)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO txs (id, intent_id, chain_id, wallet_id, nonce, tx_hash, status, payload, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, '{}', ?, ?)",
    )
    .bind(idempotency_key)
    .bind(&intent_id)
    .bind(chain_id as i64)
    .bind(wallet_id)
    .bind(nonce as i64)
    .bind(tx_hash)
    .bind(status.as_str())
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;

    Ok(())
}

/// Update status tx ( + hash bila sudah ada ).
pub async fn update_tx(
    pool: &SqlitePool,
    idempotency_key: &str,
    status: TxStatus,
    tx_hash: Option<&str>,
) -> Result<(), PersistError> {
    let now = chrono::Utc::now().timestamp();
    let res = sqlx::query(
        "UPDATE txs SET status = ?, tx_hash = COALESCE(?, tx_hash), updated_at = ? WHERE id = ?",
    )
    .bind(status.as_str())
    .bind(tx_hash)
    .bind(now)
    .bind(idempotency_key)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(PersistError::Duplicate(idempotency_key.to_string()));
    }
    Ok(())
}

/// Tx yang masih `sent` ( belum confirmed ) — kandidat resume setelah
/// restart ( §6.2 ). Mengembalikan ( id, wallet_id, nonce, tx_hash ).
pub async fn pending_txs(
    pool: &SqlitePool,
) -> Result<Vec<(String, String, i64, String)>, PersistError> {
    let rows: Vec<(String, String, i64, Option<String>)> =
        sqlx::query_as("SELECT id, wallet_id, nonce, tx_hash FROM txs WHERE status = 'sent'")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(a, b, c, d)| (a, b, c, d.unwrap_or_default()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> SqlitePool {
        let pool = nft_core::storage::connect("sqlite::memory:").await.unwrap();
        // Tabel intents butuh task_id nullable & txs FK ke intents.
        pool
    }

    #[tokio::test]
    async fn record_lalu_update_status() {
        let pool = setup().await;
        record_tx(
            &pool,
            "task-1:burner-1:0",
            8453,
            "burner-1",
            0,
            None,
            TxStatus::Prepared,
        )
        .await
        .unwrap();
        update_tx(&pool, "task-1:burner-1:0", TxStatus::Sent, Some("0xabc"))
            .await
            .unwrap();
        let (status, hash): (String, String) =
            sqlx::query_as("SELECT status, COALESCE(tx_hash,'') FROM txs WHERE id = ?")
                .bind("task-1:burner-1:0")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "sent");
        assert_eq!(hash, "0xabc");
    }

    #[tokio::test]
    async fn record_duplikat_ditolak() {
        let pool = setup().await;
        record_tx(
            &pool,
            "task-1:burner-1:0",
            8453,
            "burner-1",
            0,
            None,
            TxStatus::Prepared,
        )
        .await
        .unwrap();
        let err = record_tx(
            &pool,
            "task-1:burner-1:0",
            8453,
            "burner-1",
            0,
            None,
            TxStatus::Prepared,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PersistError::Duplicate(_)));
    }

    #[tokio::test]
    async fn pending_txs_mengembalikan_yang_sent() {
        let pool = setup().await;
        record_tx(&pool, "k1", 8453, "w1", 0, Some("0x1"), TxStatus::Sent)
            .await
            .unwrap();
        record_tx(&pool, "k2", 8453, "w1", 1, None, TxStatus::Confirmed)
            .await
            .unwrap();
        let pending = pending_txs(&pool).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, "k1");
    }

    #[tokio::test]
    async fn status_roundtrip() {
        for s in [
            TxStatus::Prepared,
            TxStatus::Signed,
            TxStatus::Sent,
            TxStatus::Confirmed,
            TxStatus::Failed,
        ] {
            assert_eq!(TxStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(TxStatus::parse("bogus"), None);
    }
}
