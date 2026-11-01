//! Metadata akun social vault di SQLite tabel `accounts` (blueprint 3B).
//!
//! **Aturan keamanan**: tabel ini hanya menyimpan metadata NON-rahasia
//! ( id, kind, profile_dir, last_verified ). Session state terenkripsi
//! ada di file vault ( `crates/vault` ), bukan di SQLite -- lihat
//! komentar di migrasi `0001_init.sql`.

use sqlx::SqlitePool;
use thiserror::Error;

use crate::{AccountKind, VaultError};

#[derive(Debug, Error)]
pub enum MetaError {
    #[error("error SQLite: {0}")]
    Db(#[from] sqlx::Error),

    #[error("akun tidak ditemukan: {0}")]
    NotFound(String),

    #[error("akun sudah ada: {0}")]
    Duplicate(String),

    #[error("error vault: {0}")]
    Vault(#[from] VaultError),
}

/// Metadata akun ( non-rahasia ) -- `accounts` table.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct AccountMeta {
    pub id: String,
    pub kind: String,
    pub profile_dir: String,
    pub last_verified: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl AccountMeta {
    pub fn kind_enum(&self) -> Result<AccountKind, VaultError> {
        AccountKind::parse(&self.kind)
    }
}

/// Store metadata akun di SQLite.
#[derive(Debug, Clone)]
pub struct AccountMetaStore {
    pool: SqlitePool,
}

impl AccountMetaStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Tambah akun baru. `profile_dir` = direktori profil browser isolasi.
    pub async fn add(
        &self,
        id: &str,
        kind: AccountKind,
        profile_dir: &str,
    ) -> Result<AccountMeta, MetaError> {
        let now = chrono::Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO accounts (id, kind, profile_dir, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(kind.as_str())
        .bind(profile_dir)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(ref de) if de.message().contains("UNIQUE") => {
                MetaError::Duplicate(id.to_string())
            }
            other => MetaError::Db(other),
        })?;

        self.get(id)
            .await?
            .ok_or_else(|| MetaError::NotFound(id.into()))
    }

    /// Ambil metadata akun.
    pub async fn get(&self, id: &str) -> Result<Option<AccountMeta>, MetaError> {
        let row = sqlx::query_as::<_, AccountMeta>(
            "SELECT id, kind, profile_dir, last_verified, created_at, updated_at \
             FROM accounts WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Daftar semua akun ( urut id ).
    pub async fn list(&self) -> Result<Vec<AccountMeta>, MetaError> {
        let rows = sqlx::query_as::<_, AccountMeta>(
            "SELECT id, kind, profile_dir, last_verified, created_at, updated_at \
             FROM accounts ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Pool SQLite ( untuk operasi langsung di test/internal ).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Catat waktu verifikasi sesi terakhir ( unix detik ).
    pub async fn touch_verified(&self, id: &str) -> Result<(), MetaError> {
        let now = chrono::Utc::now().timestamp();
        let res = sqlx::query("UPDATE accounts SET last_verified = ?, updated_at = ? WHERE id = ?")
            .bind(now)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(MetaError::NotFound(id.to_string()));
        }
        Ok(())
    }

    /// Hapus akun.
    pub async fn remove(&self, id: &str) -> Result<(), MetaError> {
        let res = sqlx::query("DELETE FROM accounts WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(MetaError::NotFound(id.to_string()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> AccountMetaStore {
        let pool = nft_core::storage::connect("sqlite::memory:").await.unwrap();
        AccountMetaStore::new(pool)
    }
    #[tokio::test]
    async fn add_get_list_akun() {
        let s = store().await;
        let a = s
            .add(
                "x-main-burner",
                AccountKind::X,
                "vault/profiles/x-main-burner",
            )
            .await
            .unwrap();
        assert_eq!(a.id, "x-main-burner");
        assert_eq!(a.kind_enum().unwrap(), AccountKind::X);

        let got = s.get("x-main-burner").await.unwrap().unwrap();
        assert_eq!(got.profile_dir, "vault/profiles/x-main-burner");

        s.add("google-1", AccountKind::Google, "vault/profiles/google-1")
            .await
            .unwrap();
        let list = s.list().await.unwrap();
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn add_duplikat_ditolak() {
        let s = store().await;
        s.add("g-1", AccountKind::Google, "p").await.unwrap();
        let err = s.add("g-1", AccountKind::Google, "p").await.unwrap_err();
        assert!(err.to_string().contains("sudah ada"));
    }

    #[tokio::test]
    async fn touch_verified_mengisi_last_verified() {
        let s = store().await;
        s.add("x-1", AccountKind::X, "p").await.unwrap();
        s.touch_verified("x-1").await.unwrap();
        let got = s.get("x-1").await.unwrap().unwrap();
        assert!(got.last_verified.is_some());
    }

    #[tokio::test]
    async fn remove_akun() {
        let s = store().await;
        s.add("x-1", AccountKind::X, "p").await.unwrap();
        s.remove("x-1").await.unwrap();
        assert!(s.get("x-1").await.unwrap().is_none());
        assert!(s.remove("x-1").await.is_err());
    }

    #[tokio::test]
    async fn get_akun_tidak_ada() {
        let s = store().await;
        assert!(s.get("hantu").await.unwrap().is_none());
    }
}
