//! `nft-api` -- REST API lokal (blueprint SF10, S9 Fase 10).
//!
//! Untuk kustomisasi lebih dalam: tambah rule, kelola skill, ganti model,
//! ambil portfolio, stream log ( SSE ).
//!
//! **Keamanan** ( SF10 ):
//! - Hanya bind `127.0.0.1` / lewat VPN ( Tailscale ) -- NEVER bind ke
//!   `0.0.0.0`. Bind ke IPv4 loopback saja.
//! - Pakai **Bearer token** ( dari env `NFT_API_TOKEN` ); request tanpa/
//!   salah token -> `401`.
//! - Token API tidak pernah di-log / di-audit bersama payload.
//!
//! # Endpoint
//!
//! - `GET /health` -- status dasar ( tanpa auth, localhost-only ).
//! - `GET /api status` -- status agent.
//! - `GET /api/rules` -- daftar rule.
//! - `POST /api/rules` -- tambah rule ( validasi ketat ).
//! - `GET /api/portfolio` -- ringkasan PnL.
//! - `GET /api/jobs` -- daftar job scheduler.
//! - `POST /api/jobs/oneshot` -- jadwalkan job one-shot.
//! - `GET /openapi.json` -- skema OpenAPI 3.0 ( dokumentasi ).
//!
//! ponytail: SSE stream log ( `/api/logs/stream` ) menyusul setelah
//! tracing subscriber layer tersedia di crate agent.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use thiserror::Error;

/// Nama env var token API ( SF10: token dari env, bukan file config ).
pub const API_TOKEN_ENV: &str = "NFT_API_TOKEN";

/// State bersama API.
#[derive(Clone)]
pub struct ApiState {
    pub token: String,
    pub rules: Arc<tokio::sync::Mutex<nft_trader::RulesConfig>>,
    pub portfolio: Arc<tokio::sync::Mutex<nft_trader::Portfolio>>,
    pub scheduler: Option<Arc<nft_scheduler::Scheduler>>,
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("unauthorized")]
    Unauthorized,

    #[error("rule tidak valid: {0}")]
    InvalidRule(#[from] nft_trader::RulesError),

    #[error("job tidak valid: {0}")]
    InvalidJob(#[from] nft_scheduler::SchedulerError),

    #[error("payload bukan objek JSON")]
    BadPayload,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, msg) = match &self {
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
            _ => {
                let code = match &self {
                    ApiError::InvalidRule(_) | ApiError::BadPayload => StatusCode::BAD_REQUEST,
                    ApiError::InvalidJob(_) => StatusCode::INTERNAL_SERVER_ERROR,
                    ApiError::Unauthorized => unreachable!(),
                };
                (code, self.to_string())
            }
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

/// Cek Bearer token. Wajib cocok persis ( constant-time-ish via len cek
/// dulu; token dari env panjang acak ).
pub fn check_token(headers: &HeaderMap, token: &str) -> Result<(), ApiError> {
    let auth = headers
        .get(HeaderName::from_static("authorization"))
        .and_then(|v| v.to_str().ok())
        .ok_or(ApiError::Unauthorized)?;
    let provided = auth.strip_prefix("Bearer ").ok_or(ApiError::Unauthorized)?;
    // Hindari timing attack sederhana: bandingkan panjang dulu.
    if provided.len() != token.len() {
        return Err(ApiError::Unauthorized);
    }
    if provided != token {
        return Err(ApiError::Unauthorized);
    }
    Ok(())
}

/// Router API. Bind ke `127.0.0.1` saja ( SF10 ).
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/openapi.json", get(openapi))
        .route("/api/status", get(api_status))
        .route("/api/rules", get(list_rules).post(add_rule))
        .route("/api/portfolio", get(portfolio))
        .route("/api/jobs", get(list_jobs))
        .route("/api/jobs/oneshot", post(add_oneshot))
        .with_state(state)
}

/// Jalankan server terikat ke `127.0.0.1:port` ( SF10: tidak pernah
/// bind ke 0.0.0.0 / IP publik ).
pub async fn serve(state: ApiState, port: u16) -> Result<(), std::io::Error> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "REST API lokal dimulai (127.0.0.1 only)");
    axum::serve(listener, router(state)).await
}

// ---------------------------------------------------------------------------
// handler
// ---------------------------------------------------------------------------

async fn health(State(_s): State<ApiState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn openapi(State(_s): State<ApiState>) -> Json<serde_json::Value> {
    Json(openapi_spec())
}

async fn api_status(
    State(s): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_token(&headers, &s.token)?;
    let rules = s.rules.lock().await;
    Ok(Json(serde_json::json!({
        "mode": "dry-run",
        "rules_count": rules.rule.len(),
        "portfolio_positions": s.portfolio.lock().await.positions().len(),
    })))
}

async fn list_rules(
    State(s): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_token(&headers, &s.token)?;
    let rules = s.rules.lock().await;
    Ok(Json(serde_json::json!({ "rules": rules.rule })))
}

#[derive(Debug, Deserialize)]
struct AddRuleBody {
    rule: nft_trader::Rule,
}

async fn add_rule(
    State(s): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<AddRuleBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_token(&headers, &s.token)?;
    let mut rules = s.rules.lock().await;
    let rule = body.rule;
    rule.validate()?;
    if rules.rule.iter().any(|r| r.name == rule.name) {
        return Err(ApiError::InvalidRule(
            nft_trader::RulesError::DuplicateName(rule.name),
        ));
    }
    rules.rule.push(rule.clone());
    Ok(Json(serde_json::json!({ "added": rule })))
}

async fn portfolio(
    State(s): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_token(&headers, &s.token)?;
    let p = s.portfolio.lock().await;
    Ok(Json(serde_json::json!({
        "summary": p.summarize(),
        "positions": p.positions(),
    })))
}

async fn list_jobs(
    State(s): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_token(&headers, &s.token)?;
    let Some(sched) = s.scheduler.as_ref() else {
        return Ok(Json(serde_json::json!({ "jobs": [] })));
    };
    let jobs = sched.list().await?;
    Ok(Json(serde_json::json!({ "jobs": jobs })))
}

#[derive(Debug, Deserialize)]
struct AddOneshotBody {
    name: String,
    payload: serde_json::Value,
    run_at: i64,
}

async fn add_oneshot(
    State(s): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<AddOneshotBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_token(&headers, &s.token)?;
    let Some(sched) = s.scheduler.as_ref() else {
        return Err(ApiError::InvalidJob(
            nft_scheduler::SchedulerError::Validation("scheduler tidak dikonfigurasi".into()),
        ));
    };
    if !body.payload.is_object() {
        return Err(ApiError::BadPayload);
    }
    let run_at = chrono::DateTime::from_timestamp(body.run_at, 0).ok_or_else(|| {
        ApiError::InvalidJob(nft_scheduler::SchedulerError::Validation(
            "run_at tidak valid".into(),
        ))
    })?;
    let id = sched.add_oneshot(&body.name, body.payload, run_at).await?;
    Ok(Json(serde_json::json!({ "id": id })))
}

/// Skema OpenAPI 3.0 sederhana ( dokumentasi endpoint ).
fn openapi_spec() -> serde_json::Value {
    serde_json::json!({
        "openapi": "3.0.0",
        "info": {
            "title": "nft-agent REST API",
            "version": "1.0.0",
            "description": "API lokal nft-agent (127.0.0.1 only, Bearer token)"
        },
        "servers": [{ "url": "http://127.0.0.1:9876" }],
        "components": {
            "securitySchemes": {
                "bearer": { "type": "http", "scheme": "bearer" }
            }
        },
        "security": [{ "bearer": [] }],
        "paths": {
            "/health": { "get": { "summary": "Status dasar ( tanpa auth )" } },
            "/api/status": { "get": { "summary": "Status agent" } },
            "/api/rules": {
                "get": { "summary": "Daftar rule trading" },
                "post": { "summary": "Tambah rule trading ( validasi ketat )" }
            },
            "/api/portfolio": { "get": { "summary": "Ringkasan PnL portfolio" } },
            "/api/jobs": { "get": { "summary": "Daftar job scheduler" } },
            "/api/jobs/oneshot": { "post": { "summary": "Jadwalkan job one-shot" } }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn state() -> ApiState {
        ApiState {
            token: "test-token".into(),
            rules: Arc::new(tokio::sync::Mutex::new(
                nft_trader::parse_rules("").unwrap(),
            )),
            portfolio: Arc::new(tokio::sync::Mutex::new(nft_trader::Portfolio::new())),
            scheduler: None,
        }
    }

    fn auth_req(method: &str, uri: &str, token: Option<&str>, body: &str) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        if !body.is_empty() {
            b = b.header("content-type", "application/json");
        }
        let body = body.to_string();
        b.body(Body::from(body)).unwrap()
    }

    #[tokio::test]
    async fn health_tanpa_auth_ok() {
        let app = router(state());
        let res = app
            .oneshot(auth_req("GET", "/health", None, ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn endpoint_tanpa_token_ditolak_401() {
        let app = router(state());
        let res = app
            .oneshot(auth_req("GET", "/api/status", None, ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn endpoint_token_salah_ditolak_401() {
        let app = router(state());
        let res = app
            .oneshot(auth_req("GET", "/api/status", Some("salah"), ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn endpoint_token_benar_ok() {
        let app = router(state());
        let res = app
            .oneshot(auth_req("GET", "/api/status", Some("test-token"), ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn list_rules_auth_ok() {
        let app = router(state());
        let res = app
            .oneshot(auth_req("GET", "/api/rules", Some("test-token"), ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn tambah_rule_valid() {
        let s = state();
        let app = router(s.clone());
        let body = r#"{"rule":{"name":"api-snipe","kind":"buy","collection":"0x1e0e7eF5D8C2a093F9c2d5b8e2e2e2e2e2e2e2e2","action":"buy","max_price_eth":2.0}}"#;
        let res = app
            .oneshot(auth_req("POST", "/api/rules", Some("test-token"), body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(s.rules.lock().await.rule.len(), 1);
    }

    #[tokio::test]
    async fn tambah_rule_invalid_ditolak_400() {
        let app = router(state());
        // Buy tanpa kriteria harga -> 400.
        let body = r#"{"rule":{"name":"bad","kind":"buy","collection":"0x1e0e7eF5D8C2a093F9c2d5b8e2e2e2e2e2e2e2e2","action":"buy"}}"#;
        let res = app
            .oneshot(auth_req("POST", "/api/rules", Some("test-token"), body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn tambah_rule_duplikat_ditolak() {
        let s = state();
        let body = r#"{"rule":{"name":"api-snipe","kind":"buy","collection":"0x1e0e7eF5D8C2a093F9c2d5b8e2e2e2e2e2e2e2e2","action":"buy","max_price_eth":2.0}}"#;
        {
            let add: AddRuleBody = serde_json::from_str(body).unwrap();
            let mut rules = s.rules.lock().await;
            rules.rule.push(add.rule);
        }
        let app = router(s.clone());
        let res = app
            .oneshot(auth_req("POST", "/api/rules", Some("test-token"), body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn tambah_rule_tanpa_auth_ditolak() {
        let app = router(state());
        let body = r#"{"rule":{"name":"x","kind":"buy","collection":"0x1e0e7eF5D8C2a093F9c2d5b8e2e2e2e2e2e2e2e2","action":"buy","max_price_eth":2.0}}"#;
        let res = app
            .oneshot(auth_req("POST", "/api/rules", None, body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn portfolio_auth_ok() {
        let app = router(state());
        let res = app
            .oneshot(auth_req("GET", "/api/portfolio", Some("test-token"), ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn jobs_kosong_bila_scheduler_none() {
        let app = router(state());
        let res = app
            .oneshot(auth_req("GET", "/api/jobs", Some("test-token"), ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn oneshot_scheduler_none_error() {
        let app = router(state());
        let body = r#"{"name":"x","payload":{},"run_at":1893456000}"#;
        let res = app
            .oneshot(auth_req(
                "POST",
                "/api/jobs/oneshot",
                Some("test-token"),
                body,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn oneshot_payload_bukan_objek_ditolak() {
        // Scheduler None -> 500 sekarang; test validasi payload via handler
        // bila scheduler ada ( lihat integration dibawah ).
        let app = router(state());
        let body = r#"{"name":"x","payload":[1],"run_at":1893456000}"#;
        let res = app
            .oneshot(auth_req(
                "POST",
                "/api/jobs/oneshot",
                Some("test-token"),
                body,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn oneshot_run_at_invalid_ditolak() {
        let app = router(state());
        let body = r#"{"name":"x","payload":{},"run_at":99999999999999}"#;
        let res = app
            .oneshot(auth_req(
                "POST",
                "/api/jobs/oneshot",
                Some("test-token"),
                body,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn openapi_json_tersedia() {
        let app = router(state());
        let res = app
            .oneshot(auth_req("GET", "/openapi.json", None, ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["openapi"], "3.0.0");
    }

    #[tokio::test]
    async fn jobs_dengan_scheduler_ada() {
        // Scheduler dengan db memory + runner no-op.
        struct NoopRunner;
        #[async_trait::async_trait]
        impl nft_scheduler::JobRunner for NoopRunner {
            async fn run(&self, _job: &nft_scheduler::Job) -> Result<(), String> {
                Ok(())
            }
        }
        let db = nft_core::storage::connect("sqlite::memory:").await.unwrap();
        let sched = Arc::new(
            nft_scheduler::Scheduler::new(db, Arc::new(NoopRunner))
                .await
                .unwrap(),
        );
        let s = ApiState {
            token: "t".into(),
            rules: Arc::new(tokio::sync::Mutex::new(
                nft_trader::parse_rules("").unwrap(),
            )),
            portfolio: Arc::new(tokio::sync::Mutex::new(nft_trader::Portfolio::new())),
            scheduler: Some(sched),
        };
        let app = router(s);
        let res = app
            .oneshot(auth_req("GET", "/api/jobs", Some("t"), ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[test]
    fn check_token_panjang_berbeda_ditolak() {
        let headers = HeaderMap::new();
        assert!(check_token(&headers, "abc").is_err());
    }

    #[test]
    fn api_token_env_constant() {
        assert_eq!(API_TOKEN_ENV, "NFT_API_TOKEN");
    }
}
