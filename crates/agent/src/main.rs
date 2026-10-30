//! Binary utama `nft-agent`.
//!
//! Fase 1: baru kerangka — modul fungsional menyusul per fase (blueprint §9).
//! Allocator global `mimalloc` sudah dipasang di sini supaya profil rilis
//! (§8.2 poin 10) benar-benar aktif saat binary berjalan.

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL_ALLOCATOR: MiMalloc = MiMalloc;

fn main() {
    println!(
        "nft-agent v{} — mode dry-run, belum ada modul aktif (Fase 1: fondasi).",
        env!("CARGO_PKG_VERSION")
    );
}
