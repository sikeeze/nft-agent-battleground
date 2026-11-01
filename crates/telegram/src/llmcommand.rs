//! Perintah LLM & skill: `/model`, `/skills` (blueprint SF7, SF8, Fase 9).
//!
//! - `/model` S daftar model dari `models.toml` + model aktif.
//! - `/model <id>` S ganti model aktif tanpa ubah kode ( SF7 ).
//! - `/skills` S daftar skill terpasang ( dari folder `skills/` ).
//! - `/skills <id>` S jalankan skill sekali ( hasilnya laporan, bukan tx ).
//!
//! **Aturan**: skill tidak punya akses signer/keystore ( SF8 ); output skill
//! hanya laporan/observasi. Eksekusi tetap lewat Policy Engine.

use std::path::Path;

use nft_core::config::ModelsConfig;
use nft_llm::LlmClient;
use nft_skills::{run_skill, SkillInput};

/// Hasil perintah untuk chat.
#[derive(Debug, Clone)]
pub struct LlmOutcome {
    pub text: String,
}

/// `/model` & `/model <id>`.
pub async fn model_command(
    args: Option<&str>,
    models: &ModelsConfig,
    client: &LlmClient,
) -> LlmOutcome {
    let Some(id) = args.map(str::trim).filter(|s| !s.is_empty()) else {
        let active = client.model().await;
        let list: Vec<String> = models
            .model
            .iter()
            .map(|m| {
                let mark = if m.id == active.id { " (aktif)" } else { "" };
                format!("- {} {}{}", m.id, m.model_name, mark)
            })
            .collect();
        return LlmOutcome {
            text: format!(
                "Model LLM (models.toml):\n{}\n\n/model <id> - ganti model aktif",
                list.join("\n")
            ),
        };
    };
    let Some(entry) = models.model(id) else {
        return LlmOutcome {
            text: format!("[X] Model '{id}' tidak ada di models.toml."),
        };
    };
    match client.with_model(entry.clone()).await {
        Ok(()) => LlmOutcome {
            text: format!(
                "[OK] Model aktif diganti ke {} ({}).",
                entry.id, entry.model_name
            ),
        },
        Err(e) => LlmOutcome {
            text: format!("[X] Gagal ganti model: {e}"),
        },
    }
}

/// `/skills` & `/skills <id>`.
pub async fn skills_command(args: Option<&str>, skills_dir: &Path) -> LlmOutcome {
    let Some(id) = args.map(str::trim).filter(|s| !s.is_empty()) else {
        return list_skills(skills_dir).await;
    };
    let dir = skills_dir.join(id);
    if !dir.is_dir() {
        return LlmOutcome {
            text: format!(
                "[X] Skill '{id}' tidak ditemukan di {}.",
                skills_dir.display()
            ),
        };
    }
    let input = SkillInput::default();
    match run_skill(&dir, &input).await {
        Ok(out) => LlmOutcome { text: out.message },
        Err(e) => LlmOutcome {
            text: format!("[X] Skill '{id}' gagal: {e}"),
        },
    }
}

/// Daftar skill terpasang ( folder dengan `skill.toml` ).
async fn list_skills(skills_dir: &Path) -> LlmOutcome {
    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return LlmOutcome {
            text: format!(
                "[X] Folder skill tidak ditemukan ({}).",
                skills_dir.display()
            ),
        };
    };
    let mut names = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() && path.join("skill.toml").is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                names.push(name.to_string());
            }
        }
    }
    if names.is_empty() {
        return LlmOutcome {
            text: "Tidak ada skill terpasang ( folder skills/ kosong ).".into(),
        };
    }
    LlmOutcome {
        text: format!(
            "Skill terpasang ({}):\n{}\n\n/skills <id> - jalankan skill",
            names.len(),
            names
                .iter()
                .map(|n| format!("- {n}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nft_core::config::ModelEntry;
    use std::env;

    fn models() -> ModelsConfig {
        ModelsConfig {
            default: "test".into(),
            plan_timeout_secs: 3,
            model: vec![ModelEntry {
                id: "test".into(),
                base_url_env: "NFT_TG_LLM_URL".into(),
                api_key_env: "NFT_TG_LLM_KEY".into(),
                model_name: "gpt-test".into(),
                max_tokens: Some(128),
            }],
        }
    }

    fn set_env() {
        env::set_var("NFT_TG_LLM_URL", "https://api.example.com/v1");
        env::set_var("NFT_TG_LLM_KEY", "sk-test");
    }

    fn client() -> LlmClient {
        set_env();
        LlmClient::new(
            models().model[0].clone(),
            None,
            std::time::Duration::from_secs(3),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn model_daftar_dan_aktif() {
        let c = client();
        let out = model_command(None, &models(), &c).await;
        assert!(out.text.contains("test"));
        assert!(out.text.contains("gpt-test"));
        assert!(out.text.contains("(aktif)"));
    }

    #[tokio::test]
    async fn model_ganti_tanpa_ubah_kode() {
        let c = client();
        let out = model_command(Some("test"), &models(), &c).await;
        assert!(out.text.contains("[OK]"));
    }

    #[tokio::test]
    async fn model_tidak_ada_ditolak() {
        let c = client();
        let out = model_command(Some("tidak-ada"), &models(), &c).await;
        assert!(out.text.contains("[X]"));
        assert!(out.text.contains("tidak-ada"));
    }

    #[tokio::test]
    async fn skills_folder_kosong_jelas() {
        let dir = tempfile::tempdir().unwrap();
        let out = list_skills(dir.path()).await;
        assert!(out.text.contains("kosong"));
    }

    #[tokio::test]
    async fn skills_daftar_terpasang() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("contoh");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("skill.toml"),
            "name = \"contoh\"\nscript = \"run.js\"\n",
        )
        .unwrap();
        let out = list_skills(dir.path()).await;
        assert!(out.text.contains("contoh"));
        assert!(out.text.contains("1"));
    }

    #[tokio::test]
    async fn skill_tidak_ada_ditolak() {
        let dir = tempfile::tempdir().unwrap();
        let out = skills_command(Some("hantu"), dir.path()).await;
        assert!(out.text.contains("[X]"));
    }
}
