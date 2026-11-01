//! `nft-skills` -- loader & sandbox skill (blueprint SF8, S9 Fase 9).
//!
//! Skill = folder plugin ( `skill.toml` + script ) yang bisa ditambah tanpa
//! compile ulang. Sandbox dengan **permission eksplisit** ( domain jaringan,
//! tool yang boleh dipanggil, batas waktu ). Skill **tidak punya akses
//! signer/keystore**; hanya bisa menghasilkan Intent yang tetap lewat Policy
//! Engine ( aturan keras #2, SF8 ).
//!
//! # Eksekusi
//!
//! Skill dijalankan sebagai **subprocess terpisah** ( bukan WASM -- lihat
//! catatan `sandbox` ). Komunikasi lewat JSON di stdin/stdout: agent
//! mengirimkan `SkillInput` ( data pasar, dll ), skill membalas `SkillOutput`
//! ( usulan Intent / tool call ). Batas waktu mutlak ( default 5 detik ).
//!
//! # Permission
//!
//! `skill.toml` wajib mendeklarasikan permission. Skill yang memanggil tool
//! di luar permission ditolak ( acceptance Fase 9 ). Daftar tool yang
//! diizinkan untuk skill ( SF8 ): `get_floor`, `get_listings`, `gas_price`,
//! `mint_plan`. Tool financial ( `buy`, `sign`, `approve` ) **tidak pernah**
//! diizinkan untuk skill.

pub mod manifest;
pub mod sandbox;

pub use manifest::{SkillManifest, SkillPermission, SkillTool};
pub use sandbox::{run_skill, SkillError, SkillInput, SkillOutput};

/// Tool yang bisa dipanggil skill ( SF8 ). Daftar ini tertutup -- tool
/// financial ( buy/sign/approve ) tidak tersedia untuk skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowedTool {
    GetFloor,
    GetListings,
    GasPrice,
    MintPlan,
}

impl AllowedTool {
    /// Semua tool yang sah ( untuk validasi manifest ).
    pub fn all() -> &'static [AllowedTool] {
        &[
            AllowedTool::GetFloor,
            AllowedTool::GetListings,
            AllowedTool::GasPrice,
            AllowedTool::MintPlan,
        ]
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AllowedTool::GetFloor => "get_floor",
            AllowedTool::GetListings => "get_listings",
            AllowedTool::GasPrice => "gas_price",
            AllowedTool::MintPlan => "mint_plan",
        }
    }

    pub fn parse(s: &str) -> Option<AllowedTool> {
        Self::all().iter().copied().find(|t| t.as_str() == s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_tool_parse_dan_str() {
        for t in AllowedTool::all() {
            assert_eq!(AllowedTool::parse(t.as_str()), Some(*t));
        }
        assert!(AllowedTool::parse("buy").is_none());
        assert!(AllowedTool::parse("sign").is_none());
    }

    #[test]
    fn tool_finansial_tidak_tersedia() {
        // Tool financial tidak boleh ada di daftar ( SF8 ).
        let names: Vec<&str> = AllowedTool::all().iter().map(|t| t.as_str()).collect();
        assert!(!names.contains(&"buy"));
        assert!(!names.contains(&"sign"));
        assert!(!names.contains(&"approve"));
    }
}
