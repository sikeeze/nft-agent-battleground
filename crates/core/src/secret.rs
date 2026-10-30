//! `Secret<T>` — pembungkus nilai rahasia (blueprint §F4).
//!
//! Aturan keras:
//! - Tidak ada `Clone`/`Copy` (profil: kunci tidak pernah diduplikasi).
//! - `Debug`/`Display` selalu menampilkan `[REDACTED]` — tidak mungkin ter-log.
//! - Nilai di-nol-kan (`zeroize`) saat nilai dibuang.
//! - Tidak ada impl `Serialize` — rahasia tidak boleh di-serialize apa pun formatnya.

use std::fmt;
use std::ops::{Deref, DerefMut};
use zeroize::Zeroize;

pub struct Secret<T: Zeroize> {
    inner: Box<T>,
}

impl<T: Zeroize> Secret<T> {
    pub fn new(value: T) -> Self {
        Self {
            inner: Box::new(value),
        }
    }

    /// Akses ke plaintext. Hanya untuk operasi yang benar-benar membutuhkan
    /// (signing, enkripsi keystore). Hasil akses TIDAK BOLEH di-log atau di-copy keluar.
    pub fn expose(&self) -> &T {
        &self.inner
    }

    pub fn expose_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: Zeroize> Drop for Secret<T> {
    fn drop(&mut self) {
        self.inner.zeroize();
    }
}

impl<T: Zeroize> From<T> for Secret<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: Zeroize> Deref for Secret<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: Zeroize> DerefMut for Secret<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: Zeroize> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret([REDACTED])")
    }
}

impl<T: Zeroize> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_dan_display_selalu_meredaksi() {
        let s = Secret::new("mnemonic rahasia 12 kata di sini".to_string());
        assert!(!format!("{s:?}").contains("rahasia"));
        assert!(!format!("{s}").contains("rahasia"));
        assert_eq!(format!("{s:?}"), "Secret([REDACTED])");
        assert_eq!(format!("{s}"), "[REDACTED]");
    }

    #[test]
    fn expose_memberi_akses_lalu_zeroize_saat_manual() {
        let mut s = Secret::new("super-secret".to_string());
        assert_eq!(s.expose().as_str(), "super-secret");
        assert_eq!(s.as_str(), "super-secret"); // via Deref

        // Zeroize manual (publik via expose_mut) harus membersihkan memori.
        let buf: &mut String = s.expose_mut();
        buf.zeroize();
        assert!(s.expose().is_empty());
    }

    #[test]
    fn from_memungkinkan_konversi_rapi() {
        let s = Secret::from(vec![0x41u8, 0x42, 0x43]);
        assert_eq!(s.expose().len(), 3);
    }

    // CATATAN: `Secret` sengaja TIDAK mengimplementasi Clone/Copy — duplikasi
    // rahasia tidak diizinkan (lihat doc di atas). Tidak ada assert compile-time
    // untuk itu (negative trait bound masih unstable), jadi jaga jangan pernah
    // menambahkan impl Clone pada tipe ini.
}
