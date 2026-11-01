//! `nft-vault` -- Social Vault: penyimpanan sesi Google/X terenkripsi
//! (blueprint F12 / 3B, Fase 7).
//!
//! Prinsip (3B):
//! 1. **Tidak ada password mentah.** Yang disimpan adalah *storage state*
//!    sesi browser (cookies + localStorage) yang dienkripsi dengan mesin
//!    keystore yang sama (Argon2id + XChaCha20-Poly1305).
//! 2. File-based, bukan SQLite: satu file terenkripsi per akun di
//!    `storage/vault/<account_id>.bin`. SQLite `accounts` hanya menyimpan
//!    metadata (id, kind, profile_dir, last_verified) -- tidak ada rahasia.
//! 3. Satu profil browser per akun, terpisah dari yang lain.
//! 4. Sesi kedaluwarsa dipantau via `session_health` (3B poin 5).
//!
//! Format file (biner, sama dengan keystore):
//! `magic(7) | version(1) | salt(32) | nonce(24) | ciphertext`
//! ciphertext = JSON `SessionState`.

use std::path::{Path, PathBuf};

use chacha20poly1305::{
    aead::{Aead, KeyInit, OsRng},
    XChaCha20Poly1305, XNonce,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroize;

use nft_core::secret::Secret;

pub mod health;
pub mod meta;

pub use health::{
    session_health, HealthConfig, SessionHealthReport, SessionStatus, DEFAULT_CHECK_INTERVAL_HOURS,
    DEFAULT_EXPIRING_WITHIN_HOURS,
};
pub use meta::{AccountMeta, AccountMetaStore, MetaError};

const MAGIC: &[u8; 7] = b"NFTVL01";
const VERSION: u8 = 1;
const SALT_LEN: usize = 32;
const NONCE_LEN: usize = 24;

/// Parameter Argon2id sama dengan keystore (3B: mesin keystore yang sama).
const ARGON2_M_COST: u32 = 64 * 1024;
const ARGON2_T_COST: u32 = 3;
const ARGON2_P_COST: u32 = 1;

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("gagal membaca file vault '{path}': {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("gagal menulis file vault '{path}': {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("bukan file vault valid (magic/versi tidak dikenali) di '{path}'")]
    InvalidFile { path: String },

    #[error("passphrase salah atau data vault corrupt di '{path}'")]
    WrongPassphrase { path: String },

    #[error("akun vault tidak ditemukan: {account_id}")]
    AccountNotFound { account_id: String },

    #[error("id akun hanya boleh huruf/angka/-/_ (dapat: {account_id})")]
    InvalidAccountId { account_id: String },

    #[error("kind tidak dikenali: {kind} (yang didukung: google | x | discord | custom)")]
    InvalidKind { kind: String },

    #[error("state sesi rusak (parse JSON gagal): {0}")]
    InvalidState(#[from] serde_json::Error),
}

/// Jenis akun sosial (3B). `custom` untuk situs lain yang butuh login.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    Google,
    X,
    Discord,
    Custom,
}

impl AccountKind {
    pub fn parse(s: &str) -> Result<Self, VaultError> {
        match s.to_ascii_lowercase().as_str() {
            "google" => Ok(Self::Google),
            "x" | "twitter" => Ok(Self::X),
            "discord" => Ok(Self::Discord),
            "custom" => Ok(Self::Custom),
            _ => Err(VaultError::InvalidKind {
                kind: s.to_string(),
            }),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::X => "x",
            Self::Discord => "discord",
            Self::Custom => "custom",
        }
    }
}

/// Storage state browser (cookies + localStorage) yang **dienkripsi at rest**.
/// Isi hanya muncul di memori saat `Vault::open_account` dipanggil.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    /// Daftar cookie browser ( nama, domain, path, value, kedaluwarsa ).
    pub cookies: Vec<VaultCookie>,
    /// localStorage per origin ( origin -> key -> value ).
    #[serde(default)]
    pub local_storage:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    /// User-Agent profil browser ini (IP/profil tetap = 3B poin 3).
    #[serde(default)]
    pub user_agent: Option<String>,
}

impl SessionState {
    /// Cookie untuk lookup cepat ( domain + name ).
    pub fn cookie(&self, domain: &str, name: &str) -> Option<&VaultCookie> {
        self.cookies
            .iter()
            .find(|c| c.domain == domain && c.name == name)
    }

    /// Apakah sesi ini memiliki cookie sesi inti (bukan hanya cookie pelengkap).
    pub fn has_session_cookie(&self) -> bool {
        self.cookies.iter().any(|c| {
            (c.name.contains("session") || c.name.contains("auth") || c.name.contains("token"))
                && !c.value.is_empty()
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    #[serde(default)]
    pub path: String,
    /// Unix detik; None = session cookie.
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
}

/// Akun vault yang sedang dibuka ( session state di memori ).
#[derive(Debug)]
pub struct OpenAccount {
    pub id: String,
    pub kind: AccountKind,
    /// Direktori profil browser isolasi ( 3B poin 3 ).
    pub profile_dir: PathBuf,
    session: Secret<Vec<u8>>,
}

impl OpenAccount {
    /// Ambil session state terenkripsi sebagai plaintext sementara.
    /// Pemanggil wajib membatasi penggunaan ini ke browser worker.
    pub fn session_bytes(&self) -> &[u8] {
        self.session.expose()
    }

    /// Direktori profil sebagai string ( untuk meta / logging -- aman ).
    pub fn profile_dir_string(&self) -> String {
        self.profile_dir.display().to_string()
    }
}

/// Social Vault: mengelola file sesi terenkripsi per akun.
///
/// File tersimpan di `<root>/<account_id>.bin`. Metadata non-rahasia
/// ( kind, profile_dir, last_verified ) ada di `accounts` SQLite lewat
/// `AccountMetaStore`; vault file sendiri bersifat opsional/lokal.
#[derive(Debug)]
pub struct Vault {
    root: PathBuf,
}

impl Vault {
    /// Buka vault pada direktori `root` ( dibuat bila belum ada ).
    pub fn open(root: impl AsRef<Path>) -> Result<Self, VaultError> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root).map_err(|e| VaultError::Write {
            path: root.display().to_string(),
            source: e,
        })?;
        Ok(Self { root })
    }

    /// Path file terenkripsi untuk akun ( validasi id supaya tidak path traversal ).
    fn account_path(&self, account_id: &str) -> Result<PathBuf, VaultError> {
        validate_account_id(account_id)?;
        Ok(self.root.join(format!("{account_id}.bin")))
    }

    /// Simpan session state terenkripsi untuk akun ( atomic write ).
    pub fn save_account(
        &self,
        account_id: &str,
        state: &SessionState,
        passphrase: &Secret<Vec<u8>>,
    ) -> Result<(), VaultError> {
        let path = self.account_path(account_id)?;
        let json = serde_json::to_vec(state)?;
        let ct = seal(&json, passphrase);

        let tmp_path = path.with_extension("tmp");
        std::fs::write(&tmp_path, &ct).map_err(|e| VaultError::Write {
            path: tmp_path.display().to_string(),
            source: e,
        })?;
        std::fs::rename(&tmp_path, &path).map_err(|e| VaultError::Write {
            path: path.display().to_string(),
            source: e,
        })?;
        Ok(())
    }

    /// Buka akun: dekripsi session state ke memori.
    pub fn open_account(
        &self,
        account_id: &str,
        passphrase: &Secret<Vec<u8>>,
    ) -> Result<OpenAccount, VaultError> {
        let path = self.account_path(account_id)?;
        let raw = std::fs::read(&path).map_err(|e| VaultError::Read {
            path: path.display().to_string(),
            source: e,
        })?;
        let json = open_seal(&raw, passphrase, &path)?;
        let _state: SessionState = serde_json::from_slice(&json)?;
        Ok(OpenAccount {
            id: account_id.to_string(),
            kind: AccountKind::Custom,
            profile_dir: self.root.join("profiles").join(account_id),
            session: Secret::new(json),
        })
    }

    /// Buka akun beserta metadata kind ( dari `AccountMetaStore` ).
    pub fn open_account_with(
        &self,
        meta: &AccountMeta,
        passphrase: &Secret<Vec<u8>>,
    ) -> Result<OpenAccount, VaultError> {
        let path = self.account_path(&meta.id)?;
        let raw = std::fs::read(&path).map_err(|e| VaultError::Read {
            path: path.display().to_string(),
            source: e,
        })?;
        let json = open_seal(&raw, passphrase, &path)?;
        let _state: SessionState = serde_json::from_slice(&json)?;
        Ok(OpenAccount {
            id: meta.id.clone(),
            kind: meta.kind_enum().unwrap_or(AccountKind::Custom),
            profile_dir: PathBuf::from(&meta.profile_dir),
            session: Secret::new(json),
        })
    }

    /// Hapus file sesi akun.
    pub fn remove_account(&self, account_id: &str) -> Result<(), VaultError> {
        let path = self.account_path(account_id)?;
        if !path.exists() {
            return Err(VaultError::AccountNotFound {
                account_id: account_id.to_string(),
            });
        }
        std::fs::remove_file(&path).map_err(|e| VaultError::Write {
            path: path.display().to_string(),
            source: e,
        })
    }

    /// Apakah file sesi akun ada?
    pub fn has_account(&self, account_id: &str) -> bool {
        self.account_path(account_id).is_ok_and(|p| p.exists())
    }

    /// Daftar id akun yang ada di vault ( dari nama file ).
    pub fn account_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for e in entries.flatten() {
                if let Some(name) = e.file_name().to_str() {
                    if let Some(id) = name.strip_suffix(".bin") {
                        if validate_account_id(id).is_ok() {
                            ids.push(id.to_string());
                        }
                    }
                }
            }
        }
        ids.sort();
        ids
    }
}

fn validate_account_id(id: &str) -> Result<(), VaultError> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(VaultError::InvalidAccountId {
            account_id: id.to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// enkripsi ( mesin yang sama dengan nft-keystore: Argon2id + XChaCha20 )
// ---------------------------------------------------------------------------

fn seal(plaintext: &[u8], passphrase: &Secret<Vec<u8>>) -> Vec<u8> {
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce);

    let mut key = derive_key(passphrase, &salt);
    let cipher = XChaCha20Poly1305::new((&key).into());
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext)
        .expect("enkripsi XChaCha20 gagal");
    key.zeroize();

    let mut out = Vec::with_capacity(MAGIC.len() + 1 + SALT_LEN + NONCE_LEN + ct.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

fn open_seal(raw: &[u8], passphrase: &Secret<Vec<u8>>, path: &Path) -> Result<Vec<u8>, VaultError> {
    if raw.len() < MAGIC.len() + 1 + SALT_LEN + NONCE_LEN {
        return Err(VaultError::InvalidFile {
            path: path.display().to_string(),
        });
    }
    let (header, rest) = raw.split_at(MAGIC.len() + 1);
    if &header[..MAGIC.len()] != MAGIC || header[MAGIC.len()] != VERSION {
        return Err(VaultError::InvalidFile {
            path: path.display().to_string(),
        });
    }
    let (salt, rest) = rest.split_at(SALT_LEN);
    let (nonce, ct) = rest.split_at(NONCE_LEN);

    let mut key = derive_key(passphrase, salt);
    let cipher = XChaCha20Poly1305::new((&key).into());
    let pt =
        cipher
            .decrypt(XNonce::from_slice(nonce), ct)
            .map_err(|_| VaultError::WrongPassphrase {
                path: path.display().to_string(),
            })?;
    key.zeroize();
    Ok(pt)
}

fn derive_key(passphrase: &Secret<Vec<u8>>, salt: &[u8]) -> [u8; 32] {
    use argon2::{Algorithm, Argon2, Params, Version};
    let params = Params::new(ARGON2_M_COST, ARGON2_T_COST, ARGON2_P_COST, None)
        .expect("parameter Argon2id valid");
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; 32];
    argon2
        .hash_password_into(passphrase.expose(), salt, &mut key)
        .expect("Argon2id KDF gagal");
    key
}

pub fn default_root() -> PathBuf {
    PathBuf::from("storage/vault")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn pass() -> Secret<Vec<u8>> {
        Secret::new(b"vault-passphrase".to_vec())
    }

    fn state() -> SessionState {
        SessionState {
            cookies: vec![VaultCookie {
                name: "session".into(),
                value: "abc123".into(),
                domain: ".example.com".into(),
                path: "/".into(),
                expires_at: Some(2000000000),
                secure: true,
                http_only: true,
            }],
            local_storage: [(
                "https://example.com".to_string(),
                [("k".to_string(), "v".to_string())].into_iter().collect(),
            )]
            .into_iter()
            .collect(),
            user_agent: Some("vault-test-agent".into()),
        }
    }

    #[test]
    fn vault_open_membuat_direktori() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("vault");
        let vault = Vault::open(&root).unwrap();
        assert!(root.exists());
        assert!(vault.account_ids().is_empty());
    }

    #[test]
    fn save_lalu_open_account() {
        let dir = tempdir().unwrap();
        let v = Vault::open(dir.path()).unwrap();
        v.save_account("x-main-burner", &state(), &pass()).unwrap();

        let acc = v.open_account("x-main-burner", &pass()).unwrap();
        assert_eq!(acc.id, "x-main-burner");
        // Session plaintext hanya di memori, bisa di-parse kembali.
        let s: SessionState = serde_json::from_slice(acc.session_bytes()).unwrap();
        assert_eq!(s.cookie(".example.com", "session").unwrap().value, "abc123");
    }

    #[test]
    fn open_account_dengan_meta() {
        let dir = tempdir().unwrap();
        let v = Vault::open(dir.path()).unwrap();
        v.save_account("google-1", &state(), &pass()).unwrap();

        let meta = AccountMeta {
            id: "google-1".into(),
            kind: AccountKind::Google.as_str().into(),
            profile_dir: dir
                .path()
                .join("profiles")
                .join("google-1")
                .display()
                .to_string(),
            last_verified: None,
            created_at: 0,
            updated_at: 0,
        };
        let acc = v.open_account_with(&meta, &pass()).unwrap();
        assert_eq!(acc.kind, AccountKind::Google);
    }

    #[test]
    fn passphrase_salah_ditolak() {
        let dir = tempdir().unwrap();
        let v = Vault::open(dir.path()).unwrap();
        v.save_account("x-1", &state(), &pass()).unwrap();
        let err = v
            .open_account("x-1", &Secret::new(b"salah".to_vec()))
            .unwrap_err();
        assert!(err.to_string().contains("passphrase salah"));
    }

    #[test]
    fn file_vault_tidak_mengandung_plaintext_cookie() {
        let dir = tempdir().unwrap();
        let v = Vault::open(dir.path()).unwrap();
        let st = state();
        v.save_account("x-1", &st, &pass()).unwrap();

        let bytes = std::fs::read(dir.path().join("x-1.bin")).unwrap();
        // Nilai cookie & domain tidak boleh muncul di file.
        for needle in ["abc123", ".example.com", "session"] {
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "'{needle}' bocor di file vault"
            );
        }
    }

    #[test]
    fn file_corrupt_dan_file_biasa_ditolak() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("corrupt.bin");
        std::fs::write(&path, b"bukan-vault").unwrap();
        let raw = std::fs::read(&path).unwrap();
        let err = open_seal(&raw, &pass(), &path).unwrap_err();
        assert!(err.to_string().contains("bukan file vault valid"));

        // header benar, ciphertext terpotong
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.push(VERSION);
        bad.extend_from_slice(&[0u8; SALT_LEN + NONCE_LEN]);
        bad.extend_from_slice(&[0u8; 5]);
        std::fs::write(&path, &bad).unwrap();
        let raw = std::fs::read(&path).unwrap();
        let err = open_seal(&raw, &pass(), &path).unwrap_err();
        assert!(err.to_string().contains("passphrase salah"));
    }

    #[test]
    fn account_id_invalid_ditolak() {
        let dir = tempdir().unwrap();
        let v = Vault::open(dir.path()).unwrap();
        for bad in ["", "..", "a/b", "x y", &"a".repeat(65)] {
            assert!(v.save_account(bad, &state(), &pass()).is_err());
        }
    }

    #[test]
    fn remove_dan_has_account() {
        let dir = tempdir().unwrap();
        let v = Vault::open(dir.path()).unwrap();
        v.save_account("x-1", &state(), &pass()).unwrap();
        assert!(v.has_account("x-1"));
        v.remove_account("x-1").unwrap();
        assert!(!v.has_account("x-1"));
        assert!(v.remove_account("x-1").is_err());
    }

    #[test]
    fn account_ids_mendaftar_semua_file() {
        let dir = tempdir().unwrap();
        let v = Vault::open(dir.path()).unwrap();
        v.save_account("b-2", &state(), &pass()).unwrap();
        v.save_account("a-1", &state(), &pass()).unwrap();
        v.save_account("c-3", &state(), &pass()).unwrap();
        assert_eq!(v.account_ids(), vec!["a-1", "b-2", "c-3"]);
    }

    #[test]
    fn has_session_cookie_mendeteksi_cookie_inti() {
        let with = state();
        assert!(with.has_session_cookie());
        let without = SessionState {
            cookies: vec![VaultCookie {
                name: "pref".into(),
                value: "x".into(),
                domain: ".example.com".into(),
                path: "/".into(),
                expires_at: None,
                secure: false,
                http_only: false,
            }],
            local_storage: Default::default(),
            user_agent: None,
        };
        assert!(!without.has_session_cookie());
    }

    #[test]
    fn kind_parse_dan_str() {
        assert_eq!(AccountKind::parse("google").unwrap(), AccountKind::Google);
        assert_eq!(AccountKind::parse("X").unwrap(), AccountKind::X);
        assert_eq!(AccountKind::parse("twitter").unwrap(), AccountKind::X);
        assert_eq!(AccountKind::parse("custom").unwrap(), AccountKind::Custom);
        assert!(AccountKind::parse("fbook").is_err());
        assert_eq!(AccountKind::X.as_str(), "x");
    }

    #[test]
    fn default_root_adalah_storage_vault() {
        assert_eq!(default_root(), PathBuf::from("storage/vault"));
    }

    #[test]
    fn session_state_serde_roundtrip() {
        let st = state();
        let json = serde_json::to_string(&st).unwrap();
        let back: SessionState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cookies.len(), 1);
        assert_eq!(back.user_agent.as_deref(), Some("vault-test-agent"));
    }

    #[test]
    fn secret_tidak_muncul_di_debug() {
        let acc = OpenAccount {
            id: "x".into(),
            kind: AccountKind::X,
            profile_dir: PathBuf::from("/tmp/p"),
            session: Secret::new(b"rahasia".to_vec()),
        };
        let debug = format!("{:?}", acc);
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("rahasia"));
    }
}
