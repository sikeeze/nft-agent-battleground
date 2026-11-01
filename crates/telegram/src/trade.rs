//! Perintah trade: `/portfolio`, `/rules` (blueprint §F6, §F2, Fase 8).
//!
//! - `/portfolio` — ringkasan PnL ( data akuntansi, bebas rahasia ).
//! - `/rules` — daftar rule aktif dari `rules.toml`.
//! - `/rules reload` — muat ulang `rules.toml` ( parsing + validasi ).
//!
//! **Aturan**: perintah ini hanya **membaca**. Eksekusi jual/beli tetap
//! lewat Policy Engine + konfirmasi tombol ( aturan keras #2, §F5 ).

use std::path::Path;

use nft_trader::rules::{parse_rules, RulesConfig, RulesError};

/// Hasil perintah trade untuk chat.
#[derive(Debug, Clone)]
pub struct TradeOutcome {
    pub text: String,
}

/// Muat `rules.toml` dari path ( validasi ketat ).
pub fn load_rules(path: &Path) -> Result<RulesConfig, RulesError> {
    let text = std::fs::read_to_string(path).map_err(|e| RulesError::Validation {
        name: path.display().to_string(),
        msg: format!("gagal membaca rules.toml: {e}"),
    })?;
    parse_rules(&text)
}

/// `/rules` — daftar rule.
pub fn list_rules(config: &RulesConfig) -> TradeOutcome {
    if config.rule.is_empty() {
        return TradeOutcome {
            text: "Tidak ada rule di rules.toml. Tambahkan [[rule]] ( lihat §F2 ).".into(),
        };
    }
    let mut lines = Vec::with_capacity(config.rule.len());
    for r in &config.rule {
        let status = if r.enabled { "aktif" } else { "mati" };
        lines.push(format!(
            "- {} [{status}] kind={:?} action={} koleksi={}",
            r.name, r.kind, r.action, r.collection
        ));
    }
    TradeOutcome {
        text: format!(
            "Rule trading ({}) :\n{}\n\n/rules reload - muat ulang rules.toml",
            config.rule.len(),
            lines.join("\n")
        ),
    }
}

/// `/portfolio` — ringkasan PnL.
pub fn portfolio_summary(portfolio: &nft_trader::Portfolio) -> TradeOutcome {
    TradeOutcome {
        text: portfolio.summarize(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const RULES_OK: &str = r#"
        [[rule]]
        name = "floor-snipe"
        kind = "buy"
        collection = "0x1e0e7eF5D8C2a093F9c2d5b8e2e2e2e2e2e2e2e2"
        action = "buy"
        max_price_eth = 8.0
        below_floor_pct = 10
        max_buy_per_day = 2
    "#;

    #[test]
    fn load_rules_valid() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rules.toml");
        std::fs::write(&path, RULES_OK).unwrap();
        let cfg = load_rules(&path).unwrap();
        assert_eq!(cfg.rule.len(), 1);
    }

    #[test]
    fn load_rules_file_hilang_error_jelas() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rules.toml");
        let err = load_rules(&path).unwrap_err();
        assert!(err.to_string().contains("gagal membaca"));
    }

    #[test]
    fn load_rules_invalid_ditolak() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rules.toml");
        std::fs::write(&path, "[[rule]]\nname = \"x\"").unwrap();
        let err = load_rules(&path).unwrap_err();
        assert!(err.to_string().contains("rules.toml"));
    }

    #[test]
    fn list_rules_menampilkan_rule() {
        let cfg = parse_rules(RULES_OK).unwrap();
        let out = list_rules(&cfg);
        assert!(out.text.contains("floor-snipe"));
        assert!(out.text.contains("aktif"));
        assert!(out.text.contains("1"));
    }

    #[test]
    fn list_rules_kosong_petunjuk() {
        let cfg = parse_rules("").unwrap();
        let out = list_rules(&cfg);
        assert!(out.text.contains("Tidak ada rule"));
    }

    #[test]
    fn portfolio_summary_dari_summary_portfolio() {
        let mut p = nft_trader::Portfolio::new();
        p.record_buy(
            "0x0101010101010101010101010101010101010101"
                .parse()
                .unwrap(),
            alloy::primitives::U256::from(1),
            1.0,
            1,
        );
        let out = portfolio_summary(&p);
        assert!(out.text.contains("Total:"));
    }

    #[test]
    fn portfolio_kosong_jelas() {
        let p = nft_trader::Portfolio::new();
        let out = portfolio_summary(&p);
        assert_eq!(out.text, "Portfolio kosong.");
    }
}
