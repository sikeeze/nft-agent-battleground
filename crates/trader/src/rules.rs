//! Parser `rules.toml` (blueprint §4 config, §F2).
//!
//! Format:
//! ```toml
//! [[rule]]
//! name = "floor-snipe"
//! collection = "0x..."
//! action = "buy"
//! max_price_eth = 8.0
//! below_floor_pct = 10
//! max_buy_per_day = 2
//!
//! [[rule]]
//! name = "take-profit"
//! action = "sell"
//! take_profit_pct = 25
//! stop_loss_pct = 15
//! list_duration_hours = 72
//! ```

use alloy::primitives::Address;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Config `rules.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RulesConfig {
    #[serde(default)]
    pub rule: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    Buy,
    Sell,
    Custom,
}

/// Aksi rule. Varian baku diparsing dari string kecil; teks lainnya
/// menjadi `Custom`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleAction {
    Buy,
    Sell,
    List,
    Bid,
    Cancel,
    Custom(String),
}

impl RuleAction {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "buy" => Self::Buy,
            "sell" => Self::Sell,
            "list" => Self::List,
            "bid" => Self::Bid,
            "cancel" => Self::Cancel,
            other => Self::Custom(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
            Self::List => "list",
            Self::Bid => "bid",
            Self::Cancel => "cancel",
            Self::Custom(s) => s,
        }
    }
}

impl std::fmt::Display for RuleAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Satu rule trading.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Nama rule ( unik ).
    pub name: String,
    /// `buy | sell | custom`.
    pub kind: RuleKind,
    /// Alamat koleksi target.
    pub collection: String,
    /// Aksi yang dilakukan saat rule cocok ( `buy | sell | list | bid | cancel | <custom>` ).
    pub action: String,
    /// Batas harga beli ( ETH ).
    #[serde(default)]
    pub max_price_eth: Option<f64>,
    /// Beli bila harga `below_floor_pct`% di bawah floor.
    #[serde(default)]
    pub below_floor_pct: Option<f64>,
    /// Maksimal pembelian koleksi ini per hari.
    #[serde(default)]
    pub max_buy_per_day: Option<u32>,
    /// Take profit ( % di atas harga beli ).
    #[serde(default)]
    pub take_profit_pct: Option<f64>,
    /// Stop loss ( % di bawah harga beli ).
    #[serde(default)]
    pub stop_loss_pct: Option<f64>,
    /// Durasi listing ( jam ).
    #[serde(default)]
    pub list_duration_hours: Option<u64>,
    /// Rule aktif?
    #[serde(default = "d_true")]
    pub enabled: bool,
}

fn d_true() -> bool {
    true
}

#[derive(Debug, Error)]
pub enum RulesError {
    #[error("rules.toml tidak valid: {source}")]
    Parse {
        #[source]
        source: toml::de::Error,
    },

    #[error("rule '{name}': {msg}")]
    Validation { name: String, msg: String },

    #[error("rule '{name}': alamat koleksi tidak valid")]
    InvalidCollection { name: String },

    #[error("nama rule duplikat: {0}")]
    DuplicateName(String),
}

impl Rule {
    /// Validasi satu rule.
    pub fn validate(&self) -> Result<(), RulesError> {
        let name = &self.name;
        if name.trim().is_empty() {
            return Err(RulesError::Validation {
                name: name.clone(),
                msg: "name tidak boleh kosong".into(),
            });
        }
        if self.collection.trim().is_empty() {
            return Err(RulesError::Validation {
                name: name.clone(),
                msg: "collection wajib diisi".into(),
            });
        }
        if self.collection.parse::<Address>().is_err() {
            return Err(RulesError::InvalidCollection { name: name.clone() });
        }
        // Rule buy wajib ada setidaknya satu kriteria harga.
        if matches!(self.kind, RuleKind::Buy)
            && self.max_price_eth.is_none()
            && self.below_floor_pct.is_none()
        {
            return Err(RulesError::Validation {
                name: name.clone(),
                msg: "rule buy wajib mengisi max_price_eth dan/atau below_floor_pct".into(),
            });
        }
        // Rule sell wajib take profit atau stop loss.
        if matches!(self.kind, RuleKind::Sell)
            && self.take_profit_pct.is_none()
            && self.stop_loss_pct.is_none()
        {
            return Err(RulesError::Validation {
                name: name.clone(),
                msg: "rule sell wajib mengisi take_profit_pct atau stop_loss_pct".into(),
            });
        }
        if let Some(pct) = self.below_floor_pct {
            if !pct.is_finite() || pct < 0.0 || pct > 100.0 {
                return Err(RulesError::Validation {
                    name: name.clone(),
                    msg: format!("below_floor_pct harus 0..=100 (didapat {pct})"),
                });
            }
        }
        if let Some(pct) = self.take_profit_pct {
            if !pct.is_finite() || pct <= 0.0 {
                return Err(RulesError::Validation {
                    name: name.clone(),
                    msg: format!("take_profit_pct harus > 0 (didapat {pct})"),
                });
            }
        }
        if let Some(pct) = self.stop_loss_pct {
            if !pct.is_finite() || pct <= 0.0 {
                return Err(RulesError::Validation {
                    name: name.clone(),
                    msg: format!("stop_loss_pct harus > 0 (didapat {pct})"),
                });
            }
        }
        if let Some(max) = self.max_price_eth {
            if !max.is_finite() || max <= 0.0 {
                return Err(RulesError::Validation {
                    name: name.clone(),
                    msg: format!("max_price_eth harus > 0 (didapat {max})"),
                });
            }
        }
        Ok(())
    }

    /// Alamat koleksi sebagai `Address` ( sudah divalidasi ).
    pub fn collection_addr(&self) -> Address {
        self.collection.parse().expect("divalidasi di validate()")
    }

    /// Aksi sebagai enum. String `"custom:nama"` -> `Custom("nama")`.
    pub fn action_enum(&self) -> RuleAction {
        match self.action.strip_prefix("custom:") {
            Some(rest) => RuleAction::Custom(rest.to_string()),
            None => RuleAction::parse(&self.action),
        }
    }
}

impl RulesConfig {
    pub fn validate(&self) -> Result<(), RulesError> {
        let mut seen = Vec::new();
        for r in &self.rule {
            r.validate()?;
            if seen.contains(&r.name) {
                return Err(RulesError::DuplicateName(r.name.clone()));
            }
            seen.push(r.name.clone());
        }
        Ok(())
    }
}

/// Parse `rules.toml` dari string ( dipakai loader config ).
pub fn parse_rules(input: &str) -> Result<RulesConfig, RulesError> {
    let c: RulesConfig = toml::from_str(input).map_err(|e| RulesError::Parse { source: e })?;
    c.validate()?;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES_OK: &str = r#"
        [[rule]]
        name = "floor-snipe"
        kind = "buy"
        collection = "0x1e0e7eF5D8C2a093F9c2d5b8e2e2e2e2e2e2e2e2"
        action = "buy"
        max_price_eth = 8.0
        below_floor_pct = 10
        max_buy_per_day = 2

        [[rule]]
        name = "take-profit"
        kind = "sell"
        collection = "0x1e0e7eF5D8C2a093F9c2d5b8e2e2e2e2e2e2e2e2"
        action = "list"
        take_profit_pct = 25
        stop_loss_pct = 15
        list_duration_hours = 72
    "#;

    #[test]
    fn parse_dua_rule_valid() {
        let cfg = parse_rules(RULES_OK).unwrap();
        assert_eq!(cfg.rule.len(), 2);
        let buy = &cfg.rule[0];
        assert_eq!(buy.name, "floor-snipe");
        assert_eq!(buy.kind, RuleKind::Buy);
        assert_eq!(buy.action_enum(), RuleAction::Buy);
        assert_eq!(buy.max_price_eth, Some(8.0));
        assert_eq!(buy.below_floor_pct, Some(10.0));
        assert_eq!(buy.max_buy_per_day, Some(2));
        assert!(buy.enabled);
        let sell = &cfg.rule[1];
        assert_eq!(sell.action_enum(), RuleAction::List);
        assert_eq!(sell.take_profit_pct, Some(25.0));
        assert_eq!(sell.list_duration_hours, Some(72));
    }

    #[test]
    fn field_tidak_dikenal_ditolak() {
        let bad = RULES_OK.replace("max_price_eth = 8.0", "max_price_eth = 8.0\ntypo = 1");
        assert!(parse_rules(&bad).is_err());
    }

    #[test]
    fn nama_duplikat_ditolak() {
        let bad = RULES_OK.replace("\"take-profit\"", "\"floor-snipe\"");
        let err = parse_rules(&bad).unwrap_err();
        assert!(err.to_string().contains("duplikat"));
    }

    #[test]
    fn buy_tanpa_kriteria_harga_ditolak() {
        let bad = r#"
        [[rule]]
        name = "no-crit"
        kind = "buy"
        collection = "0x1e0e7eF5D8C2a093F9c2d5b8e2e2e2e2e2e2e2e2"
        action = "buy"
        "#;
        let err = parse_rules(bad).unwrap_err();
        assert!(err.to_string().contains("max_price_eth"));
    }

    #[test]
    fn sell_tanpa_tp_sl_ditolak() {
        let bad = r#"
        [[rule]]
        name = "no-tp"
        kind = "sell"
        collection = "0x1e0e7eF5D8C2a093F9c2d5b8e2e2e2e2e2e2e2e2"
        action = "list"
        "#;
        let err = parse_rules(bad).unwrap_err();
        assert!(err.to_string().contains("take_profit_pct"));
    }

    #[test]
    fn collection_invalid_ditolak() {
        let bad = RULES_OK.replace(
            "\"0x1e0e7eF5D8C2a093F9c2d5b8e2e2e2e2e2e2e2e2\"",
            "\"bukan-alamat\"",
        );
        let err = parse_rules(&bad).unwrap_err();
        assert!(err.to_string().contains("alamat koleksi"));
    }

    #[test]
    fn below_floor_pct_di_luar_jangkauan_ditolak() {
        let bad = RULES_OK.replace("below_floor_pct = 10", "below_floor_pct = 150");
        assert!(parse_rules(&bad).is_err());
    }

    #[test]
    fn take_profit_negatif_ditolak() {
        let bad = RULES_OK.replace("take_profit_pct = 25", "take_profit_pct = -5");
        assert!(parse_rules(&bad).is_err());
    }

    #[test]
    fn max_price_nol_ditolak() {
        let bad = RULES_OK.replace("max_price_eth = 8.0", "max_price_eth = 0.0");
        assert!(parse_rules(&bad).is_err());
    }

    #[test]
    fn aturan_kosong_valid() {
        let cfg = parse_rules("").unwrap();
        assert!(cfg.rule.is_empty());
    }

    #[test]
    fn kind_custom_diterima() {
        let raw = r#"
        [[rule]]
        name = "c-1"
        kind = "custom"
        collection = "0x1e0e7eF5D8C2a093F9c2d5b8e2e2e2e2e2e2e2e2"
        action = "custom:alert"
        "#;
        let cfg = parse_rules(raw).unwrap();
        assert_eq!(cfg.rule[0].kind, RuleKind::Custom);
        assert_eq!(
            cfg.rule[0].action_enum(),
            RuleAction::Custom("alert".into())
        );
    }

    #[test]
    fn enabled_bisa_dimatikan() {
        let raw = RULES_OK.replace(
            "max_buy_per_day = 2",
            "max_buy_per_day = 2\nenabled = false",
        );
        let cfg = parse_rules(&raw).unwrap();
        assert!(!cfg.rule[0].enabled);
    }

    #[test]
    fn collection_addr_parse() {
        let cfg = parse_rules(RULES_OK).unwrap();
        let addr = cfg.rule[0].collection_addr();
        assert_eq!(
            addr.to_string().to_lowercase(),
            "0x1e0e7ef5d8c2a093f9c2d5b8e2e2e2e2e2e2e2e2"
        );
    }
}
