//! Nonce manager atomik per wallet ( blueprint §8.3 `reserve_nonces`, §6.2
//! "Nonce bentrok / nyangkut" ).
//!
//! Tx pre-signed butuh nonce yang **di-reservasi** sebelum dikirim supaya
//! tidak bentrok saat beberapa tx dari wallet yang sama dibuat hampir
//! bersamaan. Reservasi adalah sumber nonce yang **monoton naik** per wallet:
//! - `sync_from(addr, n)` → isi nonce awal dari `eth_getTransactionCount`
//!   (mode `pending`) — dilakukan saat preflight (§8.2 poin 4 "nonce sudah
//!   di-cache").
//! - `reserve(addr)` → nonce berikutnya yang aman dipakai, lalu menaikkannya
//!   satu tingkat. Nonce di-reservasi **sebelum** tx ditandatangani (§F1
//!   "Tx disiapkan sebelumnya").
//! - `release(addr, n)` → kembalikan nonce bila tx batal / gagal kirim.
//!
//! ponytail: ini manajemen nonce **lokal**. Penanganan tx pending yang
//! nyangkut di mempool ( cancel tx `nonce=n` dengan gas tinggi ) adalah
//! tugas broadcaster Fase 5, bukan di sini.

use std::collections::HashMap;
use std::sync::Arc;

use alloy::primitives::Address;
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::debug;

#[derive(Debug, Error)]
pub enum NonceError {
    #[error("nonce on-chain tidak bisa dibaca untuk {0}: {1}")]
    Fetch(String, String),

    #[error("nonce lokal ({local}) lebih kecil dari on-chain ({on_chain}) — sinkron ulang")]
    Behind { local: u64, on_chain: u64 },
}

/// Reservasi nonce untuk satu wallet.
#[derive(Debug, Clone, Copy)]
pub struct NonceReservation {
    pub wallet: Address,
    pub nonce: u64,
}

/// Manajemen nonce per wallet. Clone-able ( state berbagi via `Arc` ).
#[derive(Debug, Clone)]
pub struct NonceManager {
    inner: Arc<Mutex<HashMap<Address, u64>>>,
}

impl NonceManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Reservasi nonce berikutnya untuk `wallet`. Monoton naik: pemanggilan
    /// berturut-turut mengembalikan n, n+1, n+2, ... untuk wallet yang sama.
    pub async fn reserve(&self, wallet: Address) -> Result<NonceReservation, NonceError> {
        let mut g = self.inner.lock().await;
        let entry = g.entry(wallet).or_insert(0);
        let nonce = *entry;
        *entry += 1;
        debug!(?wallet, nonce, "nonce di-reservasi");
        Ok(NonceReservation { wallet, nonce })
    }

    /// Isi nonce awal untuk `wallet` ( dari `eth_getTransactionCount(pending)`
    /// atau `eth_getTransactionCount(latest)` + jumlah tx pending lokal ).
    /// Bila lokal sudah lebih besar dari `on_chain`, tidak menimpa ( supaya
    /// reservasi yang sudah berjalan tidak rusak ).
    pub async fn sync_from(&self, wallet: Address, on_chain: u64) {
        let mut g = self.inner.lock().await;
        let entry = g.entry(wallet).or_insert(on_chain);
        if *entry < on_chain {
            *entry = on_chain;
        }
    }

    /// Kembalikan nonce yang di-reservasi bila tx tidak jadi dikirim.
    /// Hanya nonce **terakhir** yang bisa dikembalikan ( nonce harus
    /// monoton; nonce di tengah dibiarkan — gap menjadi tanggung jawab
    /// broadcaster, §6.2 "cancel tx nyangkut" ).
    pub async fn release(&self, wallet: Address, nonce: u64) {
        let mut g = self.inner.lock().await;
        if let Some(current) = g.get(&wallet).copied() {
            if nonce + 1 == current {
                g.insert(wallet, nonce);
            }
        }
    }

    /// Berapa nonce berikutnya yang akan di-reservasi ( tanpa reservasi ).
    pub async fn next_nonce(&self, wallet: Address) -> u64 {
        self.inner.lock().await.get(&wallet).copied().unwrap_or(0)
    }

    /// Reset state nonce satu wallet ( setelah restart / sinkronisasi ulang ).
    pub async fn reset(&self, wallet: Address) {
        self.inner.lock().await.remove(&wallet);
    }
}

impl Default for NonceManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reservasi_monoton_naik() {
        let m = NonceManager::new();
        let a = Address::ZERO;
        assert_eq!(m.reserve(a).await.unwrap().nonce, 0);
        assert_eq!(m.reserve(a).await.unwrap().nonce, 1);
        assert_eq!(m.reserve(a).await.unwrap().nonce, 2);
        assert_eq!(m.next_nonce(a).await, 3);
    }

    #[tokio::test]
    async fn wallet_terpisah_monoton_sendiri() {
        let m = NonceManager::new();
        let a = Address::ZERO;
        let b = Address::from_slice(&[0x11; 20]);
        assert_eq!(m.reserve(a).await.unwrap().nonce, 0);
        assert_eq!(m.reserve(b).await.unwrap().nonce, 0);
        assert_eq!(m.reserve(a).await.unwrap().nonce, 1);
        assert_eq!(m.reserve(b).await.unwrap().nonce, 1);
    }

    #[tokio::test]
    async fn sync_from_menaikkan_tapi_tidak_menurunkan() {
        let m = NonceManager::new();
        let a = Address::ZERO;
        m.sync_from(a, 5).await;
        assert_eq!(m.next_nonce(a).await, 5);
        m.sync_from(a, 3).await; // tidak menimpa ke bawah
        assert_eq!(m.next_nonce(a).await, 5);
    }

    #[tokio::test]
    async fn release_nonce_terakhir_dipakai_ulang() {
        let m = NonceManager::new();
        let a = Address::ZERO;
        let r0 = m.reserve(a).await.unwrap();
        let r1 = m.reserve(a).await.unwrap();
        assert_eq!((r0.nonce, r1.nonce), (0, 1));
        m.release(a, r1.nonce).await; // kembalikan 1
        assert_eq!(m.reserve(a).await.unwrap().nonce, 1);
    }

    #[tokio::test]
    async fn release_nonce_tengah_tidak_mengubah_rantai() {
        let m = NonceManager::new();
        let a = Address::ZERO;
        let r0 = m.reserve(a).await.unwrap(); // 0
        let _r1 = m.reserve(a).await.unwrap(); // 1
        let _r2 = m.reserve(a).await.unwrap(); // 2
        m.release(a, r0.nonce).await; // 0 di tengah → diabaikan
        assert_eq!(m.next_nonce(a).await, 3);
    }

    #[tokio::test]
    async fn reset_menghapus_state_wallet() {
        let m = NonceManager::new();
        let a = Address::ZERO;
        m.sync_from(a, 10).await;
        m.reset(a).await;
        assert_eq!(m.next_nonce(a).await, 0);
    }
}
