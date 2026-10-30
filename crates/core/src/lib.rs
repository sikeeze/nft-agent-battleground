//! `nft-core` — tipe dasar domain untuk agent NFT (blueprint §4, crate `core`).
//!
//! Isu: `Intent`, `Task`, `WalletRef`, `Event`, `Secret<T>`, config loader,
//! dan storage SQLite. Semua crate lain dibangun di atas tipe-tipe ini.

pub mod config;
pub mod event;
pub mod intent;
pub mod secret;
pub mod storage;
pub mod task;
pub mod wallet;
