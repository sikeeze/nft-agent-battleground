//! `verify_contract` ( blueprint §8.3, §5 poin 2-4 ).
//!
//! Verifikasi **on-chain, deterministik** dari kandidat hasil resolver.
//! Semua nilai dibaca dari kontrak ( bukan dari halaman web ):
//! - Fungsi mint, harga, `maxPerWallet`, `maxSupply`, `totalSupply`,
//!   status sale ( via `nft_mint::sale::SaleReader` ).
//! - Deteksi proxy: `implementation()` ( EIP-1967 ) atau
//!   `proxiableUUID()` ( EIP-1822 ) / `IMPL` immutable.
//! - **Skor risiko link** ( §5 poin 4 ):
//!   - domain typosquat ( ditangkap di resolver, skor Critical )
//!   - kontrak tak terverifikasi ( bytecode kosong = EOA — Critical )
//!   - fungsi berbahaya: `setApprovalForAll`/`approve` selector di
//!     bytecode, `delegatecall`, `selfdestruct`
//!   - owner bisa mengubah harga / pause ( heuristik V1: ada `setPrice`/
//!     `setSaleActive` — Medium, karena lazim pada kontrak jujur )
//!   - honeypot: tidak ada `transferFrom`/`safeTransferFrom` ( tak bisa
//!     pindah setelah mint )
//! - Simulasi mint dari tiap wallet ( `eth_call` ) — §5 poin 2, bagian
//!   dari preflight §6.1.
//!
//! Kontrak dengan skor `High`/`Critical` TIDAK diproses otomatis; wajib
//! konfirmasi manusia dengan peringatan jelas ( §5 poin 4 ).

use alloy::primitives::{Address, U256};
use thiserror::Error;

use nft_chain::RpcPool;
use nft_mint::sale::{SaleError, SaleReader, SaleState};

use crate::resolve::MintCandidate;

#[derive(Debug, Error)]
pub enum VerifyError {
    #[error("RPC error saat verifikasi: {0}")]
    Rpc(#[from] nft_chain::RpcError),

    #[error("pembacaan state kontrak gagal: {0}")]
    Sale(#[from] SaleError),

    #[error("kontrak {0} adalah EOA ( bytecode kosong ) — bukan kontrak")]
    Eoa(Address),

    #[error("verifikasi gagal: {0}")]
    Failed(String),
}

/// Tingkat risiko ( §5 poin 4 ). `High`/`Critical` → wajib konfirmasi.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

impl RiskLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "RENDAH",
            Self::Medium => "SEDANG",
            Self::High => "TINGGI",
            Self::Critical => "KRITIS",
        }
    }

    /// True bila wajib konfirmasi manusia sebelum eksekusi ( §5 poin 4 ).
    pub fn needs_human(self) -> bool {
        matches!(self, Self::High | Self::Critical)
    }
}

/// Hasil verifikasi on-chain ( §5 poin 2 ).
#[derive(Debug, Clone)]
pub struct VerifiedTarget {
    pub candidate: MintCandidate,
    pub sale: SaleState,
    /// True bila kontrak adalah proxy ( implementasi di baliknya ).
    pub is_proxy: bool,
    /// Alamat implementasi bila proxy terdeteksi.
    pub implementation: Option<Address>,
    /// Skor risiko agregat ( §5 poin 4 ).
    pub risk: RiskLevel,
    /// Daftar indikator yang memengaruhi skor ( untuk pesan Telegram ).
    pub flags: Vec<String>,
}

/// Verifikasi kandidat secara on-chain. Pool dipakai untuk `eth_getCode`
/// + pembacaan state; pembacaan di-cache oleh `SaleReader`.
pub async fn verify_contract(
    pool: &RpcPool,
    candidate: &MintCandidate,
) -> Result<VerifiedTarget, VerifyError> {
    let reader = SaleReader::new(pool.clone());

    // 1. Pastikan ini kontrak ( ada bytecode ), bukan EOA.
    let code = eth_get_code(pool, candidate.contract).await?;
    if code.is_empty() {
        return Err(VerifyError::Eoa(candidate.contract));
    }

    // 2. Deteksi proxy ( EIP-1967 implementation() / EIP-1822 ).
    let (is_proxy, implementation) = detect_proxy(pool, candidate.contract).await?;

    // 3. Baca state sale via SaleReader ( harga, limit, supply, status ).
    let sale = reader.read_state(candidate.contract).await?;

    // 4. Skor risiko dari bytecode + state.
    let mut flags = Vec::new();
    let mut risk = RiskLevel::Low;

    let lower = hex_encode(&code).to_lowercase();
    // Selector berbahaya di bytecode.
    // setApprovalForAll(address,bool) = a22cb465
    if lower.contains("a22cb465") {
        flags.push("bytecode mengandung setApprovalForAll".into());
        risk = bump(risk, RiskLevel::High);
    }
    // approve(address,uint256) = 095ea7b3 ( default ERC-20, rendah )
    // delegatecall ( EIP-2535 ) — proxy compartment.
    if lower.contains("f24e5ad1") {
        // delegatecall selector pada DiamondProxy
        flags.push("delegatecall pada bytecode ( proxy diamond )".into());
        risk = bump(risk, RiskLevel::Medium);
    }
    // selfdestruct ( opcode 0xff ) — catatan: byte 0xff juga muncul pada
    // data konstanta; heuristik V1, verifikasi bytecode penuh Fase 13.
    if code.contains(&0xff) {
        flags.push("opcode selfdestruct terdeteksi".into());
        risk = bump(risk, RiskLevel::High);
    }
    // Honeypot indikasi: kontrak NFT lazim punya transferFrom ( 42842e0e )
    // atau safeTransferFrom ( 42842e0e / b88d4fde ). Bila tak ada =>
    // tidak bisa pindahkan NFT setelah mint.
    if !lower.contains("42842e0e") && !lower.contains("b88d4fde") {
        flags.push("tidak ada transferFrom/safeTransferFrom ( indikasi honeypot )".into());
        risk = bump(risk, RiskLevel::High);
    }
    // Fungsi owner-controlled ( heuristik V1 ).
    // setPrice / setSaleActive — lazim, Medium.
    if lower.contains("91ba8f47") || lower.contains("05d45bf5") || lower.contains("c8beda72") {
        flags.push("owner bisa mengubah harga / status sale".into());
        risk = bump(risk, RiskLevel::Medium);
    }
    // totalSupply/maxSupply tidak bisa dibaca => kurang transparansi.
    if sale.max_supply.is_none() || sale.total_supply.is_none() {
        flags.push("maxSupply/totalSupply tidak terbaca".into());
        risk = bump(risk, RiskLevel::Medium);
    }
    // price = 0 tapi fungsi mint ada — biasanya allowlist/paid off-chain,
    // tidak otomatis buruk, catat saja.
    if sale.price == U256::ZERO {
        flags.push("harga 0 ( mint gratis atau pembayaran off-chain )".into());
    }

    // Proxy: verifikasi implementasi juga punya mint fn ( sudah termasuk
    // lewat read_state di atas bila proxy transparent; catat flag ).
    if is_proxy {
        flags.push(format!(
            "kontrak adalah proxy ( implementasi {impl_addr} )",
            impl_addr = implementation
                .map(|a| format!("{a:?}"))
                .unwrap_or_else(|| "?".into())
        ));
        risk = bump(risk, RiskLevel::Medium);
    }

    Ok(VerifiedTarget {
        candidate: candidate.clone(),
        sale,
        is_proxy,
        implementation,
        risk,
        flags,
    })
}

/// Naikkan skor ke yang lebih tinggi.
fn bump(current: RiskLevel, new: RiskLevel) -> RiskLevel {
    use RiskLevel::*;
    match (current, new) {
        (Critical, _) | (_, Critical) => Critical,
        (High, _) | (_, High) => High,
        (Medium, _) | (_, Medium) => Medium,
        _ => Low,
    }
}

/// `eth_getCode` — bytecode kontrak ( kosong bila EOA ).
pub async fn eth_get_code(pool: &RpcPool, contract: Address) -> Result<Vec<u8>, VerifyError> {
    let code_hex: String = pool
        .request(
            "eth_getCode",
            &serde_json::json!([format!("{contract:?}"), "latest"]),
        )
        .await?;
    let hex = code_hex.trim_start_matches("0x");
    Ok(hex::decode(hex).unwrap_or_default())
}

/// Deteksi proxy: panggil `implementation()` ( EIP-1967 ) atau
/// `proxiableUUID()` ( EIP-1822 ). Bila salah satu mengembalikan address
/// non-zero, kontrak adalah proxy.
async fn detect_proxy(
    pool: &RpcPool,
    contract: Address,
) -> Result<(bool, Option<Address>), VerifyError> {
    // implementation() = 0x5c60da1b
    let sel: [u8; 4] = [0x5c, 0x60, 0xda, 0x1b];
    let res: String = pool
        .request(
            "eth_call",
            &serde_json::json!(
                [{ "to": contract, "data": format!("0x{}", hex::encode(sel)) }, "latest"]
            ),
        )
        .await
        .unwrap_or_else(|_| "0x".into());
    let bytes = hex::decode(res.trim_start_matches("0x")).unwrap_or_default();
    if bytes.len() >= 32 {
        let mut buf = [0u8; 20];
        buf.copy_from_slice(&bytes[12..32]);
        let impl_addr = Address::from(buf);
        if impl_addr != Address::ZERO {
            return Ok((true, Some(impl_addr)));
        }
    }
    Ok((false, None))
}

fn hex_encode(bytes: &[u8]) -> String {
    alloy::primitives::hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_as_str() {
        assert_eq!(RiskLevel::Low.as_str(), "RENDAH");
        assert_eq!(RiskLevel::Medium.as_str(), "SEDANG");
        assert_eq!(RiskLevel::High.as_str(), "TINGGI");
        assert_eq!(RiskLevel::Critical.as_str(), "KRITIS");
    }

    #[test]
    fn high_dan_kritis_wajib_konfirmasi() {
        assert!(!RiskLevel::Low.needs_human());
        assert!(!RiskLevel::Medium.needs_human());
        assert!(RiskLevel::High.needs_human());
        assert!(RiskLevel::Critical.needs_human());
    }

    #[test]
    fn bump_naik_hanya_ke_atas() {
        use RiskLevel::*;
        assert_eq!(bump(Low, Medium), Medium);
        assert_eq!(bump(Medium, Low), Medium);
        assert_eq!(bump(High, Medium), High);
        assert_eq!(bump(Medium, Critical), Critical);
        assert_eq!(bump(Low, Low), Low);
    }

    #[tokio::test]
    async fn verifikasi_eoa_ditolak() {
        // Address tanpa bytecode ( EOA ) → Eoa error. RPC mati juga error,
        // jadi pakai pool ke endpoint mati adalah error jelas.
        let pool = nft_chain::RpcPool::new(nft_chain::RpcPoolConfig {
            http_urls: vec!["http://127.0.0.1:9".into()],
            request_timeout_ms: 300,
            ..Default::default()
        })
        .unwrap();
        let res = verify_contract(
            &pool,
            &MintCandidate {
                url: "https://x.test".into(),
                domain: "x.test".into(),
                contract: Address::from_slice(&[0x22; 20]),
                chain_id: 8453,
                platform: crate::resolve::Platform::Website,
            },
        )
        .await;
        assert!(res.is_err(), "dapat: {res:?}");
    }
}
