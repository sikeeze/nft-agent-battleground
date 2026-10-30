//! `Intent` — usulan aksi dari LLM/skill/user/scheduler (blueprint §F5).
//!
//! Intent **belum berarti dijalankan**: setiap Intent wajib melewati
//! Policy Engine (`crates/policy`) dulu; hanya `PolicyDecision` yang lulus
//! yang bisa jadi `ApprovedTx` untuk signer.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Jenis aksi yang diusulkan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentKind {
    Mint,
    Buy,
    Sell,
    List,
    Bid,
    Cancel,
    Transfer,
    Sign,
}

/// Siapa yang mengajukan Intent. Originator LLM/skill selalu berbatas:
/// tidak boleh langsung menyentuh signer (blueprint §F5, §F8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Originator {
    User,
    Llm,
    Skill,
    Scheduler,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    pub id: Uuid,
    pub kind: IntentKind,
    pub chain_id: Option<u64>,
    /// Alamat kontrak target (hex string). Tipe `Address` dari alloy dipakai
    /// di crate chain/market agar tetap ringan di level core.
    pub contract: Option<String>,
    /// Parameter ekstensibel (JSON). Wajib bebas rahasia.
    pub params: serde_json::Value,
    pub originator: Originator,
    pub created_at: DateTime<Utc>,
}

impl Intent {
    pub fn new(kind: IntentKind, originator: Originator) -> Self {
        Self {
            id: Uuid::new_v4(),
            kind,
            chain_id: None,
            contract: None,
            params: serde_json::json!({}),
            originator,
            created_at: Utc::now(),
        }
    }

    pub fn with_chain(mut self, chain_id: u64) -> Self {
        self.chain_id = Some(chain_id);
        self
    }

    pub fn with_contract(mut self, contract: impl Into<String>) -> Self {
        self.contract = Some(contract.into());
        self
    }

    pub fn with_params(mut self, params: serde_json::Value) -> Self {
        self.params = params;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_mengisi_field() {
        let intent = Intent::new(IntentKind::Mint, Originator::User)
            .with_chain(8453)
            .with_contract("0xabc")
            .with_params(serde_json::json!({"qty": 1}));
        assert_eq!(intent.chain_id, Some(8453));
        assert_eq!(intent.contract.as_deref(), Some("0xabc"));
        assert_eq!(intent.params["qty"], 1);
        assert_eq!(intent.originator, Originator::User);
    }

    #[test]
    fn serde_roundtrip() {
        let intent = Intent::new(IntentKind::Buy, Originator::Llm).with_chain(1);
        let json = serde_json::to_string(&intent).unwrap();
        let back: Intent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, IntentKind::Buy);
        assert_eq!(back.originator, Originator::Llm);
        assert_eq!(back.chain_id, Some(1));
    }
}
