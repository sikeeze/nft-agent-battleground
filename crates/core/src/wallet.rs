//! `WalletRef` — referensi wallet GPS-tipe (id + label + alamat).
//!
//! Kunci privat TIDAK ADA DI SINI. Kunci hanya hidup di crate `keystore`
//! (terenkripsi Argon2id + XChaCha20) dan hanya menyentuh signer lewat
//! `ApprovedTx`. Objek di file ini aman di-log: tidak memuat rahasia.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletRef {
    pub id: String,
    pub label: String,
    /// Alamat publik — bukan rahasia, aman ditampilkan di Telegram.
    pub address: String,
}

impl WalletRef {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        address: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            address: address.into(),
        }
    }

    /// Alamat disingkat untuk tampilan notifikasi (blueprint §3C poin 4):
    /// `0xAb..12` — tidak ada perintah untuk membocorkan alamat panjang di chat.
    pub fn short_address(&self) -> String {
        let a = self.address.as_str();
        if a.len() <= 10 {
            a.to_string()
        } else {
            format!("{}..{}", &a[..6], &a[a.len() - 2..])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_address_menyingkat() {
        let w = WalletRef::new("burner-1", "Burner 1", "0xAbCdEf1234567890Ff12");
        assert_eq!(w.short_address(), "0xAbCd..12");
    }

    #[test]
    fn short_address_pendek_tetap_utuh() {
        let w = WalletRef::new("w", "w", "0x123");
        assert_eq!(w.short_address(), "0x123");
    }
}
