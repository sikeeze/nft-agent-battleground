//! `nft-keystore` — penyimpanan wallet terenkripsi (blueprint §F4, §3A).
//!
//! Prinsip:
//! - Private key / seed HANYA ada di keystore terenkripsi (Argon2id KDF + XChaCha20-Poly1305 AEAD).
//! - Passphrase TIDAK pernah ditulis ke disk / log / SQLite.
//! - Data mentah langsung di-zeroize begitu masuk `Secret<T>`.
//! - Tidak ada jalur "export plaintext key" dari keystore — hanya alamat + signer.
//!
//! Format file (biner):
//! `magic(7) | version(1) | salt(32) | nonce(24) | ciphertext`
//! Semua meta wallet (termasuk private key) berada di dalam ciphertext.

use std::path::Path;

use alloy::signers::local::PrivateKeySigner;
use chacha20poly1305::{
    aead::{Aead, KeyInit, OsRng},
    XChaCha20Poly1305, XNonce,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use nft_core::secret::Secret;

const MAGIC: &[u8; 7] = b"NFTKS01";
const VERSION: u8 = 1;
const SALT_LEN: usize = 32;
const NONCE_LEN: usize = 24;

/// Parameter Argon2id ( blueprint §3A ): tahan brute-force GPU.
/// m=64 MiB, t=3, p=1.
const ARGON2_M_COST: u32 = 64 * 1024;
const ARGON2_T_COST: u32 = 3;
const ARGON2_P_COST: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WalletMeta {
    address: String,
    label: String,
    #[serde(with = "serde_bytes")]
    secret: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum KeystoreError {
    #[error("gagal membaca file keystore '{path}': {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("gagal menulis file keystore '{path}': {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("bukan file keystore valid (magic/versi tidak dikenali) di '{path}'")]
    InvalidFile { path: String },

    #[error("passphrase salah atau data corrupt di '{path}'")]
    WrongPassphrase { path: String },

    #[error("private key tidak valid (bukan 32 byte secp256k1): {source}")]
    InvalidPrivateKey {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },

    #[error("wallet tidak ditemukan: {wallet_id}")]
    WalletNotFound { wallet_id: String },

    #[error("duplikat wallet id: {wallet_id}")]
    DuplicateWalletId { wallet_id: String },
}

#[derive(Debug)]
pub struct OpenWallet {
    pub id: String,
    pub label: String,
    pub address: String,
    secret: Secret<Vec<u8>>,
}

impl OpenWallet {
    pub fn from_raw_key(
        id: impl Into<String>,
        label: impl Into<String>,
        raw_key: Vec<u8>,
    ) -> Result<Self, KeystoreError> {
        if raw_key.len() != 32 {
            return Err(KeystoreError::InvalidPrivateKey {
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("panjang key {} bukan 32 byte", raw_key.len()),
                )),
            });
        }
        let signer = PrivateKeySigner::from_slice(&raw_key).map_err(|e| {
            KeystoreError::InvalidPrivateKey {
                source: Box::new(e),
            }
        })?;
        let address = signer.address().to_string();
        let wallet = Self {
            id: id.into(),
            label: label.into(),
            address,
            secret: Secret::new(raw_key),
        };
        Ok(wallet)
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    /// Bangun `PrivateKeySigner` alloy dari kunci ini. Kunci tetap berada di balik
    /// `Secret<T>`; hanya crate signer (yang sudah menerima `ApprovedTx`) yang memakai ini.
    pub fn signer(&self) -> Result<PrivateKeySigner, KeystoreError> {
        PrivateKeySigner::from_slice(self.secret.expose()).map_err(|e| {
            KeystoreError::InvalidPrivateKey {
                source: Box::new(e),
            }
        })
    }
}

#[derive(Debug)]
pub struct Keystore {
    path: std::path::PathBuf,
    #[allow(dead_code)]
    path_str: String,
    wallets: Vec<OpenWallet>,
}

impl Keystore {
    /// Siapkan keystore pada path ( file belum dibuat sampai `save()` dipanggil ).
    pub fn open_or_create(path: impl AsRef<Path>) -> Result<Self, KeystoreError> {
        let path = path.as_ref().to_path_buf();
        Ok(Self {
            path_str: path.display().to_string(),
            path,
            wallets: Vec::new(),
        })
    }

    /// Buka keystore yang ada dengan passphrase.
    pub fn open(
        path: impl AsRef<Path>,
        passphrase: &Secret<Vec<u8>>,
    ) -> Result<Self, KeystoreError> {
        let path = path.as_ref().to_path_buf();
        let raw = std::fs::read(&path).map_err(|e| KeystoreError::Read {
            path: path.display().to_string(),
            source: e,
        })?;
        if raw.len() < MAGIC.len() + 1 + SALT_LEN + NONCE_LEN {
            return Err(KeystoreError::InvalidFile {
                path: path.display().to_string(),
            });
        }
        let (header, rest) = raw.split_at(MAGIC.len() + 1);
        if &header[..MAGIC.len()] != MAGIC || header[MAGIC.len()] != VERSION {
            return Err(KeystoreError::InvalidFile {
                path: path.display().to_string(),
            });
        }
        let (salt, rest) = rest.split_at(SALT_LEN);
        let (nonce, ct) = rest.split_at(NONCE_LEN);

        let key = derive_key(passphrase, salt);
        let cipher = XChaCha20Poly1305::new((&key).into());
        let plaintext = cipher.decrypt(XNonce::from_slice(nonce), ct).map_err(|_| {
            KeystoreError::WrongPassphrase {
                path: path.display().to_string(),
            }
        })?;
        zeroize_key(key);

        let mut metas: Vec<WalletMeta> =
            serde_json::from_slice(&plaintext).map_err(|e| KeystoreError::InvalidPrivateKey {
                source: Box::new(e),
            })?;
        let mut wallets = Vec::with_capacity(metas.len());
        for m in metas.iter_mut() {
            let id = hash_id(&m.address, &m.label);
            let label = std::mem::take(&mut m.label);
            let secret = std::mem::take(&mut m.secret);
            wallets.push(OpenWallet::from_raw_key(id, label, secret)?);
        }
        Ok(Self {
            path_str: path.display().to_string(),
            path,
            wallets,
        })
    }

    pub fn wallets(&self) -> &[OpenWallet] {
        &self.wallets
    }

    pub fn wallet_by_id(&self, id: &str) -> Result<&OpenWallet, KeystoreError> {
        self.wallets
            .iter()
            .find(|w| w.id == id)
            .ok_or_else(|| KeystoreError::WalletNotFound {
                wallet_id: id.to_string(),
            })
    }

    pub fn add(&mut self, wallet: OpenWallet) -> Result<(), KeystoreError> {
        if self.wallets.iter().any(|w| w.id == wallet.id) {
            return Err(KeystoreError::DuplicateWalletId {
                wallet_id: wallet.id.clone(),
            });
        }
        self.wallets.push(wallet);
        Ok(())
    }

    /// Simpan keystore ke disk dengan enkripsi ( atomic: tulis tmp lalu rename ).
    pub fn save(&self, passphrase: &Secret<Vec<u8>>) -> Result<(), KeystoreError> {
        let metas: Vec<WalletMeta> = self
            .wallets
            .iter()
            .map(|w| WalletMeta {
                address: w.address.clone(),
                label: w.label.clone(),
                secret: w.secret.expose().clone(),
            })
            .collect();
        let json = serde_json::to_vec(&metas).map_err(|e| KeystoreError::InvalidPrivateKey {
            source: Box::new(e),
        })?;

        let mut salt = [0u8; SALT_LEN];
        OsRng.fill_bytes(&mut salt);
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);

        let key = derive_key(passphrase, &salt);
        let cipher = XChaCha20Poly1305::new((&key).into());
        let ct = cipher
            .encrypt(XNonce::from_slice(&nonce), json.as_ref())
            .map_err(|_| KeystoreError::Write {
                path: self.path.display().to_string(),
                source: std::io::Error::other("enkripsi gagal"),
            })?;
        zeroize_key(key);

        let mut out = Vec::with_capacity(MAGIC.len() + 1 + SALT_LEN + NONCE_LEN + ct.len());
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&salt);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);

        let tmp_path = self.path.with_extension("tmp");
        std::fs::write(&tmp_path, &out).map_err(|e| KeystoreError::Write {
            path: tmp_path.display().to_string(),
            source: e,
        })?;
        std::fs::rename(&tmp_path, &self.path).map_err(|e| KeystoreError::Write {
            path: self.path.display().to_string(),
            source: e,
        })?;
        Ok(())
    }
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

fn zeroize_key(mut key: [u8; 32]) {
    key.zeroize();
}

fn hash_id(address: &str, label: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut s = DefaultHasher::new();
    address.hash(&mut s);
    label.hash(&mut s);
    format!("wallet-{:016x}", s.finish())
}

pub fn default_path() -> std::path::PathBuf {
    std::path::PathBuf::from("storage/keystore.bin")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn ks_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("ks.bin")
    }

    fn random_key() -> Vec<u8> {
        let mut buf = vec![0u8; 32];
        OsRng.fill_bytes(&mut buf);
        buf
    }

    #[test]
    fn open_or_create_path_baru_menghasilkan_keystore_kosong() {
        let dir = tempdir().unwrap();
        let path = ks_path(&dir);
        let ks = Keystore::open_or_create(&path).unwrap();
        assert_eq!(ks.wallets().len(), 0);
        assert!(!path.exists());
    }

    #[test]
    fn add_wallet_lalu_save_dan_reload() {
        let dir = tempdir().unwrap();
        let path = ks_path(&dir);
        let raw = random_key();

        let mut ks = Keystore::open_or_create(&path).unwrap();
        ks.add(OpenWallet::from_raw_key("burner-1", "Burner 1", raw.clone()).unwrap())
            .unwrap();

        let passphrase = Secret::new(b"pass-satu-satunya".to_vec());
        ks.save(&passphrase).unwrap();

        let ks2 = Keystore::open(&path, &passphrase).unwrap();
        assert_eq!(ks2.wallets().len(), 1);
        let w = ks2.wallets().first().unwrap();
        assert_eq!(w.label, "Burner 1");
        let signer = PrivateKeySigner::from_slice(&raw).unwrap();
        assert_eq!(w.address, signer.address().to_string());
        assert_eq!(w.signer().unwrap().address(), signer.address());
    }

    #[test]
    fn passphrase_salah_ditolak() {
        let dir = tempdir().unwrap();
        let path = ks_path(&dir);
        let mut ks = Keystore::open_or_create(&path).unwrap();
        ks.add(OpenWallet::from_raw_key("burner-1", "Burner 1", random_key()).unwrap())
            .unwrap();
        let pass = Secret::new(b"pass-benar".to_vec());
        ks.save(&pass).unwrap();

        let wrong = Secret::new(b"pass-salah".to_vec());
        let err = Keystore::open(&path, &wrong).unwrap_err();
        assert!(err.to_string().contains("passphrase salah"));
    }

    #[test]
    fn file_biasa_dan_file_corrupt_ditolak() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bukan.bin");
        std::fs::write(&path, b"{garbage json luar biasa}").unwrap();
        let pass = Secret::new(b"x".to_vec());
        let err = Keystore::open(&path, &pass).unwrap_err();
        assert!(err.to_string().contains("bukan file keystore valid"));

        // header benar tapi ciphertext terpotong
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.push(VERSION);
        bad.extend_from_slice(&[0u8; SALT_LEN + NONCE_LEN]);
        bad.extend_from_slice(&[0u8; 5]);
        std::fs::write(&path, &bad).unwrap();
        let err = Keystore::open(&path, &pass).unwrap_err();
        assert!(err.to_string().contains("passphrase salah"));
    }

    #[test]
    fn file_keystore_tidak_mengandung_plaintext_key() {
        let dir = tempdir().unwrap();
        let path = ks_path(&dir);
        let raw = random_key();
        let hex = hex::encode(&raw);

        let mut ks = Keystore::open_or_create(&path).unwrap();
        ks.add(OpenWallet::from_raw_key("burner-1", "Burner 1", raw).unwrap())
            .unwrap();
        let pass = Secret::new(b"pass".to_vec());
        ks.save(&pass).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes.windows(hex.len()).any(|w| w == hex.as_bytes()));
        assert!(!bytes.windows(64).any(|w| w == hex.as_bytes()));
    }

    #[test]
    fn wallet_id_duplikat_ditolak() {
        let mut ks = Keystore::open_or_create("ignore").unwrap();
        let a = OpenWallet::from_raw_key("id", "label", random_key()).unwrap();
        let b = OpenWallet::from_raw_key("id", "label lain", random_key()).unwrap();
        ks.add(a).unwrap();
        let err = ks.add(b).unwrap_err();
        assert!(err.to_string().contains("duplikat"));
    }

    #[test]
    fn wallet_by_id() {
        let dir = tempdir().unwrap();
        let path = ks_path(&dir);
        let mut ks = Keystore::open_or_create(&path).unwrap();
        ks.add(OpenWallet::from_raw_key("burner-1", "Burner 1", random_key()).unwrap())
            .unwrap();
        let w = ks.wallet_by_id("burner-1").unwrap();
        assert_eq!(w.label, "Burner 1");
    }

    #[test]
    fn wallet_by_id_yang_tidak_ada_gagal() {
        let ks = Keystore::open_or_create("ignore").unwrap();
        let err = ks.wallet_by_id("tidak-ada").unwrap_err();
        assert!(err.to_string().contains("tidak ditemukan"));
    }

    #[test]
    fn secret_tidak_muncul_di_debug() {
        let raw = random_key();
        let w = OpenWallet::from_raw_key("burner-1", "Burner 1", raw).unwrap();
        let debug_str = format!("{:?}", w.secret);
        assert_eq!(debug_str, "Secret([REDACTED])");
    }

    #[test]
    fn default_path_adalah_storage_keystore_bin() {
        let p = default_path();
        assert_eq!(p.to_str().unwrap(), "storage/keystore.bin");
    }

    #[test]
    fn reload_setelah_tambah_wallet_kedua() {
        let dir = tempdir().unwrap();
        let path = ks_path(&dir);
        let pass = Secret::new(b"pass".to_vec());

        let mut ks = Keystore::open_or_create(&path).unwrap();
        ks.add(OpenWallet::from_raw_key("a", "A", random_key()).unwrap())
            .unwrap();
        ks.save(&pass).unwrap();

        let mut ks2 = Keystore::open(&path, &pass).unwrap();
        assert_eq!(ks2.wallets().len(), 1);
        ks2.add(OpenWallet::from_raw_key("b", "B", random_key()).unwrap())
            .unwrap();
        ks2.save(&pass).unwrap();

        let ks3 = Keystore::open(&path, &pass).unwrap();
        assert_eq!(ks3.wallets().len(), 2);
    }
}
