//! Config loader + validasi ketat (blueprint §4, §9 Fase 1).
//!
//! Prinsip fail-fast: config invalid harus menolak startup dengan pesan yang
//! menyebut nama file & field masalahnya. Semua file TOML memakai
//! `deny_unknown_fields` — salah ketik nama field = error, bukan diam-diam
//! jatuh ke default.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use serde::Deserialize;

use crate::event::Priority;

pub const AGENT_FILE: &str = "agent.toml";
pub const POLICY_FILE: &str = "policy.toml";
pub const MODELS_FILE: &str = "models.toml";

/// Template WhatsApp utility yang wajib terdaftar bila WhatsApp aktif (§3C poin 3).
const REQUIRED_WHATSAPP_TEMPLATES: [&str; 7] = [
    "mint_result",
    "war_started",
    "preflight_failed",
    "session_expired",
    "low_balance",
    "killswitch_triggered",
    "listing_sold",
];

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("gagal membaca {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{file} tidak valid: {source}")]
    Parse {
        file: &'static str,
        #[source]
        source: toml::de::Error,
    },
    #[error("{file}: {msg}")]
    Validation { file: &'static str, msg: String },
}

fn parse_err(file: &'static str, source: toml::de::Error) -> ConfigError {
    ConfigError::Parse { file, source }
}

fn val_err(file: &'static str, msg: impl fmt::Display) -> ConfigError {
    ConfigError::Validation {
        file,
        msg: msg.to_string(),
    }
}

fn check_finite(file: &'static str, field: &str, v: f64) -> Result<(), ConfigError> {
    if !v.is_finite() || v < 0.0 {
        Err(val_err(
            file,
            format!("{field} harus angka >= 0 yang valid (didapat: {v})"),
        ))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// policy.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    pub limits: Limits,
    pub allowlist: Allowlist,
    pub confirmation: Confirmation,
    pub kill_switch: KillSwitch,
    pub telegram: TelegramPolicy,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_spend_per_tx_eth: f64,
    pub max_spend_per_day_eth: f64,
    pub max_gas_gwei: u64,
    pub allowed_chains: Vec<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allowlist {
    #[serde(default)]
    pub contracts: Vec<String>,
    /// Wajib aman secara default: bila field tidak ada, blokir approval tak terbatas.
    #[serde(default = "d_true")]
    pub block_unlimited_approvals: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Confirmation {
    pub require_human_above_eth: f64,
    pub require_human_for: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KillSwitch {
    pub auto_trigger_on_loss_eth: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramPolicy {
    pub allowed_user_ids: Vec<i64>,
    pub chat_cannot: Vec<String>,
    /// Mati secara default (§3A): import lewat chat hanya untuk wallet burner.
    #[serde(default)]
    pub import_via_chat: bool,
    pub import_session_timeout_sec: u64,
    /// Aman secara default: pesan /import langsung dihapus bot.
    #[serde(default = "d_true")]
    pub delete_import_message: bool,
    #[serde(default = "d_true")]
    pub require_totp_for_import: bool,
    pub burner_max_balance_eth: f64,
    pub max_wallets: u32,
}

impl PolicyConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        let f = POLICY_FILE;
        let l = &self.limits;

        check_finite(f, "limits.max_spend_per_tx_eth", l.max_spend_per_tx_eth)?;
        check_finite(f, "limits.max_spend_per_day_eth", l.max_spend_per_day_eth)?;
        if l.max_gas_gwei == 0 {
            return Err(val_err(f, "limits.max_gas_gwei harus > 0"));
        }
        if l.allowed_chains.is_empty() {
            return Err(val_err(f, "limits.allowed_chains tidak boleh kosong"));
        }
        if l.max_spend_per_day_eth < l.max_spend_per_tx_eth {
            return Err(val_err(
                f,
                format!(
                    "limits.max_spend_per_day_eth ({}) harus >= limits.max_spend_per_tx_eth ({})",
                    l.max_spend_per_day_eth, l.max_spend_per_tx_eth
                ),
            ));
        }

        check_finite(
            f,
            "confirmation.require_human_above_eth",
            self.confirmation.require_human_above_eth,
        )?;
        check_finite(
            f,
            "kill_switch.auto_trigger_on_loss_eth",
            self.kill_switch.auto_trigger_on_loss_eth,
        )?;

        let t = &self.telegram;
        if t.allowed_user_ids.is_empty() {
            return Err(val_err(
                f,
                "telegram.allowed_user_ids tidak boleh kosong (whitelist user_id wajib, §3 poin 2)",
            ));
        }
        if t.max_wallets == 0 {
            return Err(val_err(f, "telegram.max_wallets harus >= 1"));
        }
        check_finite(
            f,
            "telegram.burner_max_balance_eth",
            t.burner_max_balance_eth,
        )?;
        if t.import_session_timeout_sec < 30 {
            return Err(val_err(
                f,
                "telegram.import_session_timeout_sec terlalu pendek (minimal 30 detik)",
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// agent.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyChannel {
    Telegram,
    Whatsapp,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Default true (§1 "Mode awal: Dry-run"). false hanya setelah uji bertahap.
    #[serde(default = "d_true")]
    pub dry_run: bool,
    pub notify: NotifyConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifyConfig {
    pub primary: NotifyChannel,
    #[serde(default = "d_true")]
    pub critical_to_all: bool,
    #[serde(default)]
    pub quiet_hours: Option<String>,
    #[serde(default = "d_dedupe")]
    pub dedupe_window_sec: u64,
    pub whatsapp: WhatsappConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WhatsappConfig {
    pub enabled: bool,
    #[serde(default)]
    pub phone_number_id: Option<String>,
    /// Token disimpan di env/secret VPS, bukan di file (§3C poin 8).
    pub access_token_env: String,
    #[serde(default)]
    pub recipient: Option<String>,
    pub min_priority: Priority,
    pub max_per_hour: u32,
    pub monthly_budget_msgs: u32,
    #[serde(default)]
    pub templates: HashMap<String, String>,
}

impl AgentConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        let f = AGENT_FILE;
        let n = &self.notify;

        if n.primary != NotifyChannel::Telegram {
            return Err(val_err(
                f,
                "notify.primary harus 'telegram' — semua kontrol hanya lewat Telegram; \
                 WhatsApp hanya notifikasi satu arah (§3C)",
            ));
        }
        if let Some(qh) = &n.quiet_hours {
            validate_quiet_hours(qh)
                .map_err(|msg| val_err(f, format!("notify.quiet_hours '{qh}': {msg}")))?;
        }

        let w = &n.whatsapp;
        if w.enabled {
            if w.phone_number_id.as_ref().is_none_or(String::is_empty) {
                return Err(val_err(
                    f,
                    "notify.whatsapp.phone_number_id wajib diisi bila enabled = true",
                ));
            }
            if w.recipient.as_ref().is_none_or(String::is_empty) {
                return Err(val_err(
                    f,
                    "notify.whatsapp.recipient wajib diisi bila enabled = true",
                ));
            }
            if w.access_token_env.trim().is_empty() {
                return Err(val_err(
                    f,
                    "notify.whatsapp.access_token_env wajib diisi (token di env, bukan file)",
                ));
            }
            if w.max_per_hour == 0 || w.monthly_budget_msgs == 0 {
                return Err(val_err(
                    f,
                    "notify.whatsapp.max_per_hour dan monthly_budget_msgs harus >= 1",
                ));
            }
            let missing: Vec<&str> = REQUIRED_WHATSAPP_TEMPLATES
                .iter()
                .filter(|t| !w.templates.contains_key(**t))
                .copied()
                .collect();
            if !missing.is_empty() {
                return Err(val_err(
                    f,
                    format!(
                        "notify.whatsapp.templates kurang: {} (template utility wajib terdaftar, §3C poin 3)",
                        missing.join(", ")
                    ),
                ));
            }
        }
        Ok(())
    }
}

fn validate_quiet_hours(s: &str) -> Result<(), String> {
    let (a, b) = s.split_once('-').ok_or("format harus 'HH:MM-HH:MM'")?;
    parse_hhmm(a).map_err(|e| format!("mulai invalid: {e}"))?;
    parse_hhmm(b).map_err(|e| format!("akhir invalid: {e}"))?;
    Ok(())
}

fn parse_hhmm(s: &str) -> Result<(u32, u32), String> {
    let (h, m) = s.split_once(':').ok_or("format harus 'HH:MM'")?;
    let h: u32 = h.parse().map_err(|_| format!("jam '{h}' bukan angka"))?;
    let m: u32 = m.parse().map_err(|_| format!("menit '{m}' bukan angka"))?;
    if h > 23 || m > 59 {
        return Err(format!("jam:menit di luar rentang (dapat {h}:{m:02})"));
    }
    Ok((h, m))
}

// ---------------------------------------------------------------------------
// models.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsConfig {
    pub default: String,
    /// LLM tidak boleh di jalur eksekusi mint (§8.2 poin 3): timeout pendek wajib.
    #[serde(default = "d_plan_timeout")]
    pub plan_timeout_secs: u64,
    #[serde(default)]
    pub model: Vec<ModelEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    pub id: String,
    /// Nama env var tempat URL endpoint; URL tidak ditulis di file.
    pub base_url_env: String,
    /// Nama env var tempat API key; kunci tidak ditulis di file (§F7).
    pub api_key_env: String,
    pub model_name: String,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

impl ModelsConfig {
    pub fn default_model(&self) -> Option<&ModelEntry> {
        self.model.iter().find(|m| m.id == self.default)
    }

    pub fn model(&self, id: &str) -> Option<&ModelEntry> {
        self.model.iter().find(|m| m.id == id)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let f = MODELS_FILE;
        if self.model.is_empty() {
            return Err(val_err(f, "minimal satu entri [[model]] wajib ada"));
        }
        let mut seen: Vec<&str> = Vec::new();
        for m in &self.model {
            if m.id.trim().is_empty() {
                return Err(val_err(f, "model.id tidak boleh kosong"));
            }
            if seen.contains(&m.id.as_str()) {
                return Err(val_err(f, format!("model.id '{}' duplikat", m.id)));
            }
            seen.push(&m.id);
            if m.base_url_env.trim().is_empty() || m.api_key_env.trim().is_empty() {
                return Err(val_err(
                    f,
                    format!(
                        "model '{}' wajib mengisi base_url_env dan api_key_env",
                        m.id
                    ),
                ));
            }
            if m.model_name.trim().is_empty() {
                return Err(val_err(f, format!("model '{}': model_name kosong", m.id)));
            }
        }
        if self.default_model().is_none() {
            return Err(val_err(
                f,
                format!(
                    "default '{}' tidak ada dalam daftar model (pilihan: {})",
                    self.default,
                    seen.join(", ")
                ),
            ));
        }
        if self.plan_timeout_secs == 0 {
            return Err(val_err(
                f,
                "plan_timeout_secs harus >= 1 (LLM wajib punya timeout, §8.)",
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Configs {
    pub agent: AgentConfig,
    pub policy: PolicyConfig,
    pub models: ModelsConfig,
}

/// Muat 3 file config dari satu direktori dan validasi semuanya.
pub fn load_dir(dir: &Path) -> Result<Configs, ConfigError> {
    Ok(Configs {
        agent: load_agent(&dir.join(AGENT_FILE))?,
        policy: load_policy(&dir.join(POLICY_FILE))?,
        models: load_models(&dir.join(MODELS_FILE))?,
    })
}

pub fn parse_agent(input: &str) -> Result<AgentConfig, ConfigError> {
    let c: AgentConfig = toml::from_str(input).map_err(|e| parse_err(AGENT_FILE, e))?;
    c.validate()?;
    Ok(c)
}

pub fn parse_policy(input: &str) -> Result<PolicyConfig, ConfigError> {
    let c: PolicyConfig = toml::from_str(input).map_err(|e| parse_err(POLICY_FILE, e))?;
    c.validate()?;
    Ok(c)
}

pub fn parse_models(input: &str) -> Result<ModelsConfig, ConfigError> {
    let c: ModelsConfig = toml::from_str(input).map_err(|e| parse_err(MODELS_FILE, e))?;
    c.validate()?;
    Ok(c)
}

pub fn load_agent(path: &Path) -> Result<AgentConfig, ConfigError> {
    parse_agent(&read_file(path)?)
}

pub fn load_policy(path: &Path) -> Result<PolicyConfig, ConfigError> {
    parse_policy(&read_file(path)?)
}

pub fn load_models(path: &Path) -> Result<ModelsConfig, ConfigError> {
    parse_models(&read_file(path)?)
}

fn read_file(path: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(path).map_err(|e| ConfigError::Read {
        path: path.display().to_string(),
        source: e,
    })
}

// --- nilai default aman ------------------------------------------------------

fn d_true() -> bool {
    true
}
fn d_dedupe() -> u64 {
    120
}
fn d_plan_timeout() -> u64 {
    3
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    const POLICY_OK: &str = r#"
        [limits]
        max_spend_per_tx_eth = 0.5
        max_spend_per_day_eth = 2.0
        max_gas_gwei = 80
        allowed_chains = [1, 8453, 42161]

        [allowlist]
        contracts = []
        block_unlimited_approvals = true

        [confirmation]
        require_human_above_eth = 0.2
        require_human_for = ["new_contract", "withdraw"]

        [kill_switch]
        auto_trigger_on_loss_eth = 1.0

        [telegram]
        allowed_user_ids = [123456789]
        chat_cannot = ["edit_policy", "export_key"]
        import_via_chat = true
        import_session_timeout_sec = 120
        delete_import_message = true
        require_totp_for_import = true
        burner_max_balance_eth = 0.5
        max_wallets = 5
    "#;

    const AGENT_OK: &str = r#"
        dry_run = true

        [notify]
        primary = "telegram"
        critical_to_all = true
        quiet_hours = "23:00-06:00"
        dedupe_window_sec = 120

        [notify.whatsapp]
        enabled = true
        phone_number_id = "123"
        access_token_env = "WA_ACCESS_TOKEN"
        recipient = "+628123"
        min_priority = "high"
        max_per_hour = 20
        monthly_budget_msgs = 500

        [notify.whatsapp.templates]
        mint_result = "mint_result_v1"
        war_started = "war_started_v1"
        preflight_failed = "preflight_failed_v1"
        session_expired = "session_expired_v1"
        low_balance = "low_balance_v1"
        killswitch_triggered = "killswitch_v1"
        listing_sold = "listing_sold_v1"
    "#;

    const MODELS_OK: &str = r#"
        default = "planner-large"
        plan_timeout_secs = 3

        [[model]]
        id = "planner-large"
        base_url_env = "NFT_LLM_PLANNER_URL"
        api_key_env = "NFT_LLM_PLANNER_KEY"
        model_name = "claude-sonnet-4-5"
        max_tokens = 2048

        [[model]]
        id = "chat-local"
        base_url_env = "NFT_LLM_CHAT_URL"
        api_key_env = "NFT_LLM_CHAT_KEY"
        model_name = "llama-3.1-8b"
    "#;

    #[test]
    fn contoh_config_valid_diterima() {
        let policy = parse_policy(POLICY_OK).unwrap();
        assert_eq!(policy.limits.max_gas_gwei, 80);
        assert_eq!(policy.telegram.burner_max_balance_eth, 0.5);

        let agent = parse_agent(AGENT_OK).unwrap();
        assert!(agent.dry_run);
        assert_eq!(agent.notify.primary, NotifyChannel::Telegram);
        assert_eq!(
            agent.notify.whatsapp.templates["mint_result"],
            "mint_result_v1"
        );

        let models = parse_models(MODELS_OK).unwrap();
        assert_eq!(models.default_model().unwrap().id, "planner-large");
        assert!(models.model("chat-local").is_some());
    }

    #[test]
    fn field_tidak_dikenal_ditolak() {
        let bad = POLICY_OK.replace("max_gas_gwei = 80", "max_gas_gwei = 80\ntypo_field = 1");
        let err = parse_policy(&bad).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("unknown field"),
            "pesan harus menyebut unknown field, dapat: {msg}"
        );
    }

    #[test]
    fn limit_per_day_lebih_kecil_dari_per_tx_ditolak() {
        let bad = POLICY_OK.replace("max_spend_per_day_eth = 2.0", "max_spend_per_day_eth = 0.1");
        let err = parse_policy(&bad).unwrap_err();
        assert!(err.to_string().contains("max_spend_per_day_eth"));
    }

    #[test]
    fn whitelist_kosong_ditolak() {
        let bad = POLICY_OK.replace("allowed_user_ids = [123456789]", "allowed_user_ids = []");
        let err = parse_policy(&bad).unwrap_err();
        assert!(err.to_string().contains("allowed_user_ids"));
    }

    #[test]
    fn import_timeout_terlalu_pendek_ditolak() {
        let bad = POLICY_OK.replace(
            "import_session_timeout_sec = 120",
            "import_session_timeout_sec = 5",
        );
        let err = parse_policy(&bad).unwrap_err();
        assert!(err.to_string().contains("import_session_timeout_sec"));
    }

    #[test]
    fn gas_nol_ditolak() {
        let bad = POLICY_OK.replace("max_gas_gwei = 80", "max_gas_gwei = 0");
        assert!(parse_policy(&bad).is_err());
    }

    #[test]
    fn whatsapp_enabled_tanpa_phone_ditolak() {
        let bad = AGENT_OK.replace("phone_number_id = \"123\"", "phone_number_id = \"\"");
        let err = parse_agent(&bad).unwrap_err();
        assert!(err.to_string().contains("phone_number_id"));
    }

    #[test]
    fn template_whatsapp_kurang_ditolak() {
        let bad = AGENT_OK.replace("listing_sold = \"listing_sold_v1\"\n", "");
        let err = parse_agent(&bad).unwrap_err();
        assert!(err.to_string().contains("templates kurang"));
    }

    #[test]
    fn quiet_hours_invalid_ditolak() {
        let bad = AGENT_OK.replace(
            "quiet_hours = \"23:00-06:00\"",
            "quiet_hours = \"25:00-08:00\"",
        );
        let err = parse_agent(&bad).unwrap_err();
        assert!(err.to_string().contains("quiet_hours"));
    }

    #[test]
    fn primary_bukan_telegram_ditolak() {
        let bad = AGENT_OK.replace("primary = \"telegram\"", "primary = \"whatsapp\"");
        let err = parse_agent(&bad).unwrap_err();
        assert!(err.to_string().contains("primary"));
    }

    #[test]
    fn model_id_duplikat_ditolak() {
        let bad = MODELS_OK.replace("id = \"chat-local\"", "id = \"planner-large\"");
        let err = parse_models(&bad).unwrap_err();
        assert!(err.to_string().contains("duplikat"));
    }

    #[test]
    fn default_tak_ada_di_daftar_ditolak() {
        let bad = MODELS_OK.replace("default = \"planner-large\"", "default = \"tidak-ada\"");
        let err = parse_models(&bad).unwrap_err();
        assert!(err.to_string().contains("tidak ada dalam daftar"));
    }

    #[test]
    fn models_kosong_ditolak() {
        let err = parse_models("default = \"x\"").unwrap_err();
        assert!(err.to_string().contains("[[model]]"));
    }

    #[test]
    fn load_dir_membaca_tiga_config() {
        let keep = tempfile::tempdir().unwrap();
        let dir = keep.path();
        fs::write(
            dir.join(AGENT_FILE),
            "dry_run = true\n[notify]\nprimary = \"telegram\"\n[notify.whatsapp]\nenabled = false\naccess_token_env = \"WA\"\nmin_priority = \"low\"\nmax_per_hour = 10\nmonthly_budget_msgs = 100",
        )
        .unwrap();
        fs::write(dir.join(POLICY_FILE), POLICY_OK).unwrap();
        fs::write(dir.join(MODELS_FILE), MODELS_OK).unwrap();

        let cfg = load_dir(dir).unwrap();
        assert!(cfg.agent.dry_run);
        assert!(!cfg.agent.notify.whatsapp.enabled); // disabled tidak butuh templates
        assert_eq!(cfg.policy.limits.allowed_chains, [1, 8453, 42161]);
    }

    #[test]
    fn file_hilang_menghasilkan_error_read() {
        let err = load_dir(Path::new("/nonexistent/dir")).unwrap_err();
        assert!(
            err.to_string().contains("gagal membaca"),
            "pesan harus jelas, dapat: {err}"
        );
    }
}
