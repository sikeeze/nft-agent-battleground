//! Manifest skill `skill.toml` (blueprint SF8).
//!
//! Satu folder = satu skill:
//!
//! ```text
//! skills/floor_sniper/
//!   skill.toml      # manifest + permission
//!   run.py          # script ( python/node/bash -- apapun yg ada di PATH )
//! ```
//!
//! Contoh `skill.toml`:
//!
//! ```toml
//! name = "floor_sniper"
//! description = "Pantau floor koleksi & usulkan buy di bawah ambang"
//! version = "1"
//! timeout_secs = 5
//! script = "run.py"
//! interpreter = "python"
//!
//! [permissions]
//! allowed_tools = ["get_floor", "get_listings"]
//! allowed_domains = ["api.opensea.io"]
//! max_calls_per_hour = 30
//! ```
//!
//! Field wajib: `name`, `script`. Permission default **deny-all**: skill
//! tanpa deklarasi tool tidak bisa memanggil tool apapun.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::AllowedTool;

/// Konfigurasi permission skill ( SF8 "Sandbox dengan permission eksplisit" ).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillPermission {
    /// Tool yang boleh dipanggil skill ( subset `AllowedTool` ).
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Domain jaringan yang boleh diakses ( kosong = tidak boleh akses
    /// jaringan apapun ).
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Batas pemanggilan per jam.
    #[serde(default = "d_max_calls")]
    pub max_calls_per_hour: u32,
    /// Skill boleh menghasilkan intent? ( default false -- skill hanya
    /// mengamati / melapor ).
    #[serde(default)]
    pub can_propose_intent: bool,
}

fn d_max_calls() -> u32 {
    30
}

impl Default for SkillPermission {
    fn default() -> SkillPermission {
        SkillPermission {
            allowed_tools: Vec::new(),
            allowed_domains: Vec::new(),
            max_calls_per_hour: 30,
            can_propose_intent: false,
        }
    }
}

/// Tool yang diizinkan untuk skill ini ( sudah divalidasi terhadap
/// `AllowedTool` ).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillTool {
    pub tool: AllowedTool,
}

/// Manifest skill ( `skill.toml` ).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillManifest {
    /// Nama skill ( unik, hanya alfanumerik + `_` + `-` ).
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "d_version")]
    pub version: String,
    /// Batas waktu eksekusi skill ( detik ). Default 5 ( SF8 ).
    #[serde(default = "d_timeout")]
    pub timeout_secs: u64,
    /// Nama file script ( relatif terhadap folder skill ).
    pub script: String,
    /// Interpreter: `python` | `node` | `bash` ( dicari di PATH ).
    #[serde(default = "d_interpreter")]
    pub interpreter: String,
    #[serde(default)]
    pub permissions: SkillPermission,
}

fn d_version() -> String {
    "1".into()
}
fn d_timeout() -> u64 {
    5
}
fn d_interpreter() -> String {
    "python".into()
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("skill.toml tidak valid: {source}")]
    Parse {
        #[source]
        source: toml::de::Error,
    },

    #[error("manifest skill: {0}")]
    Validation(String),

    #[error("tool '{0}' tidak diizinkan untuk skill ( SF8: tool financial tidak tersedia )")]
    ToolNotAllowed(String),
}

impl SkillManifest {
    /// Validasi manifest. Permission default deny-all ( SF8 ).
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.name.trim().is_empty() {
            return Err(ManifestError::Validation("name wajib diisi".into()));
        }
        // Nama skill hanya boleh alfanumerik + _ + - ( anti path traversal ).
        let valid_name = self
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid_name {
            return Err(ManifestError::Validation(
                "name hanya boleh alfanumerik / '_' / '-'".into(),
            ));
        }
        if self.script.trim().is_empty() {
            return Err(ManifestError::Validation("script wajib diisi".into()));
        }
        // Script tidak boleh keluar dari folder skill ( anti path traversal ).
        if self.script.contains("..") || self.script.contains('/') || self.script.contains('\\') {
            return Err(ManifestError::Validation(
                "script harus nama file di dalam folder skill ( tanpa path )".into(),
            ));
        }
        if self.timeout_secs == 0 {
            return Err(ManifestError::Validation("timeout_secs harus >= 1".into()));
        }
        if self.timeout_secs > 60 {
            return Err(ManifestError::Validation(
                "timeout_secs maksimum 60 detik ( SF8 batas waktu )".into(),
            ));
        }
        if !["python", "node", "bash"].contains(&self.interpreter.as_str()) {
            return Err(ManifestError::Validation(format!(
                "interpreter '{}' tidak didukung ( python | node | bash )",
                self.interpreter
            )));
        }
        // Validasi tool: harus subset AllowedTool ( tool financial ditolak ).
        for t in &self.permissions.allowed_tools {
            if AllowedTool::parse(t).is_none() {
                return Err(ManifestError::ToolNotAllowed(t.clone()));
            }
        }
        if self.permissions.max_calls_per_hour == 0 {
            return Err(ManifestError::Validation(
                "max_calls_per_hour harus >= 1".into(),
            ));
        }
        Ok(())
    }

    /// Apakah skill diizinkan memanggil tool ini?
    pub fn may_call(&self, tool: AllowedTool) -> bool {
        self.permissions
            .allowed_tools
            .iter()
            .any(|t| AllowedTool::parse(t) == Some(tool))
    }

    /// Apakah skill diizinkan mengakses domain ini?
    pub fn may_access_domain(&self, domain: &str) -> bool {
        self.permissions.allowed_domains.iter().any(|d| d == domain)
    }
}

impl SkillManifest {
    /// Parse `skill.toml` dari string.
    pub fn parse(input: &str) -> Result<SkillManifest, ManifestError> {
        let m: SkillManifest =
            toml::from_str(input).map_err(|e| ManifestError::Parse { source: e })?;
        m.validate()?;
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST_OK: &str = r#"
        name = "floor_sniper"
        description = "Pantau floor & usulkan buy di bawah ambang"
        version = "1"
        timeout_secs = 5
        script = "run.py"
        interpreter = "python"

        [permissions]
        allowed_tools = ["get_floor", "get_listings"]
        allowed_domains = ["api.opensea.io"]
        max_calls_per_hour = 30
        can_propose_intent = false
    "#;

    #[test]
    fn manifest_valid_diterima() {
        let m = SkillManifest::parse(MANIFEST_OK).unwrap();
        assert_eq!(m.name, "floor_sniper");
        assert_eq!(m.permissions.allowed_tools.len(), 2);
        assert!(m.may_call(AllowedTool::GetFloor));
        assert!(!m.may_call(AllowedTool::GasPrice));
        assert!(m.may_access_domain("api.opensea.io"));
        assert!(!m.may_access_domain("api.jahat.io"));
    }

    #[test]
    fn field_tidak_dikenal_ditolak() {
        let bad = MANIFEST_OK.replace("timeout_secs = 5", "timeout_secs = 5\ntypo = 1");
        assert!(SkillManifest::parse(&bad).is_err());
    }

    #[test]
    fn tool_finansial_ditolak() {
        let bad = MANIFEST_OK.replace(
            r#"allowed_tools = ["get_floor", "get_listings"]"#,
            r#"allowed_tools = ["get_floor", "buy"]"#,
        );
        let err = SkillManifest::parse(&bad).unwrap_err();
        assert!(err.to_string().contains("tidak diizinkan"));
    }

    #[test]
    fn tool_asing_ditolak() {
        let bad = MANIFEST_OK.replace(
            r#"allowed_tools = ["get_floor", "get_listings"]"#,
            r#"allowed_tools = ["get_floor", "tool_hack"]"#,
        );
        assert!(SkillManifest::parse(&bad).is_err());
    }

    #[test]
    fn nama_dengan_karakter_asing_ditolak() {
        let bad = MANIFEST_OK.replace("floor_sniper", "floor sniper!");
        assert!(SkillManifest::parse(&bad).is_err());
    }

    #[test]
    fn script_path_traversal_ditolak() {
        let bad = MANIFEST_OK.replace("run.py", "../../etc/passwd");
        assert!(SkillManifest::parse(&bad).is_err());
        let bad = MANIFEST_OK.replace("run.py", "sub/run.py");
        assert!(SkillManifest::parse(&bad).is_err());
    }

    #[test]
    fn timeout_nol_ditolak() {
        let bad = MANIFEST_OK.replace("timeout_secs = 5", "timeout_secs = 0");
        assert!(SkillManifest::parse(&bad).is_err());
    }

    #[test]
    fn timeout_di_atas_batas_ditolak() {
        let bad = MANIFEST_OK.replace("timeout_secs = 5", "timeout_secs = 120");
        assert!(SkillManifest::parse(&bad).is_err());
    }

    #[test]
    fn interpreter_tidak_dikenal_ditolak() {
        let bad = MANIFEST_OK.replace("python", "perl");
        assert!(SkillManifest::parse(&bad).is_err());
    }

    #[test]
    fn permission_default_deny_all() {
        // Tanpa [permissions], skill tidak boleh memanggil tool apapun.
        let raw = r#"
        name = "kosong"
        script = "run.py"
        "#;
        let m = SkillManifest::parse(raw).unwrap();
        for t in AllowedTool::all() {
            assert!(!m.may_call(*t));
        }
        assert!(!m.may_access_domain("api.opensea.io"));
        assert_eq!(m.permissions.max_calls_per_hour, 30); // default
        assert!(!m.permissions.can_propose_intent);
    }

    #[test]
    fn manifest_minimal_valid() {
        let raw = r#"
        name = "gas_optimizer"
        script = "run.py"
        "#;
        let m = SkillManifest::parse(raw).unwrap();
        assert_eq!(m.timeout_secs, 5);
        assert_eq!(m.interpreter, "python");
        assert_eq!(m.version, "1");
    }
}
