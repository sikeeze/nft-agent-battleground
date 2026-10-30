//! `Task` — unit pekerjaan terjadwal/persisten (blueprint §F9, §6.2).
//!
//! Job tersimpan di SQLite sehingga tetap jalan setelah restart; idempotency
//! key mencegah dobel eksekusi saat proses di-resume di tengah jalan.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Mint,
    /// War mint: multi-wallet dengan trigger ketat (blueprint §7).
    War,
    AutoBuy,
    AutoList,
    Monitor,
    Preflight,
    Custom(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Ready,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    /// Kunci idempotensi: (task, wallet, nonce) dsb. Restart tidak boleh
    /// menghasilkan eksekusi ganda (blueprint §6.2).
    pub idempotency_key: String,
    pub kind: TaskKind,
    /// Payload ekstensibel (JSON), bebas rahasia.
    pub payload: serde_json::Value,
    pub status: TaskStatus,
    /// Jadwal eksekusi; None untuk job event-triggered (blueprint §F9).
    pub scheduled_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Task {
    pub fn new(
        kind: TaskKind,
        idempotency_key: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            idempotency_key: idempotency_key.into(),
            kind,
            payload,
            status: TaskStatus::Pending,
            scheduled_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn with_schedule(mut self, at: DateTime<Utc>) -> Self {
        self.scheduled_at = Some(at);
        self
    }

    /// Set status; memperbarui `updated_at`.
    pub fn set_status(&mut self, status: TaskStatus) {
        self.status = status;
        self.updated_at = Utc::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_baru_pending_dengan_timestamp() {
        let t = Task::new(
            TaskKind::Mint,
            "mint:0xabc:3",
            serde_json::json!({"qty": 3}),
        );
        assert_eq!(t.status, TaskStatus::Pending);
        assert!(!t.status.is_terminal());
        assert_eq!(t.idempotency_key, "mint:0xabc:3");
        assert_eq!(t.payload["qty"], 3);
    }

    #[test]
    fn set_status_memperbarui_updated_at() {
        let mut t = Task::new(TaskKind::War, "war-1", serde_json::json!({}));
        let before = t.updated_at;
        std::thread::sleep(std::time::Duration::from_millis(5));
        t.set_status(TaskStatus::Ready);
        assert!(t.updated_at >= before);
        assert_eq!(t.status, TaskStatus::Ready);
    }

    #[test]
    fn terminal_status() {
        assert!(TaskStatus::Done.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
        assert!(!TaskStatus::Running.is_terminal());
    }
}
