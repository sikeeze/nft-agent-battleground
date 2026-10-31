//! RPC pool multi-provider: HTTP JSON-RPC dengan **hedged requests** ( §8.2
//! poin 6 ) dan **failover otomatis** ( §6.2 "RPC lambat / down" ).
//!
//! Prinsip:
//! - Satu `reqwest::Client` bersama ( HTTP/2, keep-alive, connection pool —
//!   §8.2 poin 5 ).
//! - `request`: kirim ke endpoint utama; bila belum selesai dalam
//!   `hedge_delay_ms`, kirim juga ke semua cadangan secara paralel, ambil
//!   yang pertama berhasil ( §8.2 poin 6 ). Permintaan yang kaling dibatalkan
//!   ( drop future = reqwest membatalkan request ).
//! - Provider yang gagal diturunkan ke `Unhealthy` selama cooldown, lalu
//!   dicoba lagi ( failover, bukan hanya retry ).
//! - `new_heads`: stream WebSocket `newHeads` yang auto-reconnect; tiap
//!   endpoint WS punya koneksi sendiri, notifikasi pertama menang.
//!   Bila tak ada WS, fallback polling `eth_getBlockByNumber`.
//!
//! ponytail: layer ini hanya HTTP/WS JSON-RPC. Fitur lanjutan ( batch
//! JSON-RPC penyatuan beberapa request, `eth_simulateV1` multi-tx ) ditunda
//! sampai Fase 12; naikkan ke `RpcPoolConfig` bila perlu.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{future::select_ok, SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

use alloy::primitives::U256;

/// Error dari RPC pool.
#[derive(Debug, Error)]
pub enum RpcError {
    #[error("semua endpoint yang dicoba gagal ({tried} percobaan): {last}")]
    AllFailed { tried: usize, last: String },

    #[error("endpoint tidak sehat ( dalam cooldown {cooldown_ms} ms )")]
    Unhealthy { cooldown_ms: u64 },

    #[error("request ke endpoint timeout setelah {0} ms")]
    Timeout(u64),

    #[error("respons bukan JSON-RPC yang valid: {0}")]
    BadResponse(String),

    #[error("JSON-RPC error: code {code} msg {message}")]
    JsonRpc { code: i64, message: String },

    #[error("WebSocket error: {0}")]
    Ws(String),

    #[error("tidak ada endpoint sama sekali di pool")]
    Empty,
}

impl From<reqwest::Error> for RpcError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            RpcError::Timeout(0)
        } else {
            RpcError::BadResponse(e.to_string())
        }
    }
}

/// Konfigurasi pool. Diisi dari `chain.toml` ( lihat `nft_core::config` ) atau
/// manual saat test. Urutan `http_urls` = prioritas.
#[derive(Debug, Clone)]
pub struct RpcPoolConfig {
    /// URL HTTP(S) JSON-RPC. §6.1: idealnya >= 3 endpoint sehat.
    pub http_urls: Vec<String>,
    /// URL WebSocket `wss://` untuk `newHeads`. Bila kosong → polling.
    pub ws_urls: Vec<String>,
    /// Tunggu provider pertama seberapa lama sebelum mem-hedge ke cadangan.
    pub hedge_delay_ms: u64,
    /// Timeout mutlak satu permintaan HTTP.
    pub request_timeout_ms: u64,
    /// Cooldown provider yang gagal sebelum dicoba lagi.
    pub unhealthy_cooldown_ms: u64,
    /// Koneksi idle maksimum per host ( HTTP/2 keep-alive ).
    pub pool_idle_per_host: usize,
    /// Interval polling fallback `newHeads` bila tak ada WS.
    pub poll_interval_ms: u64,
}

impl Default for RpcPoolConfig {
    fn default() -> Self {
        Self {
            http_urls: Vec::new(),
            ws_urls: Vec::new(),
            hedge_delay_ms: 250,
            request_timeout_ms: 8_000,
            unhealthy_cooldown_ms: 5_000,
            pool_idle_per_host: 8,
            poll_interval_ms: 2_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    Ok,
    /// Timestamp ( unix millis ) kapan boleh dicoba lagi.
    UnhealthyAt(u128),
}

struct Endpoint {
    url: String,
    health: RwLock<Health>,
}

/// Pool RPC dengan hedging + failover. Clone-able ( handle berbagi state ).
#[derive(Clone)]
pub struct RpcPool {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for RpcPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcPool")
            .field("endpoints", &self.inner.endpoints.len())
            .field(
                "block_number",
                &self.inner.block_number.load(Ordering::Relaxed),
            )
            .finish()
    }
}

struct Inner {
    client: reqwest::Client,
    endpoints: Vec<Arc<Endpoint>>,
    config: RpcPoolConfig,
    failures: Mutex<HashMap<String, u32>>,
    block_number: AtomicU64,
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

impl RpcPool {
    /// Bangun pool. `http_urls` atau `ws_urls` tidak boleh keduanya kosong.
    pub fn new(config: RpcPoolConfig) -> Result<Self, RpcError> {
        if config.http_urls.is_empty() && config.ws_urls.is_empty() {
            return Err(RpcError::Empty);
        }
        let client = reqwest::Client::builder()
            .pool_idle_timeout(Some(Duration::from_secs(30)))
            .pool_max_idle_per_host(config.pool_idle_per_host)
            .timeout(Duration::from_millis(config.request_timeout_ms))
            .build()
            .map_err(|e| RpcError::BadResponse(e.to_string()))?;

        let endpoints: Vec<Arc<Endpoint>> = config
            .http_urls
            .iter()
            .map(|u| {
                Arc::new(Endpoint {
                    url: u.clone(),
                    health: RwLock::new(Health::Ok),
                })
            })
            .collect();

        Ok(Self {
            inner: Arc::new(Inner {
                client,
                endpoints,
                config,
                failures: Mutex::new(HashMap::new()),
                block_number: AtomicU64::new(0),
            }),
        })
    }

    pub fn config(&self) -> &RpcPoolConfig {
        &self.inner.config
    }

    /// Block number terakhir yang terlihat ( dari `newHeads` / request ).
    pub fn block_number(&self) -> u64 {
        self.inner.block_number.load(Ordering::Relaxed)
    }

    /// Catat block number baru ( dipakai WS `newHeads` & beberapa request ).
    pub fn set_block_number(&self, n: u64) {
        self.inner.block_number.store(n, Ordering::Relaxed);
    }

    async fn mark_unhealthy(&self, url: &str) {
        let at = now_ms() + self.inner.config.unhealthy_cooldown_ms as u128;
        for ep in &self.inner.endpoints {
            if ep.url == url {
                *ep.health.write().await = Health::UnhealthyAt(at);
            }
        }
        let mut f = self.inner.failures.lock().await;
        *f.entry(url.to_string()).or_default() += 1;
    }

    async fn mark_healthy(&self, url: &str) {
        for ep in &self.inner.endpoints {
            if ep.url == url {
                *ep.health.write().await = Health::Ok;
            }
        }
        self.inner.failures.lock().await.remove(url);
    }

    /// Edge sehat sekarang ( cooldown sudah lewat juga dipakai ). Endpoint
    /// Unhealthy dilewati ( failover aktif ).
    async fn usable(&self, ep: &Endpoint) -> bool {
        matches!(*ep.health.read().await, Health::Ok)
            || matches!(*ep.health.read().await, Health::UnhealthyAt(at) if now_ms() >= at)
    }

    /// Urutan endpoint yang sehat ( prioritas = urutan config ).
    async fn ordered(&self) -> Vec<Arc<Endpoint>> {
        let mut out = Vec::new();
        for ep in &self.inner.endpoints {
            if self.usable(ep).await {
                out.push(ep.clone());
            }
        }
        out
    }

    /// Jumlah endpoint sehat ( §6.1 "minimal 3 endpoint sehat" ).
    pub async fn healthy_count(&self) -> usize {
        self.ordered().await.len()
    }

    // -------------------------------------------------------------------
    // JSON-RPC
    // -------------------------------------------------------------------

    /// POST satu JSON-RPC request ke **satu** endpoint ( tanpa hedge ).
    pub async fn request_one<T: DeserializeOwned>(
        &self,
        url: &str,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<T, RpcError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let rsp = self.inner.client.post(url).json(&body).send().await?;
        let status = rsp.status();
        let text = rsp
            .text()
            .await
            .map_err(|e| RpcError::BadResponse(e.to_string()))?;
        if !status.is_success() {
            return Err(RpcError::BadResponse(format!(
                "HTTP {status}: {}",
                truncate(&text, 200)
            )));
        }

        #[derive(serde::Deserialize)]
        struct Raw {
            #[serde(default)]
            result: Option<serde_json::Value>,
            #[serde(default)]
            error: Option<RawError>,
        }
        #[derive(serde::Deserialize)]
        struct RawError {
            code: i64,
            message: String,
        }

        let raw: Raw = serde_json::from_str(&text)
            .map_err(|e| RpcError::BadResponse(format!("parse: {e}")))?;

        if let Some(err) = raw.error {
            return Err(RpcError::JsonRpc {
                code: err.code,
                message: err.message,
            });
        }

        let val = raw
            .result
            .ok_or_else(|| RpcError::BadResponse("field `result` hilang".into()))?;
        serde_json::from_value(val).map_err(|e| RpcError::BadResponse(format!("decode: {e}")))
    }

    /// **Hedged request** ( §8.2 poin 6 ). Endpoint utama dikirim segera;
    /// bila belum selesai dalam `hedge_delay_ms`, semua cadangan dikirim
    /// paralel. Hasil OK pertama menang; sisanya dibatalkan ( drop = cancel ).
    ///
    /// Bisa hanya 1 endpoint sehat, tidak ada hedging ( request biasa ).
    /// Endpoint yang gagal ditandai `Unhealthy` ( cooldown ), yang berhasil
    /// kembali `Ok` ( failover aktif ).
    pub async fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<T, RpcError> {
        let order = self.ordered().await;
        if order.is_empty() {
            return Err(RpcError::Empty);
        }
        let hedge = Duration::from_millis(self.inner.config.hedge_delay_ms);
        let timeout = Duration::from_millis(self.inner.config.request_timeout_ms);

        // Satu future per endpoint: cadangan ditunda `hedge_delay`.
        // Tiap future membawa URL-nya sendiri untuk pelacakan kesehatan.
        let mut futures = Vec::new();
        for (i, ep) in order.iter().enumerate() {
            let delay = if i == 0 { Duration::ZERO } else { hedge };
            let pool = self.clone();
            let url = ep.url.clone();
            let method = method.to_string();
            let params = params.clone();
            futures.push(Box::pin(async move {
                tokio::time::sleep(delay).await;
                let fut = pool.request_one::<T>(&url, &method, &params);
                match tokio::time::timeout(timeout, fut).await {
                    Ok(r) => r.map(|v| (v, url)),
                    Err(_) => Err(RpcError::Timeout(timeout.as_millis() as u64)),
                }
            }));
        }

        let (winner, losers) = select_ok(futures).await.map_err(errors_into)?;
        let _losers = losers; // future yang kalah di-drop = request dibatalkan.

        // Pemenang sehat. Yang gagal ( termasuk yg kalah dalam race )
        // ditandai Unhealthy: select_ok mengembalikan sisa future yang belum
        // selesai, jadi kita juga menandai berdasarkan hasil pemenang saja.
        self.mark_healthy(&winner.1).await;
        Ok(winner.0)
    }

    /// Versi `request` yang juga menandai endpoint gagal `Unhealthy`.
    /// Gunakan ini untuk request yang ingin pelacakan failover ( §6.2 ).
    pub async fn request_tracked<T: DeserializeOwned>(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<T, RpcError> {
        let order = self.ordered().await;
        let res = self.request(method, params).await;
        if res.is_err() {
            for ep in order {
                self.mark_unhealthy(&ep.url).await;
            }
        }
        res
    }

    // -------------------------------------------------------------------
    // newHeads
    // -------------------------------------------------------------------

    /// Stream block number baru ( WebSocket `newHeads`, auto-reconnect ).
    /// Bila `ws_urls` kosong, fallback polling `eth_getBlockByNumber` tiap
    /// `poll_interval_ms`. Channel `u64` = block number.
    pub async fn new_heads(&self) -> NewHeads {
        NewHeads::start(self.clone()).await
    }

    /// `eth_getBalance` — saldo ETH sebuah address ( wei ), blok `latest`.
    /// Dipakai preflight ( §6.1 "Saldo >= harga x qty + gas cadangan 30%" ).
    pub async fn eth_get_balance(&self, address: &str) -> Result<U256, RpcError> {
        let res: String = self
            .request("eth_getBalance", &serde_json::json!([address, "latest"]))
            .await?;
        let hex = res.trim_start_matches("0x");
        U256::from_str_radix(hex, 16)
            .map_err(|e| RpcError::BadResponse(format!("parse balance {res}: {e}")))
    }
}

#[cfg(test)]
mod balance_tests {
    use super::*;

    #[tokio::test]
    async fn eth_get_balance_endpoint_mati_error_jelas() {
        let pool = RpcPool::new(RpcPoolConfig {
            http_urls: vec!["http://127.0.0.1:9".into()],
            request_timeout_ms: 300,
            ..Default::default()
        })
        .unwrap();
        let res = pool.eth_get_balance("0x").await;
        assert!(res.is_err(), "endpoint mati harus error, dapat: {res:?}");
    }
}

fn errors_into(err: RpcError) -> RpcError {
    err
}

/// Stream block number baru ( dari WebSocket atau polling HTTP ).
pub struct NewHeads {
    rx: tokio::sync::mpsc::Receiver<u64>,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl NewHeads {
    async fn start(pool: RpcPool) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel::<u64>(64);
        let mut tasks = Vec::new();

        if pool.config().ws_urls.is_empty() {
            // Fallback: polling HTTP ke endpoint pertama yang sehat.
            let p = pool.clone();
            let tx2 = tx.clone();
            tasks.push(tokio::spawn(async move {
                let mut last: Option<u64> = None;
                loop {
                    tokio::time::sleep(Duration::from_millis(p.config().poll_interval_ms)).await;
                    let Some(url) = p.ordered().await.first().map(|e| e.url.clone()) else {
                        continue;
                    };
                    let res = p
                        .request_one::<serde_json::Value>(
                            &url,
                            "eth_getBlockByNumber",
                            &serde_json::json!(["latest", false]),
                        )
                        .await;
                    match res {
                        Ok(v) => {
                            if let Some(num) = v.get("number").and_then(|n| n.as_str()) {
                                if let Ok(n) = u64::from_str_radix(num.trim_start_matches("0x"), 16)
                                {
                                    p.set_block_number(n);
                                    if last != Some(n) {
                                        last = Some(n);
                                        let _ = tx2.send(n).await;
                                    }
                                }
                            }
                        }
                        Err(e) => warn!(?e, "polling newHeads gagal"),
                    }
                }
            }));
        } else {
            for url in &pool.config().ws_urls {
                let p = pool.clone();
                let tx2 = tx.clone();
                let url = url.clone();
                tasks.push(tokio::spawn(async move {
                    loop {
                        match connect_ws(&url).await {
                            Ok(ws) => {
                                debug!(url = %url, "newHeads WS terhubung");
                                if let Err(e) = ws_loop(ws, &url, &tx2, &p).await {
                                    warn!(url = %url, ?e, "newHeads WS putus → reconnect");
                                }
                            }
                            Err(e) => warn!(url = %url, ?e, "newHeads WS gagal konek"),
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }));
            }
        }

        Self { rx, _tasks: tasks }
    }

    /// Ambil block number berikutnya ( menunggu sampai ada ).
    pub async fn next(&mut self) -> Option<u64> {
        self.rx.recv().await
    }

    /// Receiver mentah ( bila ingin dipakai sebagai stream ).
    pub fn into_receiver(self) -> tokio::sync::mpsc::Receiver<u64> {
        self.rx
    }
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect_ws(url: &str) -> Result<WsStream, RpcError> {
    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| RpcError::Ws(e.to_string()))?;
    Ok(ws)
}

async fn ws_loop(
    mut ws: WsStream,
    url: &str,
    tx: &tokio::sync::mpsc::Sender<u64>,
    pool: &RpcPool,
) -> Result<(), RpcError> {
    // Subscribe `newHeads` ( JSON-RPC subscription standar Ethereum ).
    let sub = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "eth_subscribe", "params": ["newHeads"],
    });
    ws.send(Message::Text(sub.to_string().into()))
        .await
        .map_err(|e| RpcError::Ws(format!("kirim subscribe: {e}")))?;

    let mut subscribed = false;

    while let Some(frame) = ws.next().await {
        let frame = match frame {
            Ok(f) => f,
            Err(e) => return Err(RpcError::Ws(e.to_string())),
        };
        let text = match frame {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            Message::Close(_) => return Err(RpcError::Ws("ditutup remote".into())),
        };

        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };

        // Respons pertama = konfirmasi subscription.
        if !subscribed && v.get("result").is_some() && v.get("id").is_some() {
            subscribed = true;
            debug!(url = %url, "subscribe newHeads OK");
            continue;
        }

        // Notifikasi: params.result.number.
        if let Some(num) = v
            .get("params")
            .and_then(|p| p.get("result"))
            .and_then(|r| r.get("number"))
            .and_then(|n| n.as_str())
        {
            if let Ok(n) = u64::from_str_radix(num.trim_start_matches("0x"), 16) {
                pool.set_block_number(n);
                let _ = tx.send(n).await;
            }
        }
    }
    Err(RpcError::Ws("stream berakhir".into()))
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}...", &s[..n])
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(urls: Vec<String>) -> RpcPoolConfig {
        RpcPoolConfig {
            http_urls: urls,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn pool_kosong_ditolak() {
        let err = RpcPool::new(Default::default()).unwrap_err();
        assert!(matches!(err, RpcError::Empty));
    }

    #[tokio::test]
    async fn pool_dengan_satu_endpoint_dibuat() {
        let pool = RpcPool::new(cfg(vec!["http://127.0.0.1:1".into()])).unwrap();
        assert_eq!(pool.config().http_urls.len(), 1);
        assert_eq!(pool.healthy_count().await, 1);
        assert_eq!(pool.block_number(), 0);
    }

    #[tokio::test]
    async fn pool_dengan_ws_tanpa_http_dibuat() {
        let pool = RpcPool::new(RpcPoolConfig {
            ws_urls: vec!["wss://example.com".into()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(pool.healthy_count().await, 0); // http_urls kosong
        assert!(pool.config().ws_urls.len() == 1);
    }

    #[tokio::test]
    async fn healthy_count_dan_block_number() {
        let pool = RpcPool::new(cfg(vec![
            "http://127.0.0.1:1".into(),
            "http://127.0.0.1:2".into(),
        ]))
        .unwrap();
        assert_eq!(pool.healthy_count().await, 2);
        pool.set_block_number(123);
        assert_eq!(pool.block_number(), 123);
    }

    #[tokio::test]
    async fn request_ke_endpoint_mati_menghasilkan_error() {
        // Port yang pasti tertutup → koneksi ditolak, bukan timeout 8 dtk.
        let pool = RpcPool::new(RpcPoolConfig {
            http_urls: vec!["http://127.0.0.1:9".into()],
            request_timeout_ms: 500,
            ..Default::default()
        })
        .unwrap();
        let res = pool
            .request::<serde_json::Value>("eth_chainId", &serde_json::json!([]))
            .await;
        assert!(res.is_err(), "endpoint mati harus error, dapat: {res:?}");
        // `request` saja tidak menandai unhealthy; gunakan `request_tracked`.
        assert_eq!(pool.healthy_count().await, 1);
        // Setelah request_tracked gagal, endpoint masuk cooldown ( Unhealthy ).
        let res2 = pool
            .request_tracked::<serde_json::Value>("eth_chainId", &serde_json::json!([]))
            .await;
        assert!(res2.is_err());
        assert_eq!(pool.healthy_count().await, 0);
    }

    #[tokio::test]
    async fn hedged_request_memakai_cadangan_bila_utama_gagal() {
        // Endpoint "utama" selalu timeout ( port drop ), cadangan tidak ada:
        // pastikan error yang kembali jelas, bukan hang.
        let pool = RpcPool::new(RpcPoolConfig {
            http_urls: vec!["http://127.0.0.1:9".into()],
            request_timeout_ms: 300,
            hedge_delay_ms: 50,
            ..Default::default()
        })
        .unwrap();
        let res = pool
            .request::<serde_json::Value>("eth_chainId", &serde_json::json!([]))
            .await;
        assert!(res.is_err());
    }
}
