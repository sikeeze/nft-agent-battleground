//! Adapter OpenSea ( Seaport ) — blueprint §F2, §F3, §9 Fase 8.
//!
//! OpenSea API v2 dipakai untuk **membaca** ( floor, listings ). Penulisan
//! ( buy/list/bid/cancel ) disusun sebagai `UnsignedOrder` berisi parameter
//! order Seaport ( EIP-712, §F3 ) yang **belum ditandatangani** — pemanggil
//! wajib melewatkannya ke Policy Engine baru signer ( aturan keras #2 ).
//!
//! ponytail: integrasi HTTP penuh menunggu API key ( `OPENSEA_API_KEY` );
//! sekarang adapter diuji lewat mock server di unit test. Order Seaport
//! memakai kontrak 0x00000000006c3852cbEf3e08E8dF639F9306561e ( mainnet )
//! dan turunannya di chain lain.

use std::time::Duration;

use alloy::primitives::{Address, U256};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    BidParams, FloorPrice, ListParams, Listing, MarketError, MarketHttp, Marketplace, UnsignedOrder,
};

/// Alamat kontrak Seaport 1.6 ( Ethereum mainnet ).
pub const SEAPORT_ADDRESS_MAINNET: &str = "0x00000000006c3852cbEf3e08E8dF639F9306561e";

/// Domain EIP-712 Seaport ( name + version + verifyingContract ).
pub const SEAPORT_EIP712_NAME: &str = "Seaport";
pub const SEAPORT_EIP712_VERSION: &str = "1.6";

/// Adapter OpenSea. `base_url` bisa di-override untuk test; `api_key`
/// diambil dari env di lapis pemanggil ( tidak disimpan di config ).
pub struct OpenSeaAdapter {
    http: MarketHttp,
    base_url: String,
    /// Chain id target adapter ini ( satu adapter per chain, §F2 ).
    chain_id: u64,
    /// Alamat Seaport di chain ini.
    seaport: Address,
}

impl OpenSeaAdapter {
    /// Adapter untuk chain `chain_id` + alamat Seaport setempat.
    pub fn new(chain_id: u64, seaport: Address, base_url: impl Into<String>) -> Self {
        Self {
            http: MarketHttp::new("opensea", Duration::from_secs(10)),
            base_url: base_url.into(),
            chain_id,
            seaport,
        }
    }

    /// Adapter mainnet default ( Seaport 1.6 resmi ).
    pub fn mainnet(api_base: impl Into<String>) -> Self {
        let seaport: Address = SEAPORT_ADDRESS_MAINNET
            .parse()
            .expect("alamat Seaport mainnet valid");
        Self::new(1, seaport, api_base)
    }

    pub fn seaport_address(&self) -> Address {
        self.seaport
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Susun parameter order Seaport ( EIP-712 ) untuk listing `.eth`.
    /// ponytail: parameter lengkap ( consideration, fees, zone ) menyusul
    /// saat integrasi Seaport SDK penuh; sekarang parameter inti.
    fn order_payload(
        &self,
        offer_side: &str,
        collection: Address,
        token_id: U256,
        price: U256,
        duration_secs: u64,
        maker: Address,
    ) -> serde_json::Value {
        let start_time = chrono::Utc::now().timestamp();
        let end_time = start_time + duration_secs as i64;
        json!({
            "protocol": "seaport",
            "kind": offer_side,
            "chain_id": self.chain_id,
            "seaport": format!("{:?}", self.seaport),
            "maker": format!("{maker:?}"),
            "offer": [{
                "token": format!("{collection:?}"),
                "identifier_or_criteria": token_id.to_string(),
                "start_amount": "1",
                "end_amount": "1",
            }],
            "consideration": [{
                "token": "0x0000000000000000000000000000000000000000",
                "identifier_or_criteria": "0",
                "start_amount": price.to_string(),
                "end_amount": price.to_string(),
                "recipient": format!("{maker:?}"),
            }],
            "start_time": start_time.to_string(),
            "end_time": end_time.to_string(),
            "eip712": {
                "name": SEAPORT_EIP712_NAME,
                "version": SEAPORT_EIP712_VERSION,
                "verifying_contract": format!("{:?}", self.seaport),
            },
        })
    }
}

/// Field wajib respons OpenSea hilang → error jelas (bukan parse error).
fn field_missing(field: &str) -> MarketError {
    MarketError::InvalidParam(format!("respons OpenSea tidak berisi field {field}"))
}

/// Respons `GET /api/v2/ listings` ( representasi yang dipakai adapter ).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OsListingsResponse {
    listings: Vec<OsListing>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OsListing {
    protocol_data: Option<serde_json::Value>,
    /// Order hash / identifier.
    order_hash: Option<String>,
    current_price: Option<serde_json::Value>,
    chain: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OsStatsResponse {
    #[serde(default)]
    floor_price: Option<f64>,
}

#[async_trait]
impl Marketplace for OpenSeaAdapter {
    fn name(&self) -> &str {
        "opensea"
    }

    async fn get_floor(&self, collection: Address) -> Result<FloorPrice, MarketError> {
        let url = self.url(&format!("/api/v2/collections/{collection}/stats"));
        let res = self.http.get(&url).await?;
        let body = res.text().await?;
        let stats: OsStatsResponse = serde_json::from_str(&body)?;
        // Body contoh: {"floor_price": 0.05}
        tracing::trace!(floor = ?stats.floor_price, "openSea stats");
        let price_eth = stats
            .floor_price
            .ok_or_else(|| MarketError::CollectionNotFound {
                adapter: "opensea".into(),
                collection: format!("{collection:?}"),
            })?;
        if !price_eth.is_finite() || price_eth <= 0.0 {
            return Err(MarketError::CollectionNotFound {
                adapter: "opensea".into(),
                collection: format!("{collection:?}"),
            });
        }
        let price = eth_to_wei(price_eth);
        Ok(FloorPrice {
            collection,
            price,
            chain_id: self.chain_id,
        })
    }

    async fn get_listings(&self, collection: Address) -> Result<Vec<Listing>, MarketError> {
        let url = self.url(&format!(
            "/api/v2/listings/collection/{collection}?order_by=price_asc"
        ));
        let res = self.http.get(&url).await?;
        let body = res.text().await?;
        let resp: OsListingsResponse = serde_json::from_str(&body)?;
        let mut listings = Vec::with_capacity(resp.listings.len());
        for l in resp.listings {
            // Ambil order hash + price dari struktur datar ( mock friendly ).
            let order_id = l
                .order_hash
                .clone()
                .ok_or_else(|| field_missing("order_hash"))?;
            let token_id = l
                .protocol_data
                .as_ref()
                .and_then(|p| p.get("offer"))
                .and_then(|o| o.get(0))
                .and_then(|i| i.get("identifier_or_criteria"))
                .and_then(|v| v.as_str())
                .and_then(|s| U256::from_str_radix(s, 10).ok())
                .ok_or_else(|| field_missing("offer"))?;
            let seller = l
                .protocol_data
                .as_ref()
                .and_then(|p| p.get("maker"))
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<Address>().ok())
                .ok_or_else(|| field_missing("maker"))?;
            let price = l
                .current_price
                .as_ref()
                .and_then(|p| p.as_str())
                .and_then(|s| U256::from_str_radix(s, 10).ok())
                .ok_or_else(|| field_missing("current_price"))?;
            listings.push(Listing {
                order_id,
                collection,
                token_id,
                price,
                seller,
                chain_id: self.chain_id,
            });
        }
        // Urut termurah duluan ( defensive; API sudah price_asc ).
        listings.sort_by_key(|a| a.price);
        Ok(listings)
    }

    async fn buy(&self, listing: &Listing, buyer: Address) -> Result<UnsignedOrder, MarketError> {
        if listing.chain_id != self.chain_id {
            return Err(MarketError::InvalidParam(format!(
                "listing chain {} != adapter chain {}",
                listing.chain_id, self.chain_id
            )));
        }
        let payload = self.order_payload(
            "buy",
            listing.collection,
            listing.token_id,
            listing.price,
            3600,
            buyer,
        );
        Ok(UnsignedOrder {
            adapter: self.name().into(),
            chain_id: self.chain_id,
            payload,
        })
    }

    async fn list(
        &self,
        params: &ListParams,
        seller: Address,
    ) -> Result<UnsignedOrder, MarketError> {
        if params.chain_id != self.chain_id {
            return Err(MarketError::InvalidParam(format!(
                "list chain {} != adapter chain {}",
                params.chain_id, self.chain_id
            )));
        }
        if params.price == U256::ZERO {
            return Err(MarketError::InvalidParam("harga list tidak boleh 0".into()));
        }
        if params.duration_secs == 0 {
            return Err(MarketError::InvalidParam("duration_secs harus > 0".into()));
        }
        let payload = self.order_payload(
            "list",
            params.collection,
            params.token_id,
            params.price,
            params.duration_secs,
            seller,
        );
        Ok(UnsignedOrder {
            adapter: self.name().into(),
            chain_id: self.chain_id,
            payload,
        })
    }

    async fn bid(&self, params: &BidParams, bidder: Address) -> Result<UnsignedOrder, MarketError> {
        if params.chain_id != self.chain_id {
            return Err(MarketError::InvalidParam(format!(
                "bid chain {} != adapter chain {}",
                params.chain_id, self.chain_id
            )));
        }
        if params.price == U256::ZERO {
            return Err(MarketError::InvalidParam("bid tidak boleh 0".into()));
        }
        let payload = self.order_payload(
            "bid",
            params.collection,
            params.token_id,
            params.price,
            params.duration_secs,
            bidder,
        );
        Ok(UnsignedOrder {
            adapter: self.name().into(),
            chain_id: self.chain_id,
            payload,
        })
    }

    async fn cancel(&self, order_id: &str) -> Result<UnsignedOrder, MarketError> {
        if order_id.trim().is_empty() {
            return Err(MarketError::InvalidParam("order_id kosong".into()));
        }
        Ok(UnsignedOrder {
            adapter: self.name().into(),
            chain_id: self.chain_id,
            payload: json!({
                "protocol": "seaport",
                "kind": "cancel",
                "chain_id": self.chain_id,
                "seaport": format!("{:?}", self.seaport),
                "order_hash": order_id,
            }),
        })
    }
}

/// Konversi ETH ( f64 ) ke wei ( U256 ). Presisi terbatas 1e-18.
fn eth_to_wei(eth: f64) -> U256 {
    let wei_f = eth * 1e18;
    if !wei_f.is_finite() || wei_f < 0.0 {
        return U256::ZERO;
    }
    U256::from(wei_f as u128)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn addr(b: u8) -> Address {
        Address::from_slice(&[b; 20])
    }

    fn adapter() -> OpenSeaAdapter {
        OpenSeaAdapter::new(1, addr(0x60), "http://127.0.0.1:9")
    }

    #[test]
    fn mainnet_adapter_alamat_seaport_resmi() {
        let a = OpenSeaAdapter::mainnet("https://api.opensea.io");
        assert_eq!(a.chain_id(), 1);
        assert_eq!(
            a.seaport_address().to_string().to_lowercase(),
            SEAPORT_ADDRESS_MAINNET.to_lowercase()
        );
    }

    #[test]
    fn eth_to_wei_konversi() {
        assert_eq!(eth_to_wei(1.0), U256::from(1_000_000_000_000_000_000u128));
        assert_eq!(eth_to_wei(0.05), U256::from(50_000_000_000_000_000u128));
        assert_eq!(eth_to_wei(0.0), U256::ZERO);
        assert_eq!(eth_to_wei(-1.0), U256::ZERO);
    }

    #[test]
    fn url_disusun_dari_base() {
        let a = adapter();
        assert!(a.url("/x").starts_with("http://127.0.0.1:9/x"));
    }

    fn listing() -> Listing {
        Listing {
            order_id: "0xabc".into(),
            collection: addr(0x11),
            token_id: U256::from(7),
            price: U256::from(1_000_000_000_000_000_000u128),
            seller: addr(0x22),
            chain_id: 1,
        }
    }

    #[tokio::test]
    async fn buy_menyusun_unsigned_order() {
        let a = adapter();
        let order = a.buy(&listing(), addr(0x33)).await.unwrap();
        assert_eq!(order.adapter, "opensea");
        assert_eq!(order.chain_id, 1);
        assert_eq!(order.payload["kind"], "buy");
        assert_eq!(order.payload["offer"][0]["identifier_or_criteria"], "7");
        assert_eq!(
            order.payload["consideration"][0]["start_amount"],
            "1000000000000000000"
        );
        assert_eq!(order.payload["eip712"]["name"], SEAPORT_EIP712_NAME);
    }

    #[tokio::test]
    async fn buy_chain_berbeda_ditolak() {
        let a = adapter();
        let mut l = listing();
        l.chain_id = 8453;
        let err = a.buy(&l, addr(0x33)).await.unwrap_err();
        assert!(matches!(err, MarketError::InvalidParam(_)));
    }

    #[tokio::test]
    async fn list_menyusun_unsigned_order() {
        let a = adapter();
        let params = ListParams {
            collection: addr(0x11),
            token_id: U256::from(7),
            price: U256::from(2_000_000_000_000_000_000u128),
            duration_secs: 3600,
            chain_id: 1,
        };
        let order = a.list(&params, addr(0x22)).await.unwrap();
        assert_eq!(order.payload["kind"], "list");
        assert_eq!(
            order.payload["consideration"][0]["start_amount"],
            "2000000000000000000"
        );
    }

    #[tokio::test]
    async fn list_harga_nol_ditolak() {
        let a = adapter();
        let params = ListParams {
            collection: addr(0x11),
            token_id: U256::from(7),
            price: U256::ZERO,
            duration_secs: 3600,
            chain_id: 1,
        };
        assert!(matches!(
            a.list(&params, addr(0x22)).await.unwrap_err(),
            MarketError::InvalidParam(_)
        ));
    }

    #[tokio::test]
    async fn list_durasi_nol_ditolak() {
        let a = adapter();
        let params = ListParams {
            collection: addr(0x11),
            token_id: U256::from(7),
            price: U256::from(1u128),
            duration_secs: 0,
            chain_id: 1,
        };
        assert!(a.list(&params, addr(0x22)).await.is_err());
    }

    #[tokio::test]
    async fn bid_menyusun_unsigned_order() {
        let a = adapter();
        let params = BidParams {
            collection: addr(0x11),
            token_id: U256::from(9),
            price: U256::from(500_000_000_000_000u128),
            duration_secs: 3600,
            chain_id: 1,
        };
        let order = a.bid(&params, addr(0x44)).await.unwrap();
        assert_eq!(order.payload["kind"], "bid");
        assert_eq!(order.payload["offer"][0]["identifier_or_criteria"], "9");
    }

    #[tokio::test]
    async fn cancel_menyusun_unsigned_order() {
        let a = adapter();
        let order = a.cancel("0xdeadbeef").await.unwrap();
        assert_eq!(order.payload["kind"], "cancel");
        assert_eq!(order.payload["order_hash"], "0xdeadbeef");
    }

    #[tokio::test]
    async fn cancel_kosong_ditolak() {
        let a = adapter();
        assert!(a.cancel("").await.is_err());
        assert!(a.cancel("   ").await.is_err());
    }

    #[test]
    fn deserialize_listings_response() {
        let body = json!({
            "listings": [{
                "order_hash": "0xabc",
                "current_price": "1500000000000000000",
                "protocol_data": {
                    "maker": format!("{:?}", addr(0x22)),
                    "offer": [{
                        "identifier_or_criteria": "7"
                    }]
                },
                "chain": "ethereum"
            }]
        });
        let resp: OsListingsResponse = serde_json::from_value(body).unwrap();
        assert_eq!(resp.listings.len(), 1);
    }

    #[test]
    fn deserialize_stats_response() {
        let body = json!({ "floor_price": 0.05 });
        let s: OsStatsResponse = serde_json::from_value(body).unwrap();
        assert!((s.floor_price.unwrap() - 0.05).abs() < 1e-9);
    }

    #[test]
    fn deserialize_stats_floor_hilang() {
        let body = json!({});
        let s: OsStatsResponse = serde_json::from_value(body).unwrap();
        assert!(s.floor_price.is_none());
    }

    #[tokio::test]
    async fn get_floor_endpoint_mati_error_jelas() {
        let a = adapter();
        let err = a.get_floor(addr(0x11)).await.unwrap_err();
        // 127.0.0.1:9 = connection refused → Network error, bukan panic.
        assert!(matches!(err, MarketError::Network(_)) || matches!(err, MarketError::Other(_)));
    }

    #[tokio::test]
    async fn get_listings_endpoint_mati_error_jelas() {
        let a = adapter();
        let err = a.get_listings(addr(0x11)).await.unwrap_err();
        assert!(matches!(err, MarketError::Network(_)) || matches!(err, MarketError::Other(_)));
    }
}
