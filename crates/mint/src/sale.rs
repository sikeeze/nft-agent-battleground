//! Deteksi fungsi mint & state sale on-chain ( blueprint §1 F1, §9 Fase 5 ).
//!
//! Membaca langsung dari kontrak ( lewat RPC `eth_call` ) — **bukan** dari
//! halaman web — persis seperti §5 poin 2 "Fungsi mint, harga, maxPerWallet,
//! maxSupply, totalSupply, status sale ... dibaca langsung dari kontrak".
//!
//! Selector ditebak dari nama fungsi standar ( ERC-721/terapan lazim ) dan
//! dicoba satu per satu: `mint()`, `mint(uint256)`, `mint(uint256,uint256)`,
//! `publicMint(uint256)`, `freeMint(uint256)`, `safeMint(address,uint256)`,
//! `mintTo(address,uint256)`, lalu pembaca `price()`, `maxPerWallet()`,
//! `maxSupply()`, `totalSupply()`, `saleActive()`, `balanceOf(address)`.
//!
//! ponytail: tidak ada parsing ABI dari explorer ( butuh akses jaringan
//! publik + rate limit ) — deteksi nama standar cukup untuk contract uji
//! Fase 5. Verifier ABI penuh menyusul di Fase 6 ( `linkresolver` ).

use alloy::primitives::{keccak256, Address, Bytes, U256};
use thiserror::Error;

use nft_chain::{RpcError, RpcPool};

use alloy::primitives::hex;

#[derive(Debug, Error)]
pub enum SaleError {
    #[error("RPC error saat membaca kontrak: {0}")]
    Rpc(#[from] RpcError),

    #[error("tidak ada fungsi mint yang ditemukan di kontrak {0}")]
    NoMintFunction(Address),

    #[error("pembacaan {reader} pada {contract} gagal / tidak ada")]
    ReaderFailed {
        reader: &'static str,
        contract: Address,
    },
}

/// Nama callee yang dianggap fungsi mint ( §1 F1 ). `mint_u256` dst. adalah
/// signature yang sudah diketahui; deteksi memakai eth_call probe.
const MINT_VARIANTS: &[&str] = &[
    "mint()",
    "mint(uint256)",
    "mint(uint256,uint256)",
    "publicMint(uint256)",
    "freeMint(uint256)",
    "safeMint(address,uint256)",
    "mintTo(address,uint256)",
];

/// Selector 4-byte dari signature Solidity ( `keccak256(sig)[..4]` ).
pub fn selector(signature: &str) -> [u8; 4] {
    let hash = keccak256(signature.as_bytes());
    let mut out = [0u8; 4];
    out.copy_from_slice(&hash[..4]);
    out
}

/// Hasil pembacaan state sale ( §5 poin 2 ). Semua nilai **dibaca dari
/// kontrak**, jadi bisa dipakai untuk preflight & perhitungan Tsend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaleState {
    /// Fungsi mint yang terdeteksi ( signature Solidity ).
    pub mint_signature: String,
    pub mint_selector: [u8; 4],
    /// Harga per token dalam wei ( `price()` ). 0 bila tak ada pembaca.
    pub price: U256,
    /// `maxPerWallet()` — 0 bila tak ada pembaca ( berarti tak diketahui ).
    pub max_per_wallet: Option<U256>,
    pub max_supply: Option<U256>,
    pub total_supply: Option<U256>,
    /// `saleActive()` — None bila tak ada pembaca.
    pub sale_active: Option<bool>,
}

impl SaleState {
    /// Jumlah masih bisa di-mint ( `maxSupply - totalSupply` ), bila keduanya
    /// berhasil dibaca.
    pub fn remaining(&self) -> Option<U256> {
        match (self.max_supply, self.total_supply) {
            (Some(max), Some(total)) => Some(max.saturating_sub(total)),
            _ => None,
        }
    }
}

/// Pembaca state sale pada satu kontrak. Clone-able ( `RpcPool` berbagi ).
#[derive(Debug, Clone)]
pub struct SaleReader {
    pool: RpcPool,
}

impl SaleReader {
    pub fn new(pool: RpcPool) -> Self {
        Self { pool }
    }

    /// Akses pool ( dipakai modul lain untuk request RPC langsung ).
    pub fn pool(&self) -> &RpcPool {
        &self.pool
    }

    /// Deteksi fungsi mint ( §1 F1 "Deteksi fungsi mint dari ABI" ) dengan
    /// mencoba setiap signature: `eth_call` tanpa argumen — selector yang
    /// **revert** berarti fungsi tidak ada / tidak cocok. Variant pertama
    /// yang tidak revert menang.
    ///
    /// Catatan: `mint()` tanpa argumen dipakai murni sebagai probe — tx
    /// sebenarnya tetap disusun oleh `compile_exec_plan` dengan argumen
    /// sesuai signature ( lihat `crate::plan` ).
    pub async fn detect_mint(&self, contract: Address) -> Result<[u8; 4], SaleError> {
        for sig in MINT_VARIANTS {
            let sel = selector(sig);
            // Probe: kirim selector saja. Argumen diisi nol sesuai arity —
            // beberapa kontrak revert bila arity tidak cocok, itulah sinyal
            // "fungsi ini bukan variant yang tepat".
            let arity = arg_count(sig);
            let mut data = sel.to_vec();
            for _ in 0..arity {
                data.extend_from_slice(&[0u8; 32]);
            }
            match self
                .pool
                .request::<String>(
                    "eth_call",
                    &serde_json::json!(
                        [{ "to": contract, "data": format!("0x{}", hex::encode(&data)) }, "latest"]
                    ),
                )
                .await
            {
                Ok(_res) => {
                    // non-revert → fungsi ini ada ( atau fallback accept-all,
                    // lihat catatan di bawah ).
                    return Ok(sel);
                }
                Err(e) => {
                    // Revert = coba variant berikutnya. Error non-revert
                    // ( RPC mati ) diteruskan ke pemanggil.
                    if !is_revert(&e) {
                        return Err(SaleError::Rpc(e));
                    }
                }
            }
        }
        Err(SaleError::NoMintFunction(contract))
    }

    /// Baca state sale lengkap: setelah fungsi mint terdeteksi, panggil
    /// pembaca-pembaca standar. Pembaca yang gagal diabaikan ( `None` ),
    /// kecuali semua gagal — itu pertanda kontrak aneh.
    pub async fn read_state(&self, contract: Address) -> Result<SaleState, SaleError> {
        let mint_selector = self.detect_mint(contract).await?;
        let mint_signature = MINT_VARIANTS
            .iter()
            .find(|s| selector(s) == mint_selector)
            .copied()
            .unwrap_or("mint()")
            .to_string();

        let price = self
            .read_u256(contract, "price()", &selector("price()"))
            .await
            .unwrap_or(U256::ZERO);
        let max_per_wallet = self
            .read_u256(contract, "maxPerWallet()", &selector("maxPerWallet()"))
            .await;
        let max_supply = self
            .read_u256(contract, "maxSupply()", &selector("maxSupply()"))
            .await;
        let total_supply = self
            .read_u256(contract, "totalSupply()", &selector("totalSupply()"))
            .await;
        let sale_active = self
            .read_bool(contract, "saleActive()", &selector("saleActive()"))
            .await;
        if price == U256::ZERO
            && max_per_wallet.is_none()
            && max_supply.is_none()
            && total_supply.is_none()
            && sale_active.is_none()
        {
            return Err(SaleError::ReaderFailed {
                reader: "price/maxPerWallet/maxSupply/totalSupply/saleActive",
                contract,
            });
        }

        Ok(SaleState {
            mint_signature,
            mint_selector,
            price,
            max_per_wallet,
            max_supply,
            total_supply,
            sale_active,
        })
    }

    /// `balanceOf(address)` — dipakai `confirm_and_verify` ( §6.3 ) dan
    /// pengecekan limit per wallet sebelum kirim.
    pub async fn balance_of(&self, contract: Address, wallet: Address) -> Result<U256, SaleError> {
        let sel = selector("balanceOf(address)");
        let mut data = sel.to_vec();
        // Address di-padding ke 32 byte ( 12 byte kosong + 20 byte alamat ).
        let mut padded = [0u8; 32];
        padded[12..].copy_from_slice(wallet.as_slice());
        data.extend_from_slice(&padded);
        self.read_u256(contract, "balanceOf(address)", &data)
            .await
            .ok_or(SaleError::ReaderFailed {
                reader: "balanceOf(address)",
                contract,
            })
    }

    /// `ownerOf(uint256)` — verifikasi token benar-benar dimiliki wallet (
    /// §6.3 "cek event Transfer dan balanceOf/ownerOf" ).
    pub async fn owner_of(&self, contract: Address, token_id: U256) -> Result<Address, SaleError> {
        let sel = selector("ownerOf(uint256)");
        let mut data = sel.to_vec();
        data.extend_from_slice(&token_id.to_be_bytes::<32>());
        let raw = self.call(contract, &data).await?;
        if raw.len() < 32 {
            return Err(SaleError::ReaderFailed {
                reader: "ownerOf(uint256)",
                contract,
            });
        }
        let mut buf = [0u8; 20];
        buf.copy_from_slice(&raw[12..32]);
        Ok(Address::from(buf))
    }

    // -------------------------------------------------------------------
    // internal
    // -------------------------------------------------------------------

    async fn call(&self, contract: Address, data: &[u8]) -> Result<Bytes, SaleError> {
        let res = self
            .pool
            .request::<String>(
                "eth_call",
                &serde_json::json!(
                    [{ "to": contract, "data": format!("0x{}", hex::encode(data)) }, "latest"]
                ),
            )
            .await?;
        let hex = res.trim_start_matches("0x");
        let bytes = hex::decode(hex).unwrap_or_default();
        Ok(Bytes::from(bytes))
    }

    async fn read_u256(
        &self,
        contract: Address,
        reader: &'static str,
        data: &[u8],
    ) -> Option<U256> {
        match self.call(contract, data).await {
            Ok(raw) if raw.len() >= 32 => Some(U256::from_be_slice(&raw[..32])),
            Ok(_) => None,
            Err(e) => {
                tracing::debug!(reader, error = %e, "pembaca kontrak tidak ada/ gagal");
                None
            }
        }
    }

    async fn read_bool(
        &self,
        contract: Address,
        reader: &'static str,
        data: &[u8],
    ) -> Option<bool> {
        match self.call(contract, data).await {
            Ok(raw) if raw.len() >= 32 => Some(raw[31] != 0),
            Ok(_) => None,
            Err(e) => {
                tracing::debug!(reader, error = %e, "pembaca kontrak tidak ada/ gagal");
                None
            }
        }
    }
}

fn is_revert(e: &RpcError) -> bool {
    match e {
        RpcError::JsonRpc { code, message } => {
            *code == 3 || message.to_lowercase().contains("revert")
        }
        RpcError::BadResponse(msg) => msg.to_lowercase().contains("revert"),
        _ => false,
    }
}

/// Jumlah argumen dari signature `name(type1,type2)` — untuk probe calldata.
fn arg_count(sig: &str) -> usize {
    let Some(open) = sig.find('(') else {
        return 0;
    };
    let args = &sig[open + 1..sig.len() - 1];
    if args.is_empty() {
        0
    } else {
        args.split(',').count()
    }
}

/// Versi publik `arg_count` ( dipakai `crate::plan` untuk membangun
/// calldata sesuai arity ).
pub fn arg_count_pub(sig: &str) -> usize {
    arg_count(sig)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_dari_signature_standar() {
        // selector(transfer(address,uint256)) = keccak256(...)[..4] = a9059cbb
        let sel = selector("transfer(address,uint256)");
        assert_eq!(sel, [0xa9, 0x05, 0x9c, 0xbb]);
    }

    #[test]
    fn arg_count_menhitung_parameter() {
        assert_eq!(arg_count("mint()"), 0);
        assert_eq!(arg_count("mint(uint256)"), 1);
        assert_eq!(arg_count("mint(uint256,uint256)"), 2);
        assert_eq!(arg_count("safeMint(address,uint256)"), 2);
    }

    #[test]
    fn arg_count_tanpa_kurung() {
        assert_eq!(arg_count("foo"), 0);
    }

    #[test]
    fn remaining_mengurangi_supply() {
        let st = SaleState {
            mint_signature: "mint(uint256)".into(),
            mint_selector: selector("mint(uint256)"),
            price: U256::from(1),
            max_per_wallet: Some(U256::from(3)),
            max_supply: Some(U256::from(10_000)),
            total_supply: Some(U256::from(4_210)),
            sale_active: Some(true),
        };
        assert_eq!(st.remaining(), Some(U256::from(5_790)));
    }

    #[test]
    fn none_bila_supply_tak_diketahui() {
        let st = SaleState {
            mint_signature: "mint()".into(),
            mint_selector: selector("mint()"),
            price: U256::ZERO,
            max_per_wallet: None,
            max_supply: None,
            total_supply: None,
            sale_active: None,
        };
        assert_eq!(st.remaining(), None);
    }
}
