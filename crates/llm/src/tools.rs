//! Tool calling OpenAI-compatible (blueprint SF7, SF8).
//!
//! LLM dapat memanggil "tool" ( function calling ). Tool yang tersedia &
//! parameternya dideklarasikan di `ToolSpec`. **Eksekusi tool tidak pernah
//! dijalankan oleh LLM** -- `parse_tool_calls` hanya mengambil usulan
//! pemanggilan; eksekusi dilakukan oleh kode deterministik yang memvalidasi
//! permission skill ( `crates/skills` ) & Policy Engine ( aturan keras #2 ).

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Deklarasi tool ( OpenAI function calling schema ).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub r#type: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolSpec {
    pub fn function(name: &str, description: &str, parameters: serde_json::Value) -> Self {
        Self {
            r#type: "function".into(),
            function: ToolFunction {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

/// Usulan pemanggilan tool dari LLM ( **belum dieksekusi** ).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    /// Argumen JSON mentah dari LLM ( belum divalidasi ).
    pub arguments: serde_json::Value,
}

#[derive(Debug, Error)]
pub enum ToolCallError {
    #[error("respons bukan array JSON: {0}")]
    NotArray(String),

    #[error("entri tool call invalid: {0}")]
    InvalidEntry(String),

    #[error("-field 'name' kosong")]
    EmptyName,

    #[error("field 'arguments' bukan objek JSON")]
    ArgumentsNotObject,

    #[error("error parse JSON: {0}")]
    Parse(#[from] serde_json::Error),
}

/// Parse usulan tool call dari konten respons LLM.
///
/// LLM diminta mengembalikan array JSON `[{"name": ..., "arguments": {...}}]`.
/// Parser ini longgar ( menerima pembungkus ```json ... ``` & teks
/// tambahan ) tetapi ketat terhadap field wajib.
pub fn parse_tool_calls(content: &str) -> Result<Vec<ToolCall>, ToolCallError> {
    let trimmed = strip_code_fence(content.trim());
    // Cari array JSON pertama ( LLM kadang menambahkan teks ).
    let start = trimmed
        .find('[')
        .ok_or_else(|| ToolCallError::NotArray(trimmed.chars().take(80).collect()))?;
    let end = trimmed
        .rfind(']')
        .ok_or_else(|| ToolCallError::NotArray(trimmed.chars().take(80).collect()))?;
    let slice = &trimmed[start..=end];
    let arr: Vec<serde_json::Value> = serde_json::from_str(slice)?;

    let mut calls = Vec::with_capacity(arr.len());
    for (i, v) in arr.into_iter().enumerate() {
        let name = v
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| ToolCallError::InvalidEntry(format!("entri {i} tanpa 'name'")))?
            .trim()
            .to_string();
        if name.is_empty() {
            return Err(ToolCallError::EmptyName);
        }
        let arguments = v.get("arguments").cloned().unwrap_or(serde_json::json!({}));
        if !arguments.is_object() {
            return Err(ToolCallError::ArgumentsNotObject);
        }
        calls.push(ToolCall { name, arguments });
    }
    Ok(calls)
}

/// Lepaskan pembungkus ```json ... ``` bila ada.
fn strip_code_fence(s: &str) -> &str {
    if let Some(rest) = s.strip_prefix("```json") {
        rest.strip_suffix("```").unwrap_or(rest)
    } else if let Some(rest) = s.strip_prefix("```") {
        rest.strip_suffix("```").unwrap_or(rest)
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_array_sederhana() {
        let content = r#"[{"name":"get_floor","arguments":{"collection":"0xabc"}}]"#;
        let calls = parse_tool_calls(content).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_floor");
        assert_eq!(calls[0].arguments["collection"], "0xabc");
    }

    #[test]
    fn parse_dengan_teks_pengantar() {
        let content = "Berikut tool yang dipanggil:\n[{\"name\":\"a\",\"arguments\":{}}]\nSelesai";
        let calls = parse_tool_calls(content).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "a");
    }

    #[test]
    fn parse_code_fence() {
        let content = "```json\n[{\"name\":\"a\",\"arguments\":{}}]\n```";
        assert_eq!(parse_tool_calls(content).unwrap().len(), 1);
    }

    #[test]
    fn arguments_default_objek_kosong() {
        let content = r#"[{"name":"a"}]"#;
        let calls = parse_tool_calls(content).unwrap();
        assert_eq!(calls[0].arguments, serde_json::json!({}));
    }

    #[test]
    fn name_kosong_ditolak() {
        let content = r#"[{"name":"  ","arguments":{}}]"#;
        let err = parse_tool_calls(content).unwrap_err();
        assert!(matches!(err, ToolCallError::EmptyName));
    }

    #[test]
    fn arguments_bukan_objek_ditolak() {
        let content = r#"[{"name":"a","arguments":[1,2]}]"#;
        let err = parse_tool_calls(content).unwrap_err();
        assert!(matches!(err, ToolCallError::ArgumentsNotObject));
    }

    #[test]
    fn tanpa_array_ditolak() {
        let err = parse_tool_calls("hanya teks biasa").unwrap_err();
        assert!(matches!(err, ToolCallError::NotArray(_)));
    }

    #[test]
    fn json_invalid_ditolak() {
        // "[{bad" tidak memiliki ']' -> NotArray.
        let err = parse_tool_calls("[{bad").unwrap_err();
        assert!(matches!(err, ToolCallError::NotArray(_)));
        // Array dengan JSON rusak di dalam -> Parse.
        let err = parse_tool_calls("[{bad]").unwrap_err();
        assert!(matches!(err, ToolCallError::Parse(_)));
    }

    #[test]
    fn entri_tanpa_name_ditolak() {
        let err = parse_tool_calls(r#"[{"arguments":{}}]"#).unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidEntry(_)));
    }

    #[test]
    fn tool_spec_serialisasi() {
        let spec = ToolSpec::function(
            "get_floor",
            "Ambil floor price koleksi",
            serde_json::json!({"type":"object"}),
        );
        let j = serde_json::to_string(&spec).unwrap();
        assert!(j.contains("\"function\""));
        let back: ToolSpec = serde_json::from_str(&j).unwrap();
        assert_eq!(back.function.name, "get_floor");
    }
}
