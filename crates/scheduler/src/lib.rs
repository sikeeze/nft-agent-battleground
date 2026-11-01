//! `nft-scheduler` -- scheduler job persisten (blueprint SF9, S9 Fase 10).
//!
//! Tiga jenis job:
//! - **One-shot** -- jalankan sekali pada waktu tertentu ( mis. mint pada
//!   tanggal/jam tertentu ).
//! - **Cron** -- berulang dengan jadwal cron sederhana ( menit/jam/hari ).
//! - **Event** -- dipicu event eksternal ( block baru, floor turun,
//!   listing baru ) lewat `trigger_event`.
//!
//! Job tersimpan di SQLite ( tabel `jobs` ) dan **tetap jalan setelah
//! restart**: saat startup, `Scheduler::new` memuat job belum selesai dan
//! menjadwalkan ulang yang sudah lewat ( run-once ) atau yang belum
//! dieksekusi (scheduled).
//!
//! **Aturan** ( SF9, S3 poin 6 ): mint war dan job terjadwal **tidak
//! boleh bergantung pada chat**. Scheduler ini berjalan independen dari
//! Telegram; eksekusi job dilakukan lewat trait `JobRunner` yang
//! diimplementasikan di crate agent.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// Jenis job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Jalankan sekali pada `next_run`.
    OneShot,
    /// Berulang sesuai jadwal cron ( menit/jam/hari ).
    Cron,
    /// Dipicu event eksternal ( `trigger_event` ).
    Event,
}

impl JobKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobKind::OneShot => "oneshot",
            JobKind::Cron => "cron",
            JobKind::Event => "event",
        }
    }

    pub fn parse(s: &str) -> Option<JobKind> {
        match s {
            "oneshot" => Some(JobKind::OneShot),
            "cron" => Some(JobKind::Cron),
            "event" => Some(JobKind::Event),
            _ => None,
        }
    }
}

/// Status job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Menunggu jadwal / event.
    Pending,
    /// Sedang berjalan.
    Running,
    /// Selesai ( one-shot ) atau sukses sekali jalan.
    Done,
    /// Gagal ( akan dicoba ulang bila `retries` masih ada ).
    Failed,
    /// Dibatalkan user.
    Cancelled,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::Pending => "pending",
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<JobStatus> {
        match s {
            "pending" => Some(JobStatus::Pending),
            "running" => Some(JobStatus::Running),
            "done" => Some(JobStatus::Done),
            "failed" => Some(JobStatus::Failed),
            "cancelled" => Some(JobStatus::Cancelled),
            _ => None,
        }
    }
}

/// Job terjadwal ( blueprint SF9 ).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub kind: JobKind,
    /// Payload JSON untuk runner ( bebas rahasia ).
    pub payload: serde_json::Value,
    /// Jadwal cron ( untuk `JobKind::Cron` ): "every:<N><m|h|d>".
    pub cron: Option<String>,
    /// Waktu jalang berikutnya ( unix detik ).
    pub next_run: i64,
    /// Jumlah retry tersisa.
    pub retries: u32,
    pub status: JobStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("error SQLite: {0}")]
    Db(#[from] sqlx::Error),

    #[error("error migrasi: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("job tidak valid: {0}")]
    Validation(String),

    #[error("job '{0}' tidak ditemukan")]
    NotFound(String),

    #[error("jenis job tidak dikenal: {0}")]
    UnknownKind(String),

    #[error("status job tidak dikenal: {0}")]
    UnknownStatus(String),
}

/// Runner job -- diimplementasikan di crate agent ( menghubungkan job ke
/// eksekusi mint/trade ). Runner tidak boleh bergantung pada Telegram
/// ( S3 poin 6 ).
#[async_trait::async_trait]
pub trait JobRunner: Send + Sync {
    /// Jalankan satu job. Mengembalikan `Ok(())` bila sukses.
    async fn run(&self, job: &Job) -> Result<(), String>;
}

/// Scheduler persisten.
pub struct Scheduler {
    db: sqlx::SqlitePool,
    /// Runner dipanggil saat job dijalankan.
    runner: Arc<dyn JobRunner>,
    /// Job event-triggered yang sedang menunggu ( namanya ). Nilai = channel
    /// pemicu yang dipanggil `trigger_event`.
    events: Arc<Mutex<Vec<String>>>,
    /// Handle task loop scheduler ( bisa dihentikan dengan `shutdown` ).
    loop_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Scheduler {
    /// Buat scheduler + migrasi tabel `jobs`. **Memuat job belum selesai
    /// dan menjadwalkan ulang** ( blueprint SF9: "Job tersimpan di SQLite
    /// dan tetap jalan setelah restart" ).
    pub async fn new(
        db: sqlx::SqlitePool,
        runner: Arc<dyn JobRunner>,
    ) -> Result<Self, SchedulerError> {
        Self::migrate(&db).await?;
        Ok(Self {
            db,
            runner,
            events: Arc::new(Mutex::new(Vec::new())),
            loop_handle: Mutex::new(None),
        })
    }

    /// Migrasi tabel `jobs` ( idempoten ).
    async fn migrate(db: &sqlx::SqlitePool) -> Result<(), SchedulerError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS jobs (\
             id TEXT PRIMARY KEY,\
             name TEXT NOT NULL,\
             kind TEXT NOT NULL,\
             payload TEXT NOT NULL,\
             cron TEXT,\
             next_run INTEGER NOT NULL,\
             retries INTEGER NOT NULL DEFAULT 0,\
             status TEXT NOT NULL DEFAULT 'pending',\
             created_at INTEGER NOT NULL,\
             updated_at INTEGER NOT NULL\
             )",
        )
        .execute(db)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_jobs_status ON jobs (status)")
            .execute(db)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_jobs_next_run ON jobs (next_run)")
            .execute(db)
            .await?;
        Ok(())
    }

    /// Tambah job one-shot: jalankan sekali pada `run_at`.
    pub async fn add_oneshot(
        &self,
        name: &str,
        payload: serde_json::Value,
        run_at: DateTime<Utc>,
    ) -> Result<String, SchedulerError> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().timestamp();
        self.validate_payload(&payload)?;
        sqlx::query(
            "INSERT INTO jobs (id, name, kind, payload, cron, next_run, retries, status, created_at, updated_at) \
             VALUES (?, ?, 'oneshot', ?, NULL, ?, 0, 'pending', ?, ?)",
        )
        .bind(&id)
        .bind(name)
        .bind(payload.to_string())
        .bind(run_at.timestamp())
        .bind(now)
        .bind(now)
        .execute(&self.db)
        .await?;
        debug!(%id, name, "job one-shot ditambahkan");
        Ok(id)
    }

    /// Tambah job cron: berulang. `cron_expr` = "every:<N><m|h|d>"
    /// ( mis. "every:5m", "every:1h", "every:2d" ).
    pub async fn add_cron(
        &self,
        name: &str,
        payload: serde_json::Value,
        cron_expr: &str,
    ) -> Result<String, SchedulerError> {
        let interval = parse_cron(cron_expr).ok_or_else(|| {
            SchedulerError::Validation(format!(
                "cron '{cron_expr}' tidak valid ( format: every:<N><m|h|d> )"
            ))
        })?;
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().timestamp();
        self.validate_payload(&payload)?;
        sqlx::query(
            "INSERT INTO jobs (id, name, kind, payload, cron, next_run, retries, status, created_at, updated_at) \
             VALUES (?, ?, 'cron', ?, ?, ?, 0, 'pending', ?, ?)",
        )
        .bind(&id)
        .bind(name)
        .bind(payload.to_string())
        .bind(cron_expr)
        .bind(now + interval)
        .bind(now)
        .bind(now)
        .execute(&self.db)
        .await?;
        debug!(%id, name, cron = cron_expr, "job cron ditambahkan");
        Ok(id)
    }

    /// Tambah job event-triggered: menunggu sampai `trigger_event`
    /// dipanggil dengan nama yang cocok.
    pub async fn add_event(
        &self,
        name: &str,
        payload: serde_json::Value,
    ) -> Result<String, SchedulerError> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().timestamp();
        self.validate_payload(&payload)?;
        sqlx::query(
            "INSERT INTO jobs (id, name, kind, payload, cron, next_run, retries, status, created_at, updated_at) \
             VALUES (?, ?, 'event', ?, NULL, ?, 0, 'pending', ?, ?)",
        )
        .bind(&id)
        .bind(name)
        .bind(payload.to_string())
        .bind(now) // not used for event; next_run = createdAt
        .bind(now)
        .bind(now)
        .execute(&self.db)
        .await?;
        self.events.lock().await.push(name.to_string());
        debug!(%id, name, "job event ditambahkan");
        Ok(id)
    }

    /// Picu job event berdasarkan nama ( block baru, floor turun,
    /// listing baru ). Semua job event dengan nama ini dijalankan.
    pub async fn trigger_event(&self, event_name: &str) -> Result<u32, SchedulerError> {
        let rows = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT id, name, kind, payload FROM jobs WHERE kind = 'event' AND status = 'pending'",
        )
        .fetch_all(&self.db)
        .await?;
        let mut run = 0u32;
        for (id, name, _kind, payload) in rows {
            if name != event_name {
                continue;
            }
            run += 1;
            self.run_job_by_id(&id, &payload).await;
        }
        if run == 0 {
            debug!(event = event_name, "tidak ada job event cocok");
        }
        Ok(run)
    }

    /// Batalkan job.
    pub async fn cancel(&self, id: &str) -> Result<(), SchedulerError> {
        let res = sqlx::query("UPDATE jobs SET status = 'cancelled', updated_at = ? WHERE id = ? AND status = 'pending'")
            .bind(Utc::now().timestamp())
            .bind(id)
            .execute(&self.db)
            .await?;
        if res.rows_affected() == 0 {
            return Err(SchedulerError::NotFound(id.into()));
        }
        Ok(())
    }

    /// Daftar job ( untuk API / `/status` ).
    pub async fn list(&self) -> Result<Vec<Job>, SchedulerError> {
        let rows = sqlx::query_as::<_, (String, String, String, String, Option<String>, i64, i32, String, i64, i64)>(
            "SELECT id, name, kind, payload, cron, next_run, retries, status, created_at, updated_at FROM jobs ORDER BY created_at DESC",
        )
        .fetch_all(&self.db)
        .await?;
        let mut jobs = Vec::with_capacity(rows.len());
        for (id, name, kind, payload, cron, next_run, retries, status, created_at, updated_at) in
            rows
        {
            let k =
                JobKind::parse(&kind).ok_or_else(|| SchedulerError::UnknownKind(kind.clone()))?;
            let s = JobStatus::parse(&status)
                .ok_or_else(|| SchedulerError::UnknownStatus(status.clone()))?;
            jobs.push(Job {
                id,
                name,
                kind: k,
                payload: serde_json::from_str(&payload).unwrap_or_default(),
                cron,
                next_run,
                retries: retries as u32,
                status: s,
                created_at,
                updated_at,
            });
        }
        Ok(jobs)
    }

    /// Mulai loop scheduler ( background ). Memeriksa job tiap detik.
    /// Job yang sudah lewat & berstatus pending langsung dijalankan (
    /// mencegah job tertinggal saat agent mati / restart ).
    pub async fn start(&self, tick_secs: u64) -> Result<(), SchedulerError> {
        let db = self.db.clone();
        let runner = self.runner.clone();
        let mut handle_guard = self.loop_handle.lock().await;
        if handle_guard.is_some() {
            return Ok(()); // sudah jalan
        }
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(tick_secs.max(1))).await;
                if let Err(e) = Self::tick(&db, &runner).await {
                    warn!(?e, "scheduler tick gagal");
                }
            }
        });
        *handle_guard = Some(handle);
        Ok(())
    }

    /// Hentikan loop scheduler.
    pub async fn shutdown(&self) {
        let mut handle_guard = self.loop_handle.lock().await;
        if let Some(h) = handle_guard.take() {
            h.abort();
        }
    }

    /// Satu iterasi: ambil job yang waktunya tiba, jalankan.
    async fn tick(
        db: &sqlx::SqlitePool,
        runner: &Arc<dyn JobRunner>,
    ) -> Result<(), SchedulerError> {
        let now = Utc::now().timestamp();
        let rows = sqlx::query_as::<_, (String, String, String, Option<String>)>(
            "SELECT id, name, kind, cron FROM jobs WHERE status = 'pending' AND kind != 'event' AND next_run <= ?",
        )
        .bind(now)
        .fetch_all(db)
        .await?;
        for (id, _name, kind, _cron) in rows {
            // Ambil payload untuk eksekusi.
            let row = sqlx::query_as::<_, (String,)>("SELECT payload FROM jobs WHERE id = ?")
                .bind(&id)
                .fetch_optional(db)
                .await?;
            let Some((payload,)) = row else { continue };
            let job_kind =
                JobKind::parse(&kind).ok_or_else(|| SchedulerError::UnknownKind(kind.clone()))?;
            Self::run_one(db, runner, &id, &payload, job_kind).await?;
        }
        Ok(())
    }

    /// Jalankan satu job ( helper `trigger_event` & `tick` ).
    async fn run_job_by_id(&self, id: &str, payload: &str) {
        let row = sqlx::query_as::<_, (String,)>("SELECT kind FROM jobs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.db)
            .await
            .ok()
            .flatten();
        let Some((kind,)) = row else { return };
        let Some(k) = JobKind::parse(&kind) else {
            return;
        };
        if let Err(e) = Self::run_one(&self.db, &self.runner, id, payload, k).await {
            warn!(?e, "job gagal");
        }
    }

    /// Jalankan job: tandai running -> eksekusi runner -> done/failed,
    /// lalu hitung next_run untuk cron.
    async fn run_one(
        db: &sqlx::SqlitePool,
        runner: &Arc<dyn JobRunner>,
        id: &str,
        payload: &str,
        kind: JobKind,
    ) -> Result<(), SchedulerError> {
        let now = Utc::now().timestamp();
        sqlx::query("UPDATE jobs SET status = 'running', updated_at = ? WHERE id = ?")
            .bind(now)
            .bind(id)
            .execute(db)
            .await?;

        let job = Job {
            id: id.to_string(),
            name: String::new(),
            kind,
            payload: serde_json::from_str(payload).unwrap_or_default(),
            cron: None,
            next_run: now,
            retries: 0,
            status: JobStatus::Running,
            created_at: now,
            updated_at: now,
        };
        let outcome = runner.run(&job).await;

        match outcome {
            Ok(()) => {
                // Cron: jadwalkan ulang. One-shot/Event: selesai.
                if matches!(kind, JobKind::Cron) {
                    let row = sqlx::query_as::<_, (Option<String>,)>(
                        "SELECT cron FROM jobs WHERE id = ?",
                    )
                    .bind(id)
                    .fetch_optional(db)
                    .await?;
                    if let Some((Some(cron_expr),)) = row {
                        if let Some(interval) = parse_cron(&cron_expr) {
                            sqlx::query(
                                "UPDATE jobs SET status = 'pending', next_run = ?, updated_at = ? WHERE id = ?",
                            )
                            .bind(now + interval)
                            .bind(now)
                            .bind(id)
                            .execute(db)
                            .await?;
                            return Ok(());
                        }
                    }
                }
                sqlx::query("UPDATE jobs SET status = 'done', updated_at = ? WHERE id = ?")
                    .bind(now)
                    .bind(id)
                    .execute(db)
                    .await?;
            }
            Err(msg) => {
                warn!(%id, msg, "job gagal");
                sqlx::query("UPDATE jobs SET status = 'failed', updated_at = ? WHERE id = ?")
                    .bind(now)
                    .bind(id)
                    .execute(db)
                    .await?;
            }
        }
        Ok(())
    }

    /// Validasi payload job: harus objek JSON ( bebas rahasia ).
    fn validate_payload(&self, payload: &serde_json::Value) -> Result<(), SchedulerError> {
        if !payload.is_object() {
            return Err(SchedulerError::Validation(
                "payload job harus objek JSON".into(),
            ));
        }
        Ok(())
    }
}

/// Parse ekspresi cron sederhana: "every:<N><m|h|d>" -> detik.
pub fn parse_cron(expr: &str) -> Option<i64> {
    let rest = expr.strip_prefix("every:")?;
    let p = rest.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = (&rest[..p], &rest[p..]);
    let n: i64 = num.parse().ok()?;
    if n <= 0 {
        return None;
    }
    let mult = match unit {
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return None,
    };
    Some(n * mult)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// Recorder runner: catat job yang dijalankan ( bisa dipaksa gagal ).
    struct RecorderRunner {
        runs: Arc<Mutex<Vec<String>>>,
        fail: bool,
    }

    #[async_trait]
    impl JobRunner for RecorderRunner {
        async fn run(&self, job: &Job) -> Result<(), String> {
            self.runs.lock().await.push(job.id.clone());
            if self.fail {
                Err("dipaksa gagal".into())
            } else {
                Ok(())
            }
        }
    }

    async fn setup() -> (Scheduler, Arc<Mutex<Vec<String>>>) {
        let db = nft_core::storage::connect("sqlite::memory:").await.unwrap();
        let runs = Arc::new(Mutex::new(Vec::new()));
        let runner = Arc::new(RecorderRunner {
            runs: runs.clone(),
            fail: false,
        });
        let sched = Scheduler::new(db, runner).await.unwrap();
        (sched, runs)
    }

    fn future_time() -> DateTime<Utc> {
        Utc::now() + chrono::Duration::seconds(60)
    }

    #[test]
    fn parse_cron_valid() {
        assert_eq!(parse_cron("every:5m"), Some(300));
        assert_eq!(parse_cron("every:1h"), Some(3600));
        assert_eq!(parse_cron("every:2d"), Some(172_800));
        assert_eq!(parse_cron("every:0m"), None);
        assert_eq!(parse_cron("every:x"), None);
        assert_eq!(parse_cron("5m"), None);
    }

    #[test]
    fn job_kind_status_roundtrip() {
        for k in [JobKind::OneShot, JobKind::Cron, JobKind::Event] {
            assert_eq!(JobKind::parse(k.as_str()), Some(k));
        }
        for s in [
            JobStatus::Pending,
            JobStatus::Running,
            JobStatus::Done,
            JobStatus::Failed,
            JobStatus::Cancelled,
        ] {
            assert_eq!(JobStatus::parse(s.as_str()), Some(s));
        }
        assert!(JobKind::parse("bogus").is_none());
        assert!(JobStatus::parse("bogus").is_none());
    }

    #[tokio::test]
    async fn tambah_dan_list_job_oneshot() {
        let (sched, _runs) = setup().await;
        let id = sched
            .add_oneshot(
                "mint-drop",
                serde_json::json!({"link": "https://x"}),
                future_time(),
            )
            .await
            .unwrap();
        let jobs = sched.list().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, id);
        assert_eq!(jobs[0].name, "mint-drop");
        assert_eq!(jobs[0].kind, JobKind::OneShot);
        assert_eq!(jobs[0].status, JobStatus::Pending);
    }

    #[tokio::test]
    async fn tambah_dan_list_job_cron() {
        let (sched, _runs) = setup().await;
        sched
            .add_cron("cek-floor", serde_json::json!({"kol": "0x1"}), "every:5m")
            .await
            .unwrap();
        let jobs = sched.list().await.unwrap();
        assert_eq!(jobs[0].kind, JobKind::Cron);
        assert_eq!(jobs[0].cron.as_deref(), Some("every:5m"));
    }

    #[tokio::test]
    async fn tambah_job_event_dan_picu() {
        let (sched, runs) = setup().await;
        sched
            .add_event("floor-turun", serde_json::json!({"pct": 10}))
            .await
            .unwrap();
        // Picu event yang tidak ada -> 0 job jalan.
        assert_eq!(sched.trigger_event("lain").await.unwrap(), 0);
        // Picu event yang cocok -> 1 job jalan.
        assert_eq!(sched.trigger_event("floor-turun").await.unwrap(), 1);
        let r = runs.lock().await;
        assert_eq!(r.len(), 1);
        // Status jadi done.
        let jobs = sched.list().await.unwrap();
        assert_eq!(jobs[0].status, JobStatus::Done);
    }

    #[tokio::test]
    async fn cron_invalid_ditolak() {
        let (sched, _runs) = setup().await;
        let err = sched
            .add_cron("x", serde_json::json!({}), "bogus")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("tidak valid"));
    }

    #[tokio::test]
    async fn payload_bukan_objek_ditolak() {
        let (sched, _runs) = setup().await;
        let err = sched
            .add_oneshot("x", serde_json::json!([1, 2]), future_time())
            .await
            .unwrap_err();
        assert!(matches!(err, SchedulerError::Validation(_)));
    }

    #[tokio::test]
    async fn cancel_job() {
        let (sched, _runs) = setup().await;
        let id = sched
            .add_oneshot("x", serde_json::json!({}), future_time())
            .await
            .unwrap();
        sched.cancel(&id).await.unwrap();
        let jobs = sched.list().await.unwrap();
        assert_eq!(jobs[0].status, JobStatus::Cancelled);
        // Cancel kedua kali -> NotFound ( sudah bukan pending ).
        assert!(sched.cancel(&id).await.is_err());
    }

    #[tokio::test]
    async fn cancel_job_hilang_not_found() {
        let (sched, _runs) = setup().await;
        let err = sched.cancel("tidak-ada").await.unwrap_err();
        assert!(matches!(err, SchedulerError::NotFound(_)));
    }

    #[tokio::test]
    async fn job_lewatlangsung_dijalankan_saat_restart() {
        // Blueprint SF9: "Job tersimpan di SQLite dan tetap jalan setelah
        // restart". Job one-shot yang sudah lewat harus jalan saat startup.
        let db = nft_core::storage::connect("sqlite::memory:").await.unwrap();
        let runs = Arc::new(Mutex::new(Vec::new()));
        {
            let runner = Arc::new(RecorderRunner {
                runs: runs.clone(),
                fail: false,
            });
            let sched = Scheduler::new(db.clone(), runner).await.unwrap();
            // Job dengan next_run di masa lalu ( mis. agent mati 60 dtk lalu ).
            sched
                .add_oneshot(
                    "terlambat",
                    serde_json::json!({"x": 1}),
                    Utc::now() - chrono::Duration::seconds(60),
                )
                .await
                .unwrap();
        }
        // "Restart": scheduler baru dengan db yang sama.
        {
            let runner = Arc::new(RecorderRunner {
                runs: runs.clone(),
                fail: false,
            });
            let sched = Scheduler::new(db.clone(), runner).await.unwrap();
            Scheduler::tick(&db, &sched.runner).await.unwrap();
        }
        let r = runs.lock().await;
        assert_eq!(r.len(), 1, "job lewat harus dijalankan saat restart");
    }

    #[tokio::test]
    async fn cron_terjadwal_ulang_setelah_sukses() {
        let (sched, runs) = setup().await;
        let id = sched
            .add_cron("ulang", serde_json::json!({}), "every:5m")
            .await
            .unwrap();
        // next_run masih masa depan -> tick tidak menjalankan.
        Scheduler::tick(&sched.db, &sched.runner).await.unwrap();
        assert!(runs.lock().await.is_empty());
        // Paksa next_run ke masa lalu -> tick menjalankan & menjadwal ulang.
        sqlx::query("UPDATE jobs SET next_run = ? WHERE id = ?")
            .bind(Utc::now().timestamp() - 10)
            .bind(&id)
            .execute(&sched.db)
            .await
            .unwrap();
        Scheduler::tick(&sched.db, &sched.runner).await.unwrap();
        assert_eq!(runs.lock().await.len(), 1);
        let jobs = sched.list().await.unwrap();
        assert_eq!(jobs[0].status, JobStatus::Pending); // dijadwal ulang
        assert!(jobs[0].next_run > Utc::now().timestamp());
    }

    #[tokio::test]
    async fn job_gagal_ditandai_failed() {
        let db = nft_core::storage::connect("sqlite::memory:").await.unwrap();
        let runs = Arc::new(Mutex::new(Vec::new()));
        let runner = Arc::new(RecorderRunner {
            runs: runs.clone(),
            fail: true,
        });
        let sched = Scheduler::new(db, runner).await.unwrap();
        let id = sched
            .add_oneshot(
                "gagal",
                serde_json::json!({}),
                Utc::now() - chrono::Duration::seconds(60),
            )
            .await
            .unwrap();
        Scheduler::tick(&sched.db, &sched.runner).await.unwrap();
        let jobs = sched.list().await.unwrap();
        let job = jobs.iter().find(|j| j.id == id).unwrap();
        assert_eq!(job.status, JobStatus::Failed);
    }

    #[tokio::test]
    async fn event_tidak_dijalankan_oleh_tick() {
        // Job event hanya jalan via trigger_event, bukan tick.
        let (sched, runs) = setup().await;
        sched
            .add_event("floor-turun", serde_json::json!({}))
            .await
            .unwrap();
        Scheduler::tick(&sched.db, &sched.runner).await.unwrap();
        assert!(runs.lock().await.is_empty());
    }

    #[tokio::test]
    async fn start_shutdown_loop() {
        let (sched, _runs) = setup().await;
        sched.start(1).await.unwrap();
        // Start kedua tidak error ( idempoten ).
        sched.start(1).await.unwrap();
        sched.shutdown().await;
    }
}
