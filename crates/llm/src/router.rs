//! Routing model (blueprint SF7: "model dan routing diatur di models.toml").
//!
//! `LlmClient` memegang satu `reqwest::Client` ( HTTP/2 keep-alive, pool )
//! dan model aktif. `with_model` ganti model tanpa ubah kode ( `/model <id>`
//! di Telegram ). Permintaan dipunya dengan timeout mutlak ( default 3 dtk
//! dari `models.toml`, S8.2 poin 3 ).
//!
//! Semua endpoint OpenAI-compatible: `POST {base_url}/chat/completions`
//! dengan `Authorization: Bearer <key>`. Base URL & API key dibaca dari env
//! var yang dinamai di `models.toml` ( tidak ditulis di file config ).

use std::sync::Arc;
use std::time::Duration;

use nft_core::config::ModelEntry;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use thiserror::Error;
use tracing::{debug, warn};

use crate::tools::ToolSpec;
use crate::ChatMessage;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("model '{model}': base URL kosong ( env {env} tidak diset )")]
    MissingBaseUrl { model: String, env: String },

    #[error("model '{model}': API key kosong ( env {env} tidak diset )")]
    MissingApiKey { model: String, env: String },

    #[error("error jaringan: {0}")]
    Network(#[from] reqwest::Error),

    #[error("error parse respons: {0}")]
    Parse(#[from] serde_json::Error),

    #[error("error HTTP {status} dari model {model}: {body}")]
    Http {
        status: u16,
        model: String,
        body: String,
    },

    #[error("timeout setelah {timeout_secs} detik ( model {model} )")]
    Timeout { timeout_secs: u64, model: String },

    #[error("respons tidak berisi 'choices' ( model {model} )")]
    NoChoices { model: String },

    #[error("respons kosong / tanpa konten ( model {model} )")]
    EmptyContent { model: String },

    #[error("api key mengandung karakter non-ASCII")]
    InvalidApiKey,
}

/// Header `Authorization` + `Content-Type` untuk endpoint OpenAI-compatible.
fn auth_headers(api_key: &str) -> Result<HeaderMap, LlmError> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let bearer = format!("Bearer {api_key}");
    let mut auth = HeaderValue::from_str(&bearer).map_err(|_| LlmError::InvalidApiKey)?;
    auth.set_sensitive(true);
    headers.insert(AUTHORIZATION, auth);
    Ok(headers)
}

/// Baca env var ( trim ; kosong = None ).
fn read_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

/// Ambil `choices[0].message.content` dari respons OpenAI-compatible.
fn content_of(json: &serde_json::Value) -> Option<String> {
    json.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
}

/// Klien LLM OpenAI-compatible ( routing dari `models.toml` ).
///
/// Clone murah ( `reqwest::Client` + `Arc` internal ). Satu connection pool
/// dipakai semua model ( S8.2 poin 5 ).
#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    /// Model aktif + kredensial ( bisa diganti runtime, SF7 `/model` ).
    inner: Arc<tokio::sync::RwLock<LlmInner>>,
}

#[derive(Debug)]
struct LlmInner {
    model: ModelEntry,
    base_url: String,
    api_key: String,
    timeout: Duration,
    headers: HeaderMap,
}

impl LlmClient {
    /// Bangun klien untuk `ModelEntry`. Base URL & API key dibaca dari env
    /// ( nama env var ada di `models.toml` -- kredensial tidak ditulis di
    /// file ). `base_url_override` biasanya `None` ( dipakai test / Ollama
    /// local yang URL-nya tetap ).
    pub fn new(
        model: ModelEntry,
        base_url_override: Option<&str>,
        timeout: Duration,
    ) -> Result<Self, LlmError> {
        let base_url = match base_url_override {
            Some(u) => u.to_string(),
            None => read_env(&model.base_url_env).ok_or_else(|| LlmError::MissingBaseUrl {
                model: model.id.clone(),
                env: model.base_url_env.clone(),
            })?,
        };
        let base_url = base_url.trim_end_matches('/').to_string();
        let api_key = read_env(&model.api_key_env).ok_or_else(|| LlmError::MissingApiKey {
            model: model.id.clone(),
            env: model.api_key_env.clone(),
        })?;

        let http = reqwest::Client::builder()
            .timeout(timeout)
            .https_only(true)
            .build()?;

        let headers = auth_headers(&api_key)?;
        Ok(Self {
            http,
            inner: Arc::new(tokio::sync::RwLock::new(LlmInner {
                model,
                base_url,
                api_key,
                timeout,
                headers,
            })),
        })
    }

    /// Model aktif.
    pub async fn model(&self) -> ModelEntry {
        self.inner.read().await.model.clone()
    }

    /// Ganti model aktif ( SF7: ganti model tanpa ubah kode ). Base URL &
    /// API key dibaca ulang dari env var entri baru.
    pub async fn with_model(&self, model: ModelEntry) -> Result<(), LlmError> {
        let base_url = read_env(&model.base_url_env).ok_or_else(|| LlmError::MissingBaseUrl {
            model: model.id.clone(),
            env: model.base_url_env.clone(),
        })?;
        let api_key = read_env(&model.api_key_env).ok_or_else(|| LlmError::MissingApiKey {
            model: model.id.clone(),
            env: model.api_key_env.clone(),
        })?;
        let mut inner = self.inner.write().await;
        inner.model = model;
        inner.base_url = base_url.trim_end_matches('/').to_string();
        inner.api_key = api_key.clone();
        inner.headers = auth_headers(&api_key)?;
        debug!("model aktif diganti");
        Ok(())
    }

    /// URL endpoint chat completions ( `{base}/chat/completions` ).
    fn completions_url(inner: &LlmInner) -> String {
        format!("{}/chat/completions", inner.base_url)
    }

    /// Kirim permintaan chat completion dan tunggu respons text.
    ///
    /// Konten tidak tepercaya ( SF7 ) harus dilewatkan sebagai pesan `user`
    /// yang sudah disanitasi ( lihat `sanitize_untrusted` ), tidak pernah
    /// sebagai `system`.
    pub async fn complete(&self, messages: &[ChatMessage]) -> Result<String, LlmError> {
        self.complete_with_tools(messages, &[]).await
    }

    /// Kirim chat completion dengan deklarasi tool ( OpenAI function
    /// calling ). Mengembalikan konten text.
    pub async fn complete_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> Result<String, LlmError> {
        let inner = self.inner.read().await;
        let model = &inner.model;
        let url = Self::completions_url(&inner);

        let mut body = serde_json::json!({
            "model": model.model_name,
            "messages": messages,
        });
        if let Some(max_tokens) = model.max_tokens {
            body["max_tokens"] = serde_json::json!(max_tokens);
        }
        if !tools.is_empty() {
            body["tools"] = serde_json::json!(tools);
        }

        debug!(model = %model.id, url = %url, "request chat completion");

        let res = self
            .http
            .post(url)
            .headers(inner.headers.clone())
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    LlmError::Timeout {
                        timeout_secs: inner.timeout.as_secs(),
                        model: model.id.clone(),
                    }
                } else {
                    LlmError::Network(e)
                }
            })?;

        let status = res.status().as_u16();
        if status >= 400 {
            let body = res.text().await.unwrap_or_default();
            warn!(status, model = %model.id, "endpoint menolak request");
            return Err(LlmError::Http {
                status,
                model: model.id.clone(),
                body,
            });
        }

        let json: serde_json::Value = res.json().await?;
        let content = content_of(&json).ok_or_else(|| LlmError::NoChoices {
            model: model.id.clone(),
        })?;
        if content.trim().is_empty() {
            return Err(LlmError::EmptyContent {
                model: model.id.clone(),
            });
        }
        Ok(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_entry() -> ModelEntry {
        ModelEntry {
            id: "test".into(),
            base_url_env: "NFT_TEST_LLM_URL".into(),
            api_key_env: "NFT_TEST_LLM_KEY".into(),
            model_name: "gpt-test".into(),
            max_tokens: Some(128),
        }
    }

    fn set_env() {
        std::env::set_var("NFT_TEST_LLM_URL", "https://api.example.com/v1");
        std::env::set_var("NFT_TEST_LLM_KEY", "sk-test");
    }

    const OK_RESPONSE: &str = r#"{"choices":[{"message":{"content":"halo"}}]}"#;

    #[test]
    fn header_authorization_dibangun() {
        let h = auth_headers("sk-test").unwrap();
        assert_eq!(h.get(CONTENT_TYPE).unwrap(), "application/json");
        let auth = h.get(AUTHORIZATION).unwrap().to_str().unwrap();
        assert_eq!(auth, "Bearer sk-test");
    }

    #[test]
    fn header_key_dengan_newline_ditolak() {
        // HeaderValue menolak \r / \n ( injeksi header ).
        let key = "sk-test\r\nX-Injected: 1";
        assert!(auth_headers(key).is_err());
        // Key biasa tetap diterima.
        assert!(auth_headers("sk-test-ok").is_ok());
    }

    #[test]
    fn content_of_mengambil_isi_choices() {
        let v: serde_json::Value = serde_json::from_str(OK_RESPONSE).unwrap();
        assert_eq!(content_of(&v).unwrap(), "halo");
        assert!(content_of(&serde_json::json!({"choices":[]})).is_none());
    }

    #[tokio::test]
    async fn klien_dibangun_dari_env() {
        set_env();
        let c = LlmClient::new(model_entry(), None, Duration::from_secs(3)).unwrap();
        let inner = c.inner.read().await;
        assert_eq!(inner.base_url, "https://api.example.com/v1");
        assert_eq!(
            LlmClient::completions_url(&inner),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(inner.model.id, "test");
    }

    #[tokio::test]
    async fn base_url_override_mengganti_env() {
        set_env();
        let c = LlmClient::new(
            model_entry(),
            Some("http://localhost:11431/v1"),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(c.inner.read().await.base_url, "http://localhost:11431/v1");
    }

    #[tokio::test]
    async fn env_tidak_diset_error_jelas() {
        std::env::remove_var("NFT_TEST_LLM_URL_MISSING");
        std::env::remove_var("NFT_TEST_LLM_KEY_MISSING");
        let mut m = model_entry();
        m.base_url_env = "NFT_TEST_LLM_URL_MISSING".into();
        m.api_key_env = "NFT_TEST_LLM_KEY_MISSING".into();
        let err = LlmClient::new(m, None, Duration::from_secs(3))
            .err()
            .expect("harus error");
        assert!(matches!(err, LlmError::MissingBaseUrl { .. }));
        assert!(err.to_string().contains("NFT_TEST_LLM_URL_MISSING"));
    }

    #[tokio::test]
    async fn ganti_model_tanpa_ubah_kode() {
        set_env();
        std::env::set_var("NFT_TEST_LLM_URL_2", "https://api.example.com/v1");
        std::env::set_var("NFT_TEST_LLM_KEY_2", "sk-test-2");
        let c = LlmClient::new(model_entry(), None, Duration::from_secs(3)).unwrap();
        let mut m2 = model_entry();
        m2.id = "local".into();
        m2.base_url_env = "NFT_TEST_LLM_URL_2".into();
        m2.api_key_env = "NFT_TEST_LLM_KEY_2".into();
        c.with_model(m2).await.unwrap();
        assert_eq!(c.model().await.id, "local");
    }

    #[tokio::test]
    async fn ganti_model_env_hilang_ditolak() {
        set_env();
        let c = LlmClient::new(model_entry(), None, Duration::from_secs(3)).unwrap();
        let mut m2 = model_entry();
        m2.id = "local".into();
        m2.base_url_env = "NFT_TEST_LLM_URL_MISSING".into();
        let err = c.with_model(m2).await.unwrap_err();
        assert!(matches!(err, LlmError::MissingBaseUrl { .. }));
    }

    #[tokio::test]
    async fn api_key_kosong_ditolak() {
        std::env::set_var("NFT_TEST_LLM_URL", "https://api.example.com/v1");
        std::env::set_var("NFT_TEST_LLM_KEY_EMPTY", "   ");
        let mut m = model_entry();
        m.api_key_env = "NFT_TEST_LLM_KEY_EMPTY".into();
        let err = LlmClient::new(m, None, Duration::from_secs(3))
            .err()
            .expect("harus error");
        assert!(matches!(err, LlmError::MissingApiKey { .. }));
    }
}
