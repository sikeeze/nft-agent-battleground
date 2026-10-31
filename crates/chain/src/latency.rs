//! `measure_latency` ( blueprint §8.3, §7 "Latensi diukur, bukan ditebak" ).
//!
//! Saat preflight agent mengirim ping ke tiap RPC/relay dan menghitung p50/p95,
//! lalu menyimpannya untuk menghitung `Tsend` ( §7 model waktu ). Statistik
//! tersimpan ke SQLite lewat `persist` ( §9 Fase 3: "statistik latensi
//! tersimpan" ).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;

#[derive(Debug, Error)]
pub enum LatencyError {
    #[error("RPC error saat mengukur latensi: {0}")]
    Rpc(String),

    #[error("persistensi statistik gagal: {0}")]
    Persist(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencySample {
    pub endpoint: String,
    pub rtt_ms: u64,
    pub at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointStats {
    pub endpoint: String,
    pub samples: u64,
    pub min_ms: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub max_ms: u64,
    pub last_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LatencyStats {
    pub per_endpoint: Vec<EndpointStats>,
}

impl LatencyStats {
    /// p95 terbaik ( terendah ) di antara endpoint.
    pub fn best_p95_ms(&self) -> Option<u64> {
        self.per_endpoint.iter().map(|e| e.p95_ms).min()
    }

    /// p50 terbaik ( terendah ).
    pub fn best_p50_ms(&self) -> Option<u64> {
        self.per_endpoint.iter().map(|e| e.p50_ms).min()
    }
}

/// Konfigurasi endpoint RPC/relay untuk pengukuran.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub url: String,
    #[serde(default)]
    pub label: Option<String>,
}

mod tracker_inner {
    use std::collections::HashMap;

    #[derive(Debug)]
    pub struct Inner {
        pub window: usize,
        pub samples: HashMap<String, Vec<(u64, i64)>>,
    }

    impl Inner {
        pub fn new(window: usize) -> Self {
            Self {
                window,
                samples: HashMap::new(),
            }
        }

        pub fn record(&mut self, endpoint: &str, rtt_ms: u64, at: i64) {
            let v = self.samples.entry(endpoint.to_string()).or_default();
            v.push((rtt_ms, at));
            if v.len() > self.window {
                v.remove(0);
            }
        }

        pub fn stats(&self, endpoint: &str) -> Option<super::EndpointStats> {
            let v = self.samples.get(endpoint)?;
            if v.is_empty() {
                return None;
            }
            let mut sorted: Vec<u64> = v.iter().map(|(r, _)| *r).collect();
            sorted.sort_unstable();
            let pct = |p: f64| -> u64 {
                let idx = ((sorted.len() - 1) as f64 * p / 100.0).round() as usize;
                sorted[idx.min(sorted.len() - 1)]
            };
            Some(super::EndpointStats {
                endpoint: endpoint.to_string(),
                samples: v.len() as u64,
                min_ms: *sorted.first().unwrap(),
                p50_ms: pct(50.0),
                p95_ms: pct(95.0),
                max_ms: *sorted.last().unwrap(),
                last_at: v.last().map(|(_, at)| *at).unwrap_or(0),
            })
        }

        pub fn all_stats(&self) -> Vec<super::EndpointStats> {
            let mut keys: Vec<&String> = self.samples.keys().collect();
            keys.sort();
            keys.into_iter().filter_map(|k| self.stats(k)).collect()
        }
    }
}

/// Pengukur latensi. Menyimpan sampel dalam window ( default 100 sampel
/// per endpoint ) dan menghitung persentil sesuai permintaan.
#[derive(Debug, Clone)]
pub struct LatencyTracker {
    inner: Arc<RwLock<tracker_inner::Inner>>,
}

impl LatencyTracker {
    pub fn new() -> Self {
        Self::with_window(100)
    }

    pub fn with_window(window: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(tracker_inner::Inner::new(window.max(1)))),
        }
    }

    /// Ukur latensi endpoint dengan request `eth_chainId` ( ringan, tidak
    /// membebani node ). Menyimpan sampel ke window.
    pub async fn measure(&self, endpoint: &str) -> Result<LatencySample, LatencyError> {
        let start = std::time::Instant::now();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| LatencyError::Rpc(e.to_string()))?;
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "eth_chainId", "params": serde_json::Value::Array(vec![]),
        });
        let _ = client
            .post(endpoint)
            .json(&body)
            .send()
            .await
            .map_err(|e| LatencyError::Rpc(e.to_string()))?;
        let rtt_ms = start.elapsed().as_millis() as u64;
        let at = chrono::Utc::now().timestamp();
        self.inner.write().await.record(endpoint, rtt_ms, at);
        Ok(LatencySample {
            endpoint: endpoint.to_string(),
            rtt_ms,
            at,
        })
    }

    /// Statistik semua endpoint yang sudah diukur.
    pub async fn stats(&self) -> LatencyStats {
        LatencyStats {
            per_endpoint: self.inner.read().await.all_stats(),
        }
    }
}

impl Default for LatencyTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// `measure_latency` ( §8.3 ): ukur RTT semua endpoint, kembalikan statistik.
pub async fn measure_latency(
    tracker: &LatencyTracker,
    endpoints: &[Endpoint],
) -> Result<LatencyStats, LatencyError> {
    for ep in endpoints {
        match tracker.measure(&ep.url).await {
            Ok(s) => tracing::debug!(?s, "latensi sampel"),
            Err(e) => tracing::warn!(url = %ep.url, ?e, "gagal mengukur latensi"),
        }
    }
    Ok(tracker.stats().await)
}

/// Simpan statistik latensi ke tabel `latency_stats`.
pub async fn persist(pool: &sqlx::SqlitePool, stats: &LatencyStats) -> Result<(), LatencyError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| LatencyError::Persist(e.to_string()))?;
    let now = chrono::Utc::now().timestamp();
    for e in &stats.per_endpoint {
        sqlx::query(
            "INSERT INTO latency_stats (endpoint, samples, min_ms, p50_ms, p95_ms, max_ms, at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&e.endpoint)
        .bind(e.samples as i64)
        .bind(e.min_ms as i64)
        .bind(e.p50_ms as i64)
        .bind(e.p95_ms as i64)
        .bind(e.max_ms as i64)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(|e| LatencyError::Persist(e.to_string()))?;
    }
    tx.commit()
        .await
        .map_err(|e| LatencyError::Persist(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tracker_menyimpan_sampel_dan_menghitung_persentil() {
        let t = LatencyTracker::with_window(10);
        for i in 0..10 {
            t.inner.write().await.record("http://x", 100 + i, 1);
        }
        let st = t.stats().await;
        assert_eq!(st.per_endpoint.len(), 1);
        let e = &st.per_endpoint[0];
        assert_eq!(e.samples, 10);
        assert_eq!(e.min_ms, 100);
        assert_eq!(e.max_ms, 109);
        // sorted = [100..109]; idx = round(9 * 50/100) = 5 ( rounded ) → 105.
        assert_eq!(e.p50_ms, 105);
        // idx = round(9 * 95/100) = round(8.55) = 9 → 109.
        assert_eq!(e.p95_ms, 109);
    }

    #[tokio::test]
    async fn tracker_window_membatasi_sampel() {
        let t = LatencyTracker::with_window(3);
        for i in 0..10 {
            t.inner.write().await.record("http://x", i, 1);
        }
        let st = t.stats().await;
        assert_eq!(st.per_endpoint[0].samples, 3);
    }

    #[tokio::test]
    async fn best_p95_dipilih_yang_terendah() {
        let t = LatencyTracker::with_window(10);
        t.inner.write().await.record("a", 10, 1);
        t.inner.write().await.record("b", 50, 1);
        let st = t.stats().await;
        assert_eq!(st.best_p95_ms(), Some(10));
        assert_eq!(st.best_p50_ms(), Some(10));
    }

    #[tokio::test]
    async fn measure_endpoint_mati_error() {
        let t = LatencyTracker::new();
        let res = t.measure("http://127.0.0.1:9").await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn persist_menyimpan_statistik_ke_sqlite() {
        let pool = nft_core::storage::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS latency_stats (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, \
             endpoint TEXT NOT NULL, samples INTEGER, min_ms INTEGER, \
             p50_ms INTEGER, p95_ms INTEGER, max_ms INTEGER, at INTEGER NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let t = LatencyTracker::with_window(10);
        t.inner.write().await.record("http://x", 42, 1);
        let stats = t.stats().await;
        persist(&pool, &stats).await.unwrap();
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM latency_stats")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }
}
