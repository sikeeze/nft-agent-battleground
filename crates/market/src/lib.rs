//! `nft-market` — adapter marketplace seragam (blueprint §F2, §9 Fase 8).
//!
//! Interface `Marketplace` menyatukan operasi lintas marketplace:
//! `get_floor`, `get_listings`, `buy`, `list`, `bid`, `cancel`.
//! Adapter pertama: **OpenSea (Seaport)** via REST API v2 + order EIP-712.
//!
//! Aturan:
//! - `buy`/`list`/`bid`/`cancel` adalah **side-effect** → wajib lewat Policy
//!   Engine di lapis pemanggil ( aturan keras #2 ). Crate ini hanya
//!   menyusun permintaan + menandatangani order via signer ApprovedTx.
//! - Rate limiter per adapter ( §F2 "Rate limiter + retry" ) + retry
//!   exponential backoff.
//! - Konten API ( nama koleksi, deskripsi ) = data tidak tepercaya ( §F7 );
//!   tidak pernah dipakai untuk menyusun intent final.

use std::time::Duration;

use alloy::primitives::{Address, U256};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod opensea;
pub mod rate;

pub use opensea::OpenSeaAdapter;
pub use rate::RateLimiter;

/// Error marketplace ( seragam lintas adapter ).
#[derive(Debug, Error)]
pub enum MarketError {
    #[error("error jaringan: {0}")]
    Network(#[from] reqwest::Error),

    #[error("error parse respons: {0}")]
    Parse(#[from] serde_json::Error),

    #[error("rate limit terlampaui untuk {adapter}; coba lagi nanti")]
    RateLimited { adapter: String },

    #[error("koleksi {collection} tidak ditemukan di {adapter}")]
    CollectionNotFound { adapter: String, collection: String },

    #[error("listing {order_id} tidak ditemukan / sudah terbeli")]
    ListingNotFound { order_id: String },

    #[error("parameter tidak valid: {0}")]
    InvalidParam(String),

    #[error("error HTTP {status} dari {adapter}: {body}")]
    Http {
        status: u16,
        adapter: String,
        body: String,
    },

    #[error("error signer saat menyusun order: {0}")]
    Signer(#[from] nft_signer::SignerError),

    #[error("error lain: {0}")]
    Other(String),
}

/// Item listing NFT ( representasi seragam lintas marketplace ).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listing {
    /// Identifier unik order di marketplace ( digunakan untuk buy/cancel ).
    pub order_id: String,
    pub collection: Address,
    pub token_id: U256,
    /// Harga dalam wei ( native atau wrapped — adapter menentukan ).
    pub price: U256,
    /// Penjual.
    pub seller: Address,
    /// Chain id listing.
    pub chain_id: u64,
}

impl Listing {
    /// Harga dalam ETH ( untuk tampilan / perhitungan PnL ).
    pub fn price_eth(&self) -> f64 {
        wei_to_eth(self.price)
    }
}

/// Hasil query floor price.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FloorPrice {
    pub collection: Address,
    /// Floor terendah saat ini dalam wei.
    pub price: U256,
    pub chain_id: u64,
}

impl FloorPrice {
    pub fn price_eth(&self) -> f64 {
        wei_to_eth(self.price)
    }
}

/// Parameter membuat listing ( sell ).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListParams {
    pub collection: Address,
    pub token_id: U256,
    /// Harga dalam wei.
    pub price: U256,
    /// Berapa lama listing berlaku ( detik ).
    pub duration_secs: u64,
    pub chain_id: u64,
}

/// Parameter membuat bid ( tawaran ).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BidParams {
    pub collection: Address,
    pub token_id: U256,
    /// Tawaran dalam wei.
    pub price: U256,
    pub duration_secs: u64,
    pub chain_id: u64,
}

/// Order siap-tanda-tangan ( EIP-712 untuk Seaport, dll ). Diturunkan ke
/// adapter untuk format konkret. Order **belum ditandatangani** di sini —
/// pemanggil wajib menyuruh signer menandatangani via Policy Engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnsignedOrder {
    /// Adapter spesifik ( mis. Seaport order parameters ).
    pub adapter: String,
    pub chain_id: u64,
    /// Payload order mentah ( JSON ) siap diproses adapter.
    pub payload: serde_json::Value,
}

/// Trait seragam marketplace ( §F2: "Adapter marketplace seragam" ).
///
/// `buy`/`list`/`bid`/`cancel` mengembalikan `UnsignedOrder` yang harus
/// dilewati Policy Engine sebelum di-submit — tidak ada tx/sign langsung.
#[async_trait]
pub trait Marketplace: Send + Sync {
    /// Nama adapter ( "opensea", "blur", ... ).
    fn name(&self) -> &str;

    /// Floor price koleksi.
    async fn get_floor(&self, collection: Address) -> Result<FloorPrice, MarketError>;

    /// Daftar listing aktif koleksi ( termurah duluan ).
    async fn get_listings(&self, collection: Address) -> Result<Vec<Listing>, MarketError>;

    /// Susun order beli untuk listing. Belum ditandatangani.
    async fn buy(&self, listing: &Listing, buyer: Address) -> Result<UnsignedOrder, MarketError>;

    /// Susun order listing ( sell ) untuk token milik `seller`.
    async fn list(
        &self,
        params: &ListParams,
        seller: Address,
    ) -> Result<UnsignedOrder, MarketError>;

    /// Susun order bid untuk token.
    async fn bid(&self, params: &BidParams, bidder: Address) -> Result<UnsignedOrder, MarketError>;

    /// Susun permintaan cancel order.
    async fn cancel(&self, order_id: &str) -> Result<UnsignedOrder, MarketError>;
}

/// Permintaan HTTP bersama ( HTTP/2, keep-alive, §8.2 poin 5 ). Adapter
/// memakai client ini supaya connection pool terbagi.
pub struct MarketHttp {
    client: reqwest::Client,
    limiter: RateLimiter,
    timeout: Duration,
}

impl MarketHttp {
    pub fn new(name: impl Into<String>, timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .https_only(true)
            .build()
            .expect("reqwest client valid");
        Self {
            client,
            limiter: RateLimiter::new(name, 5, 120),
            timeout,
        }
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn limiter(&self) -> &RateLimiter {
        &self.limiter
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// GET dengan rate limit; menerapkan retry backoff sederhana.
    pub async fn get(&self, url: &str) -> Result<reqwest::Response, MarketError> {
        self.limiter.acquire().await?;
        let mut backoff = Duration::from_millis(250);
        let mut last: Option<MarketError> = None;
        for attempt in 0..3u32 {
            let res = self
                .client
                .get(url)
                .header("User-Agent", "nft-agent/1 (market adapter)")
                .send()
                .await;
            match res {
                Ok(r) => {
                    let status = r.status().as_u16();
                    if status == 429 {
                        // Rate limit dari server; tunggu lalu coba lagi.
                        last = Some(MarketError::RateLimited {
                            adapter: self.limiter.adapter().to_string(),
                        });
                    } else if status >= 500 && attempt < 2 {
                        // Server error; retry.
                        let body = r.text().await.unwrap_or_default();
                        last = Some(MarketError::Http {
                            status,
                            adapter: self.limiter.adapter().to_string(),
                            body,
                        });
                    } else if status >= 400 {
                        let body = r.text().await.unwrap_or_default();
                        return Err(MarketError::Http {
                            status,
                            adapter: self.limiter.adapter().to_string(),
                            body,
                        });
                    } else {
                        return Ok(r);
                    }
                }
                Err(e) => {
                    if attempt == 2 || !e.is_timeout() {
                        return Err(MarketError::Network(e));
                    }
                    last = Some(MarketError::Network(e));
                }
            }
            tokio::time::sleep(backoff).await;
            backoff *= 2;
        }
        Err(last.unwrap_or_else(|| MarketError::Other("retry habis".into())))
    }
}

pub fn wei_to_eth(wei: U256) -> f64 {
    let s = wei.to_string();
    let bytes = s.as_bytes();
    if bytes.len() <= 18 {
        let v: f64 = s.parse().unwrap_or(0.0);
        v / 1e18
    } else {
        let whole = &s[..bytes.len() - 18];
        let frac = &s[bytes.len() - 18..];
        let w: f64 = whole.parse().unwrap_or(0.0);
        let f: f64 = format!("0.{frac}").parse().unwrap_or(0.0);
        w + f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wei_to_eth_konversi() {
        assert_eq!(wei_to_eth(U256::from(1_000_000_000_000_000_000u128)), 1.0);
        assert_eq!(
            wei_to_eth(U256::from(10u128 * 1_000_000_000_000_000u128)),
            0.01
        );
        assert_eq!(wei_to_eth(U256::ZERO), 0.0);
    }

    #[test]
    fn listing_price_eth() {
        let l = Listing {
            order_id: "o1".into(),
            collection: Address::ZERO,
            token_id: U256::from(1),
            price: U256::from(2_000_000_000_000_000_000u128),
            seller: Address::ZERO,
            chain_id: 1,
        };
        assert!((l.price_eth() - 2.0).abs() < 1e-12);
    }

    #[test]
    fn floor_price_eth() {
        let f = FloorPrice {
            collection: Address::ZERO,
            price: U256::from(50_000_000_000_000_000u128),
            chain_id: 1,
        };
        assert!((f.price_eth() - 0.05).abs() < 1e-12);
    }

    #[tokio::test]
    async fn http_get_error_url_jelas() {
        let http = MarketHttp::new("test", Duration::from_millis(300));
        let err = http.get("http://127.0.0.1:9/x").await.unwrap_err();
        // Koneksinya ditolak → Network error, bukan panic.
        assert!(matches!(err, MarketError::Network(_)) || matches!(err, MarketError::Other(_)));
    }

    #[test]
    fn market_http_membuat_client() {
        let h = MarketHttp::new("opensea", Duration::from_secs(5));
        assert_eq!(h.timeout(), Duration::from_secs(5));
        assert_eq!(h.limiter().adapter(), "opensea");
    }
}
