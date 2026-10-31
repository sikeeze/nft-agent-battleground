//! `nft-linkresolver` — link-to-mint ( blueprint §5, §8.3, §9 Fase 6 ).
//!
//! Alur ( §5 ):
//! ```text
//! Link -> [resolve_mint_link] -> [verify_contract on-chain] -> [build_mint_plan]
//!      -> Ringkasan + Konfirmasi Telegram -> [preflight] -> [eksekusi]
//! ```
//! - `resolve`: link -> `MintCandidate` ( alamat kontrak + chain id +
//!   deteksi typosquat dini ).
//! - `verify`: kandidat -> `VerifiedTarget` ( state on-chain via
//!   `nft_mint::sale::SaleReader`, deteksi proxy, **skor risiko** ). Link
//!   yang gagal verifikasi tidak diproses otomatis ( §5 poin 4 ).
//! - `plan`: `build_mint_plan` -> `nft_mint::plan::MintPlan` siap
//!   `compile_exec_plan`.
//!
//! Aturan keras: **LLM tidak boleh membantu di jalur ini** ( §8.2 poin 2 )
//! — resolver hanya parsing deterministik.

pub mod plan;
pub mod resolve;
pub mod verify;

pub use plan::{build_mint_plan, summarize, PlanError, UserCtx};
pub use resolve::{resolve_mint_link, MintCandidate, Platform, ResolveError};
pub use verify::{eth_get_code, verify_contract, RiskLevel, VerifiedTarget, VerifyError};
