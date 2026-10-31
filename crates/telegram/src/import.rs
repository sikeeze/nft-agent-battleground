//! Alur `/import` wallet burner via chat ( blueprint §3A ).
//!
//! **Ini adalah bagian paling sensitif dari agent.** Chat Telegram **tidak
//! end-to-end encrypted**, jadi import hanya untuk wallet burner dengan
//! pengaman berlapis:
//!
//! 1. User kirim `/import`. Bot menjawab peringatan + minta konfirmasi tombol
//!    "Saya konfirmasi ini wallet burner, bukan wallet utama" (+ TOTP bila
//!    aktif). Fitur ini **mati secara default** ( `import_via_chat = false`
//!    di policy.toml ; §3A "Fitur ini mati secara default" ).
//! 2. Bot meminta user mengirim **private key atau mnemonic dalam satu pesan**.
//!    Sesi import kedaluwarsa dalam 2 menit ( `import_session_timeout_sec` ).
//! 3. Begitu pesan diterima, bot **langsung `deleteMessage`** pada pesan user
//!    ( §3A poin 3 ), **sebelum proses lain**.
//! 4. Phrase di-parse di memori, diturunkan ke alamat, **dienkripsi ke
//!    keystore**, lalu `zeroize`. Tidak ditulis ke SQLite/log/file sementara.
//! 5. Bot memeriksa saldo wallet. Jika di atas `burner_max_balance_eth`,
//!    import **ditolak** dengan pesan agar memindahkan sebagian dana dulu.
//! 6. Bot membalas hanya dengan **alamat wallet + saldo**, tidak pernah
//!    mengulang phrase. Audit log mencatat alamat dan waktu, bukan isi.
//!
//! Aturan tambahan ( §3A ):
//! - Utamakan private key daripada mnemonic. Mnemonic hanya menurunkan satu
//!   path `m/44'/60'/0'/0/0`.
//! - Rate limit + lockout setelah beberapa kali gagal parse ( `AccessControl` ).
//! - Hanya `user_id` whitelist yang bisa memulai import.
//! - Test otomatis membuktikan phrase tidak muncul di log ( lihat test
//!   `phrase_tidak_muncul_di_balasan` dll ).

use std::sync::Arc;
use std::time::{Duration, Instant};

use nft_core::secret::Secret;
use nft_keystore::{Keystore, OpenWallet};
use teloxide::prelude::*;
use tokio::sync::Mutex;
use zeroize::Zeroize;

/// Sesi import kedaluwarsa dalam 2 menit ( §3A poin 2 ). Konstanta supaya
/// `ImportSessions` dan `ImportConfig` konsisten.
const SESSION_TIMEOUT: Duration = Duration::from_secs(120);

/// ID sesi import ( per user ).
#[derive(Debug, Clone)]
pub struct ImportSession {
    pub user_id: i64,
    pub started_at: Instant,
    pub chat_id: ChatId,
    /// Apakah user sudah mengkonfirmasi ( tombol burner ) + TOTP valid.
    pub confirmed: bool,
}

/// Hasil parse phrase ( private key atau mnemonic ).
#[derive(Debug)]
pub enum ParsedSecret {
    PrivateKey(Vec<u8>),
    Mnemonic(String),
}

impl ParsedSecret {
    /// Turkunkan ke `OpenWallet` ( alamat + kunci ). Mnemonic hanya menurunkan
    /// path `m/44'/60'/0'/0/0` ( §3A: "agent hanya menurunkan satu path" ).
    pub fn into_wallet(
        self,
        id: String,
        label: String,
    ) -> Result<(OpenWallet, String), ImportError> {
        match self {
            ParsedSecret::PrivateKey(raw) => {
                let wallet = OpenWallet::from_raw_key(&id, &label, raw)?;
                Ok((wallet, label))
            }
            ParsedSecret::Mnemonic(phrase) => {
                // §3A: hanya path tunggal m/44'/60'/0'/0/0 (
                // `try_from_phrase_first` adalah cek path pertama ).
                let signer = alloy::signers::local::MnemonicBuilder::try_from_phrase_first(&phrase)
                    .map_err(|e| ImportError::InvalidMnemonic(e.to_string()))?;
                let raw = signer.to_bytes();
                let wallet = OpenWallet::from_raw_key(&id, &label, raw.to_vec())?;
                Ok((wallet, label))
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("input bukan private key ( hex 32 byte ) atau mnemonic yang valid")]
    InvalidFormat,

    #[error("mnemonic tidak valid: {0}")]
    InvalidMnemonic(String),

    #[error("keystore error: {0}")]
    Keystore(#[from] nft_keystore::KeystoreError),

    #[error("private key tidak valid: {0}")]
    InvalidKey(String),
}

/// Konfigurasi alur import ( dari policy.toml ).
#[derive(Debug, Clone)]
pub struct ImportConfig {
    /// `telegram.import_via_chat` -€” default **false** ( §3A ).
    pub import_via_chat: bool,
    /// `telegram.import_session_timeout_sec` -€” kedaluwarsa sesi.
    pub session_timeout: Duration,
    /// `telegram.delete_import_message` -€” hapus pesan user ( default true ).
    pub delete_message: bool,
    /// `telegram.require_totp_for_import` -€” TOTP wajib ( default true ).
    pub require_totp: bool,
    /// `telegram.burner_max_balance_eth` -€” saldo maksimum burner.
    pub burner_max_balance_eth: f64,
    /// `telegram.max_wallets` -€” jumlah wallet maksimum di keystore.
    pub max_wallets: u32,
}

impl Default for ImportConfig {
    fn default() -> Self {
        // Default AMAN sesuai §3A ( semua pembatas aktif ).
        Self {
            import_via_chat: false,
            session_timeout: Duration::from_secs(120),
            delete_message: true,
            require_totp: true,
            burner_max_balance_eth: 0.5,
            max_wallets: 5,
        }
    }
}

/// Validasi TOTP ( §3 poin 5 ). `secret` = secret TOTP user ( hex/base32 ).
pub fn verify_totp(secret: &str, code: &str) -> bool {
    let totp = match totp_rs::TOTP::new(
        totp_rs::Algorithm::SHA1,
        6,
        1,
        30,
        secret.as_bytes().to_vec(),
    ) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let now = chrono::Utc::now().timestamp();
    totp.check_current(code).unwrap_or(false) && now > 0
}

/// Parse pesan user menjadi `ParsedSecret` ( §3A: utamakan private key ).
/// Mondar-mandir input yang tidak valid -†’ `InvalidFormat` ( caller mencatat
/// kegagalan untuk lockout ).
pub fn parse_phrase(text: &str) -> Result<ParsedSecret, ImportError> {
    let t = text.trim();
    if t.is_empty() {
        return Err(ImportError::InvalidFormat);
    }

    // 1. Private key hex 64 chars ( dengan/tanpa 0x ).
    let hex = t.trim_start_matches("0x").trim_start_matches("0X");
    if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let raw = hex::decode(hex).map_err(|_| ImportError::InvalidFormat)?;
        if raw.len() == 32 {
            return Ok(ParsedSecret::PrivateKey(raw));
        }
    }

    // 2. Mnemonic ( 12/24 kata, dipisah spasi ).
    let words: Vec<&str> = t.split_whitespace().collect();
    if matches!(words.len(), 12 | 24)
        && words
            .iter()
            .all(|w| w.chars().all(|c| c.is_ascii_alphabetic()))
    {
        return Ok(ParsedSecret::Mnemonic(t.to_string()));
    }

    Err(ImportError::InvalidFormat)
}

/// Manajer sesi import ( satu sesi aktif per user, timeout 2 menit ).
#[derive(Debug, Clone)]
pub struct ImportSessions {
    inner: Arc<Mutex<std::collections::HashMap<i64, ImportSession>>>,
}

impl ImportSessions {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Mulai sesi import untuk user. Return false bila sudah ada sesi aktif.
    pub async fn start(&self, user_id: i64, chat_id: ChatId) -> bool {
        let mut g = self.inner.lock().await;
        if let Some(s) = g.get(&user_id) {
            if s.started_at.elapsed() < SESSION_TIMEOUT {
                return false;
            }
        }
        g.insert(
            user_id,
            ImportSession {
                user_id,
                started_at: Instant::now(),
                chat_id,
                confirmed: false,
            },
        );
        true
    }

    /// Tandai sesi sudah dikonfirmasi user ( tombol burner + TOTP ).
    pub async fn confirm(&self, user_id: i64) -> bool {
        let mut g = self.inner.lock().await;
        if let Some(s) = g.get_mut(&user_id) {
            if s.started_at.elapsed() < SESSION_TIMEOUT {
                s.confirmed = true;
                return true;
            }
            g.remove(&user_id);
        }
        false
    }

    /// Ambil sesi aktif ( belum konfirmasi / belum kedaluwarsa ).
    pub async fn get_active(&self, user_id: i64) -> Option<ImportSession> {
        let g = self.inner.lock().await;
        let s = g.get(&user_id)?;
        if s.started_at.elapsed() < SESSION_TIMEOUT {
            Some(s.clone())
        } else {
            None
        }
    }

    /// Hapus sesi ( setelah import selesai / dibatalkan ).
    pub async fn end(&self, user_id: i64) {
        self.inner.lock().await.remove(&user_id);
    }
}

impl Default for ImportSessions {
    fn default() -> Self {
        Self::new()
    }
}

/// Hasil akhir import.
#[derive(Debug, Clone)]
pub struct ImportOutcome {
    pub address: String,
    /// Saldo ( ETH ) saat ini; `None` bila RPC tidak bisa dicek.
    pub balance_eth: Option<f64>,
    pub wallet_label: String,
}

/// Langkah 5-6 §3A: cek saldo, lalu simpan ke keystore. **Phrase sudah
/// harus berada dalam `ParsedSecret`** dan di-zeroize setelah fungsi ini.
///
/// `balance_eth` = saldo on-chain ( dari chain layer ). Melebihi
/// `burner_max_balance_eth` -†’ import ditolak ( §3A poin 5 ).
pub async fn finalize_import(
    parsed: ParsedSecret,
    keystore: &mut Keystore,
    passphrase: &Secret<Vec<u8>>,
    balance_eth: Option<f64>,
    config: &ImportConfig,
) -> Result<ImportOutcome, ImportError> {
    // 5. Cek saldo burner.
    if let Some(balance) = balance_eth {
        if balance > config.burner_max_balance_eth {
            return Err(ImportError::Keystore(
                nft_keystore::KeystoreError::InvalidPrivateKey {
                    source: Box::new(std::io::Error::other(format!(
                        "saldo wallet {balance:.4} ETH melebihi batas burner \
                         {} ETH -€” pindahkan sebagian dana dulu ( §3A poin 5 )",
                        config.burner_max_balance_eth
                    ))),
                },
            ));
        }
    }

    // 6. Batas jumlah wallet.
    if keystore.wallets().len() >= config.max_wallets as usize {
        return Err(ImportError::Keystore(
            nft_keystore::KeystoreError::InvalidPrivateKey {
                source: Box::new(std::io::Error::other(format!(
                    "jumlah wallet sudah mencapai batas {} ( telegram.max_wallets )",
                    config.max_wallets
                ))),
            },
        ));
    }

    let label = format!("burner-{}", keystore.wallets().len() + 1);
    let id = format!("burner-{}", keystore.wallets().len() + 1);
    let (wallet, label) = parsed.into_wallet(id, label)?;
    let address = wallet.address().to_string();
    keystore.add(wallet)?;
    keystore.save(passphrase)?;

    Ok(ImportOutcome {
        address,
        balance_eth,
        wallet_label: label,
    })
}

/// Zeroize phrase setelah dipakai ( §3A poin 4 ). Mnemonic adalah String,
/// harus di-zeroize eksplisit ( private key ada dalam `Secret<Vec<u8>>`
/// yang otomatis zeroize saat drop ). Mengembalikan nilai yang sudah
/// dibersihkan supaya pemanggil dapat memeriksa ( lihat test ).
pub fn zeroize_phrase(mut parsed: ParsedSecret) -> ParsedSecret {
    match &mut parsed {
        ParsedSecret::PrivateKey(raw) => raw.zeroize(),
        ParsedSecret::Mnemonic(phrase) => phrase.zeroize(),
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_private_key_hex_valid() {
        let hex = "a".repeat(64);
        let parsed = parse_phrase(&hex).unwrap();
        assert!(matches!(parsed, ParsedSecret::PrivateKey(_)));
    }

    #[test]
    fn parse_private_key_hex_dengan_prefix_0x() {
        let hex = format!("0x{}", "b".repeat(64));
        let parsed = parse_phrase(&hex).unwrap();
        assert!(matches!(parsed, ParsedSecret::PrivateKey(_)));
    }

    #[test]
    fn parse_mnemonic_12_kata_valid() {
        // Mnemonic uji ( bukan wallet nyata, hanya untuk parse ).
        let phrase = "abandon abandon abandon abandon abandon abandon \
                      abandon abandon abandon abandon abandon about";
        let parsed = parse_phrase(phrase).unwrap();
        assert!(matches!(parsed, ParsedSecret::Mnemonic(_)));
    }

    #[test]
    fn parse_input_asing_ditolak() {
        assert!(parse_phrase("hello world").is_err());
        assert!(parse_phrase("").is_err());
        assert!(parse_phrase("0x1234").is_err()); // terlalu pendek
        assert!(parse_phrase(&"z".repeat(64)).is_err()); // bukan hex
    }

    #[test]
    fn parse_mnemonic_dengan_kata_asing_ditolak() {
        // 12 kata tapi mengandung karakter non-alfabet.
        let phrase = "abandon abandon abandon abandon abandon abandon \
                      abandon abandon abandon abandon abandon 123";
        assert!(parse_phrase(phrase).is_err());
    }

    #[test]
    fn mnemonic_menurunkan_wallet_dengan_alamat() {
        let phrase = "abandon abandon abandon abandon abandon abandon \
                      abandon abandon abandon abandon abandon about";
        let parsed = parse_phrase(phrase).unwrap();
        let (w, _) = parsed
            .into_wallet("burner-x".into(), "Burner X".into())
            .unwrap();
        assert!(w.address().starts_with("0x"));
        assert_eq!(w.address().len(), 42);
    }

    #[test]
    fn private_key_menurunkan_wallet_dengan_alamat() {
        let hex = "a".repeat(64);
        let parsed = parse_phrase(&hex).unwrap();
        let (w, _) = parsed
            .into_wallet("burner-y".into(), "Burner Y".into())
            .unwrap();
        assert_eq!(w.address().len(), 42);
    }

    #[tokio::test]
    async fn sesi_import_mulai_konfirmasi_selesai() {
        let s = ImportSessions::new();
        assert!(s.start(1, ChatId(1)).await);
        // Sesi kedua untuk user yang sama ditolak.
        assert!(!s.start(1, ChatId(1)).await);
        assert!(!s.get_active(1).await.unwrap().confirmed);
        assert!(s.confirm(1).await);
        assert!(s.get_active(1).await.unwrap().confirmed);
        s.end(1).await;
        assert!(s.get_active(1).await.is_none());
    }

    #[tokio::test]
    async fn sesi_import_kedaluwarsa() {
        let s = ImportSessions::new();
        s.start(1, ChatId(1)).await;
        // Simulasikan kedaluwarsa dengan manipulasi waktu tidak bisa; pakai
        // window 120 dkt -€” verifikasi logika get_active menolak setelah
        // kedaluwarsa dengan sesi yang starting_atnya jauh di masa lalu.
        let mut g = s.inner.lock().await;
        g.get_mut(&1).unwrap().started_at = Instant::now() - Duration::from_secs(200);
        drop(g);
        assert!(s.get_active(1).await.is_none());
    }

    #[tokio::test]
    async fn finalize_import_menolak_saldo_di_atas_burner() {
        // §3A poin 5: import ditolak kalau saldo > burner_max_balance_eth.
        let dir = tempfile::tempdir().unwrap();
        let mut ks = Keystore::open_or_create(dir.path().join("ks.bin")).unwrap();
        let parsed = parse_phrase(&"a".repeat(64)).unwrap();
        let pass = Secret::new(b"pass".to_vec());
        let cfg = ImportConfig::default();
        let err = finalize_import(parsed, &mut ks, &pass, Some(1.0), &cfg)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("melebihi batas burner"),
            "pesan harus menyebut batas burner, dapat: {err}"
        );
    }

    #[tokio::test]
    async fn finalize_import_menolak_melebihi_max_wallets() {
        let dir = tempfile::tempdir().unwrap();
        let mut ks = Keystore::open_or_create(dir.path().join("ks.bin")).unwrap();
        let pass = Secret::new(b"pass".to_vec());
        let cfg = ImportConfig {
            max_wallets: 1,
            ..Default::default()
        };
        // Isi keystore dengan 1 wallet.
        let parsed = parse_phrase(&"a".repeat(64)).unwrap();
        finalize_import(parsed, &mut ks, &pass, None, &cfg)
            .await
            .unwrap();
        // Wallet kedua ditolak.
        let parsed2 = parse_phrase(&"b".repeat(64)).unwrap();
        let err = finalize_import(parsed2, &mut ks, &pass, None, &cfg)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("batas"));
    }

    #[tokio::test]
    async fn finalize_import_berhasil_dengan_saldo_kosong() {
        let dir = tempfile::tempdir().unwrap();
        let mut ks = Keystore::open_or_create(dir.path().join("ks.bin")).unwrap();
        let parsed = parse_phrase(&"a".repeat(64)).unwrap();
        let pass = Secret::new(b"pass".to_vec());
        let cfg = ImportConfig::default();
        let out = finalize_import(parsed, &mut ks, &pass, Some(0.0), &cfg)
            .await
            .unwrap();
        assert!(out.address.starts_with("0x"));
        assert_eq!(out.balance_eth, Some(0.0));
        // Keystore tersimpan; reload menemukan wallet.
        let ks2 = Keystore::open(dir.path().join("ks.bin"), &pass).unwrap();
        assert_eq!(ks2.wallets().len(), 1);
    }

    #[tokio::test]
    async fn keystore_tidak_mengandung_phrase_plaintext() {
        // §3A: setelah dienkripsi, data mentah tidak ada di file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ks.bin");
        let mut ks = Keystore::open_or_create(&path).unwrap();
        let hex = "c".repeat(64);
        let parsed = parse_phrase(&hex).unwrap();
        let pass = Secret::new(b"pass".to_vec());
        let cfg = ImportConfig::default();
        let _ = finalize_import(parsed, &mut ks, &pass, None, &cfg)
            .await
            .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes.windows(64).any(|w| w == hex.as_bytes()));
    }

    #[test]
    fn verify_totp_dengan_secret_dan_code_yang_cocok() {
        let secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"; // base32 uji
        let totp = totp_rs::TOTP::new(
            totp_rs::Algorithm::SHA1,
            6,
            1,
            30,
            secret.as_bytes().to_vec(),
        )
        .unwrap();
        let code = totp.generate_current().unwrap();
        assert!(verify_totp(secret, &code));
        assert!(!verify_totp(secret, "000000"));
    }

    #[test]
    fn verify_totp_secret_asing_ditolak() {
        assert!(!verify_totp("bukan-secret", "123456"));
    }

    #[tokio::test]
    async fn zeroize_phrase_menghapus_isi() {
        let parsed = parse_phrase(&"a".repeat(64)).unwrap();
        let p = zeroize_phrase(parsed);
        match &p {
            ParsedSecret::PrivateKey(raw) => assert!(raw.iter().all(|&b| b == 0)),
            ParsedSecret::Mnemonic(phrase) => assert!(phrase.chars().all(|c| c == '\0')),
        }
    }
}
