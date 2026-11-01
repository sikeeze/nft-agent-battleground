//! Perintah scheduler: `/jobs` (blueprint SF9, Fase 10).
//!
//! - `/jobs` --- daftar job scheduler.
//! - `/jobs oneshot <nama> <unix-detik>` --- tambah job one-shot.
//! - `/jobs cron <nama> <every:NM|m|h|d>` --- tambah job cron.
//! - `/jobs cancel <id>` --- batalkan job.
//!
//! **Aturan** ( S3 poin 6 ): job terjadwal tidak bergantung pada chat ---
//! scheduler berjalan independen dari Telegram.

use nft_scheduler::{JobKind, Scheduler, SchedulerError};

/// Hasil perintah untuk chat.
#[derive(Debug, Clone)]
pub struct JobsOutcome {
    pub text: String,
}

/// `/jobs` & subperintah.
pub async fn jobs_command(args: Option<&str>, sched: Option<&Scheduler>) -> JobsOutcome {
    let Some(sched) = sched else {
        return JobsOutcome {
            text: "[X] Scheduler belum dikonfigurasi.".into(),
        };
    };
    let args = args.unwrap_or("").trim();
    let mut parts = args.split_whitespace();
    let sub = parts.next().unwrap_or("");
    match sub {
        "" => list_jobs(sched).await,
        "oneshot" => {
            let Some(name) = parts.next() else {
                return usage();
            };
            let Some(when) = parts.next() else {
                return usage();
            };
            let Ok(ts) = when.parse::<i64>() else {
                return JobsOutcome {
                    text: format!("[X] Waktu tidak valid: {when} ( unix detik )"),
                };
            };
            let run_at = match chrono::DateTime::from_timestamp(ts, 0) {
                Some(t) => t,
                None => {
                    return JobsOutcome {
                        text: format!("[X] Waktu di luar jangkauan: {ts}"),
                    }
                }
            };
            match sched
                .add_oneshot(name, serde_json::json!({ "source": "telegram" }), run_at)
                .await
            {
                Ok(id) => JobsOutcome {
                    text: format!("[OK] Job one-shot '{name}' dijadwalkan ( id {id} )."),
                },
                Err(e) => JobsOutcome {
                    text: format!("[X] Gagal menambah job: {e}"),
                },
            }
        }
        "cron" => {
            let Some(name) = parts.next() else {
                return usage();
            };
            let Some(expr) = parts.next() else {
                return usage();
            };
            match sched
                .add_cron(name, serde_json::json!({ "source": "telegram" }), expr)
                .await
            {
                Ok(id) => JobsOutcome {
                    text: format!("[OK] Job cron '{name}' dijadwalkan ( id {id} )."),
                },
                Err(e) => JobsOutcome {
                    text: format!("[X] Gagal menambah job: {e}"),
                },
            }
        }
        "cancel" => {
            let Some(id) = parts.next() else {
                return usage();
            };
            match sched.cancel(id).await {
                Ok(()) => JobsOutcome {
                    text: format!("[OK] Job {id} dibatalkan."),
                },
                Err(SchedulerError::NotFound(_)) => JobsOutcome {
                    text: format!("[X] Job {id} tidak ditemukan / sudah tidak pending."),
                },
                Err(e) => JobsOutcome {
                    text: format!("[X] Gagal membatalkan: {e}"),
                },
            }
        }
        _ => usage(),
    }
}

fn usage() -> JobsOutcome {
    JobsOutcome {
        text: "Pakai: /jobs [oneshot <nama> <unix-detik> | cron <nama> every:5m | cancel <id>]"
            .into(),
    }
}

async fn list_jobs(sched: &Scheduler) -> JobsOutcome {
    match sched.list().await {
        Ok(jobs) => {
            if jobs.is_empty() {
                return JobsOutcome {
                    text: "Tidak ada job scheduler.".into(),
                };
            }
            let lines: Vec<String> = jobs
                .iter()
                .map(|j| {
                    let kind = match j.kind {
                        JobKind::OneShot => "oneshot",
                        JobKind::Cron => "cron",
                        JobKind::Event => "event",
                    };
                    format!("- {} [{}] {} {}", j.name, kind, j.status.as_str(), j.id)
                })
                .collect();
            JobsOutcome {
                text: format!("Job scheduler ({}):\n{}", jobs.len(), lines.join("\n")),
            }
        }
        Err(e) => JobsOutcome {
            text: format!("[X] Gagal membaca job: {e}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use nft_scheduler::JobRunner;

    struct NoopRunner;

    #[async_trait]
    impl JobRunner for NoopRunner {
        async fn run(&self, _job: &nft_scheduler::Job) -> Result<(), String> {
            Ok(())
        }
    }
    async fn sched() -> Scheduler {
        let db = nft_core::storage::connect("sqlite::memory:").await.unwrap();
        Scheduler::new(db, std::sync::Arc::new(NoopRunner))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn daftar_kosong() {
        let s = sched().await;
        let out = jobs_command(None, Some(&s)).await;
        assert!(out.text.contains("Tidak ada job"));
    }

    #[tokio::test]
    async fn tambah_oneshot() {
        let s = sched().await;
        let ts = chrono::Utc::now().timestamp() + 3600;
        let out = jobs_command(Some(&format!("oneshot mint-drop {ts}")), Some(&s)).await;
        assert!(out.text.contains("[OK]"));
        let list = jobs_command(None, Some(&s)).await;
        assert!(list.text.contains("mint-drop"));
        assert!(list.text.contains("oneshot"));
    }

    #[tokio::test]
    async fn tambah_cron() {
        let s = sched().await;
        let out = jobs_command(Some("cron cek-floor every:5m"), Some(&s)).await;
        assert!(out.text.contains("[OK]"));
        let list = jobs_command(None, Some(&s)).await;
        assert!(list.text.contains("cek-floor"));
    }

    #[tokio::test]
    async fn cron_invalid_ditolak() {
        let s = sched().await;
        let out = jobs_command(Some("cron x bogus"), Some(&s)).await;
        assert!(out.text.contains("[X]"));
    }

    #[tokio::test]
    async fn cancel_job() {
        let s = sched().await;
        let ts = chrono::Utc::now().timestamp() + 3600;
        let out = jobs_command(Some(&format!("oneshot mint-drop {ts}")), Some(&s)).await;
        let id = out.text.split("id ").nth(1).unwrap().trim_end_matches('.');
        let out = jobs_command(Some(&format!("cancel {id}")), Some(&s)).await;
        assert!(out.text.contains("[OK]"));
    }

    #[tokio::test]
    async fn cancel_tidak_ada_ditolak() {
        let s = sched().await;
        let out = jobs_command(Some("cancel tidaksada"), Some(&s)).await;
        assert!(out.text.contains("[X]"));
    }

    #[tokio::test]
    async fn subperintah_tidak_dikenal_usage() {
        let s = sched().await;
        let out = jobs_command(Some("bogus"), Some(&s)).await;
        assert!(out.text.contains("Pakai:"));
    }

    #[tokio::test]
    async fn scheduler_none_dtolak_jelas() {
        let out = jobs_command(None, None).await;
        assert!(out.text.contains("[X]"));
    }

    #[tokio::test]
    async fn oneshot_waktu_invalid_ditolak() {
        let s = sched().await;
        let out = jobs_command(Some("oneshot x bukan-angka"), Some(&s)).await;
        assert!(out.text.contains("tidak valid"));
    }

    #[tokio::test]
    async fn oneshot_tanpa_waktu_usage() {
        let s = sched().await;
        let out = jobs_command(Some("oneshot x"), Some(&s)).await;
        assert!(out.text.contains("Pakai:"));
    }
}
