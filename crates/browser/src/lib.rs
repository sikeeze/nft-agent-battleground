//! `nft-browser` -- Browser worker: provider EIP-1193 + parser SIWE
//! (blueprint F12 / 3B, Fase 7).
//!
//! Prinsip ( 3B "Wallet connect lewat browser worker" ):
//! - Chromium headless diberi provider `window.ethereum` ( EIP-1193 ) buatan
//!   agent.
//! - Setiap `eth_requestAccounts`, `personal_sign`, `eth_signTypedData_v4`,
//!   `eth_sendTransaction` dari halaman web diteruskan ke **Policy Engine**
//!   ( bukan langsung ke signer ) -- aturan keras #2.
//! - SIWE: pesan login diparsing, domain di pesan = domain halaman, nonce
//!   segar, tidak ada permintaan tersembunyi. Pesan yang tidak cocok ditolak.
//! - Situs yang meminta `setApprovalForAll`/approve tak terbatas ke alamat
//!   asing otomatis ditolak.
//!
//! ponytail: integrasi Chromium nyata ( chromiumoxide / CDP ) menyusul saat
//! VPS siap -- modul ini berisi lapisan yang dapat diuji tanpa browser:
//! - `eip1193` -- decode/encode request EIP-1193 + gate policy.
//! - `siwe` -- parser pesan SIWE ( EIP-4361 ) dengan validasi ketat.
//! - `provider` -- `Eip1193Provider` yang meneruskan sign request ke policy.
//!
//! Tidak ada solver captcha / anti-detect ( aturan keras #5 ).

pub mod eip1193;
pub mod provider;
pub mod siwe;

pub use eip1193::{Eip1193Request, Eip1193Response, RequestError};
pub use provider::{Eip1193Provider, ProviderConfig};
pub use siwe::{SiweError, SiweMessage};
