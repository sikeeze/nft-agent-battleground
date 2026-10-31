//! `nft-mint` — mint engine & eksekusi ( blueprint §9 Fase 5, §7, §8.3 ).
//!
//! Fase 5 ( Hari 17-21 ):
//! - `sale`: deteksi fungsi mint dari kontrak ( harga, limit per wallet,
//!   status sale ) — dibaca **on-line via RPC**, bukan dari halaman web.
//! - `plan`: `compile_exec_plan` ( §8.3 ) — tx varian pre-signed 3 tier gas
//!   per wallet ( §7 "Multi-wallet paralel" ).
//! - `trigger`: `wait_for_trigger` ( timestamp / block / state on-chain
//!   `saleActive()` ).
//! - `race`: `race_broadcast` — kirim ke beberapa relay paralel, ambil yang
//!   pertama OK ( §8.3 ).
//! - `exec`: `MintExecutor` — `confirm_and_verify` ( receipt + balanceOf /
//!   ownerOf, §6.3 ), `bump_or_cancel` ( RBF +12.5% dalam cap, atau cancel
//!   bila sale habis ).
//! - `persist`: idempotency key per ( task, wallet, nonce ) di tabel `txs`
//!   + resume setelah restart ( §6.2 ).
//!
//! Aturan keras yang dipatuhi:
//! 2. **Tidak ada jalur sign yang lewati Policy Engine** — `compile_exec_plan`
//!    hanya menyusun `TxRequest` mentah; penandatanganan hanya lewat
//!    `PolicyEngine::policy_check` → `ApprovedTx` → `WalletSigner`.
//! 3. **Default dry-run** — `MintExecutorConfig.dry_run = true`; tx tidak
//!    dikirim tanpa flag eksplisit.
//!
//! Fase 3 sebelumnya: `Broadcaster` + `submit_bundle` ( public RPC atau
//! private relay Flashbots Protect ).

pub mod broadcaster;
pub mod exec;
pub mod persist;
pub mod plan;
pub mod race;
pub mod sale;
pub mod trigger;

pub use broadcaster::{
    BroadcastMode, BroadcastOutcome, Broadcaster, BroadcasterConfig, SubmitBundleOutcome,
};
pub use exec::{ExecError, MintExecutor, MintExecutorConfig};
pub use race::{race_broadcast, RaceError, RaceOutcome, RaceTarget};
