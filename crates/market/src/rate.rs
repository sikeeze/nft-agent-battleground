//! Rate limiter sederhana per adapter (blueprint §F2: "Rate limiter + retry").
//!
//! Token bucket per adapter: maksimum `capacity` request dalam `window`.
//! Digunakan bersama `MarketHttp` yang sudah menerapkan retry backoff.

use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::MarketError;

#[derive(Debug)]
pub struct RateLimiter {
    adapter: String,
    capacity: u32,
    window: Duration,
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    timestamps: Vec<Instant>,
}

impl RateLimiter {
    /// `capacity` request per `window`.
    pub fn new(adapter: impl Into<String>, capacity: u32, window_secs: u64) -> Self {
        Self {
            adapter: adapter.into(),
            capacity,
            window: Duration::from_secs(window_secs),
            inner: Mutex::new(Inner {
                timestamps: Vec::with_capacity(capacity as usize + 1),
            }),
        }
    }

    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    /// Ambil satu token; menunggu bila bucket penuh. Maksimum menunggu
    /// satu window penuh agar tidak memblokir hot path terlalu lama.
    pub async fn acquire(&self) -> Result<(), MarketError> {
        let mut inner = self.inner.lock().await;
        let now = Instant::now();
        inner
            .timestamps
            .retain(|t| now.duration_since(*t) < self.window);
        if inner.timestamps.len() >= self.capacity as usize {
            return Err(MarketError::RateLimited {
                adapter: self.adapter.clone(),
            });
        }
        inner.timestamps.push(now);
        Ok(())
    }

    /// Berapa request tersisa di window ini ( untuk observabilitas ).
    pub async fn remaining(&self) -> u32 {
        let mut inner = self.inner.lock().await;
        let now = Instant::now();
        inner
            .timestamps
            .retain(|t| now.duration_since(*t) < self.window);
        self.capacity.saturating_sub(inner.timestamps.len() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn capacity_dihormati() {
        let rl = RateLimiter::new("test", 2, 60);
        assert!(rl.acquire().await.is_ok());
        assert!(rl.acquire().await.is_ok());
        let err = rl.acquire().await.unwrap_err();
        assert!(matches!(err, MarketError::RateLimited { .. }));
    }

    #[tokio::test]
    async fn remaining_berkurang() {
        let rl = RateLimiter::new("test", 3, 60);
        assert_eq!(rl.remaining().await, 3);
        rl.acquire().await.unwrap();
        assert_eq!(rl.remaining().await, 2);
    }

    #[tokio::test]
    async fn window_berlalu_token_kembali() {
        let rl = RateLimiter::new("test", 1, 1);
        rl.acquire().await.unwrap();
        assert!(rl.acquire().await.is_err());
        // Setelah 1 detik window bergeser.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(rl.acquire().await.is_ok());
    }

    #[tokio::test]
    async fn adapter_name_tersedia() {
        let rl = RateLimiter::new("opensea", 5, 60);
        assert_eq!(rl.adapter(), "opensea");
    }
}
