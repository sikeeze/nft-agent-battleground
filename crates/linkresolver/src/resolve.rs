//! `resolve_mint_link` ( blueprint §8.3, §5 poin 1 ).
//!
//! Menerima link ( OpenSea Drops, Zora, Manifold, mint.fun, Magic Eden,
//! atau website project apa pun ), mengambil halaman, lalu mengekstrak
//! **kandidat** alamat kontrak + chain id. Hasil di sini masih kandidat —
//! wajib diverifikasi on-chain oleh `crate::verify` sebelum dipercaya
//! ( §5 poin 4: "tidak ada kontrak yang di-mint hanya karena ada di link" ).
//!
//! Strategi ( universal, tanpa API key ):
//! 1. Normalisasi URL; catat domain untuk deteksi typosquat ( §5 poin 4 ).
//! 2. Ambil HTML halaman via `reqwest` ( HTTP/2, redirect diikuti ).
//! 3. Ekstrak kandidat: pola regex bebas-spacing pada alamat 0x40-hex yang
//!    berdekatan dengan kata kunci `contract`/`address`/`collection` dalam
//!    JSON tersemat ( `<script type="application/json">` ) atau atribut
//!    `content` dari og:meta.
//! 4. chain id: dari TLD domain ( opensea.io|zora.co|mint.fun -> 8453,
//!    magic.eden -> 1 ) atau dari JSON tersemat ( `chainId`/`network` ).
//!
//! ponytail: handler per-platform yang memakai API resmi mereka ( OpenSea
//! Drops API, Zora API, Manifold ) menyusul Fase 12. Sekarang fallback
//! universal + verifikasi on-chain ketat sudah cukup untuk acceptance F6.

use std::time::Duration;

use alloy::primitives::Address;
use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("URL tidak valid: {0}")]
    BadUrl(String),

    #[error("gagal mengambil halaman {url}: {source}")]
    Fetch {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("tidak ada alamat kontrak yang ditemukan di halaman {0}")]
    NoContract(String),

    #[error("chain id tidak terdeteksi untuk {0}")]
    NoChain(String),
}

/// Kandidat hasil resolver ( belum diverifikasi ).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintCandidate {
    pub url: String,
    pub domain: String,
    /// Alamat kontrak kandidat ( hasil ekstraksi halaman ).
    pub contract: Address,
    /// Chain id hasil tebakan domain / JSON halaman.
    pub chain_id: u64,
    /// Ciri khusus platform ( untuk pesan ringkasan ).
    pub platform: Platform,
}

/// Platform yang dikenali dari domain ( §5 poin 1 ).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    OpenSea,
    Zora,
    Manifold,
    MintFun,
    MagicEden,
    /// Website project sendiri / platform lain.
    Website,
}

impl Platform {
    pub fn from_domain(domain: &str) -> Self {
        let host = domain.to_lowercase();
        if host.contains("opensea.io") {
            Self::OpenSea
        } else if host.contains("zora.co") || host.contains("zora.network") {
            Self::Zora
        } else if host.contains("manifold.xyz") {
            Self::Manifold
        } else if host.contains("mint.fun") {
            Self::MintFun
        } else if host.contains("magiceden.io") || host.contains("magic.eden") {
            Self::MagicEden
        } else {
            Self::Website
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenSea => "OpenSea Drops",
            Self::Zora => "Zora",
            Self::Manifold => "Manifold",
            Self::MintFun => "mint.fun",
            Self::MagicEden => "Magic Eden",
            Self::Website => "Website",
        }
    }
}

/// Chain id default per platform ( mayoritas drop ada di Base / Ethereum ).
fn chain_from_domain(domain: &str) -> Option<u64> {
    let host = domain.to_lowercase();
    if host.contains("magiceden.io") || host.contains("magic.eden") {
        Some(1)
    } else if host.contains("opensea.io")
        || host.contains("zora.co")
        || host.contains("zora.network")
        || host.contains("manifold.xyz")
        || host.contains("mint.fun")
    {
        Some(8453)
    } else {
        None
    }
}

/// Resolver link universal. Mengembalikan kandidat ( belum terverifikasi ).
pub async fn resolve_mint_link(raw: &str) -> Result<MintCandidate, ResolveError> {
    let url = Url::parse(raw.trim()).map_err(|e| ResolveError::BadUrl(e.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ResolveError::BadUrl(format!(
            "scheme {scheme} tidak didukung",
            scheme = url.scheme()
        )));
    }
    let domain = url
        .host_str()
        .ok_or_else(|| ResolveError::BadUrl("host hilang".into()))?
        .to_string();

    // Cek typosquat domain dini ( §5 poin 4 ) — hasil ditolak di sini.
    typosquat_check(&domain)?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| ResolveError::BadUrl(e.to_string()))?;
    let body = client
        .get(url.clone())
        .header("User-Agent", "nft-agent/1 (link resolver)")
        .send()
        .await
        .map_err(|e| ResolveError::Fetch {
            url: raw.to_string(),
            source: e,
        })?
        .text()
        .await
        .map_err(|e| ResolveError::Fetch {
            url: raw.to_string(),
            source: e,
        })?;

    let contract =
        extract_contract(&body).ok_or_else(|| ResolveError::NoContract(raw.to_string()))?;
    let chain_id = chain_from_domain(&domain)
        .or_else(|| extract_chain_id(&body))
        .ok_or_else(|| ResolveError::NoChain(raw.to_string()))?;

    Ok(MintCandidate {
        url: raw.trim().to_string(),
        domain: domain.clone(),
        contract,
        chain_id,
        platform: Platform::from_domain(&domain),
    })
}

/// Deteksi typosquatting domain ( §5 poin 4 "domain typosquat" ).
/// Dinyalakan bila:
/// 1. jarak edit ke domain resmi = 1 ( salah ketik 1 karakter ), atau
/// 2. SLD ( nama utama ) sama dengan domain resli tapi TLD berbeda
///    ( mis. `opensea.om`, `magiceden.cm` ), atau
/// 3. SLD punya jarak edit 1 ke SLD resmi dengan TLD resmi ( `opnsea.io` ).
fn typosquat_check(domain: &str) -> Result<(), ResolveError> {
    let host = domain.to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let legit = [
        "opensea.io",
        "zora.co",
        "manifold.xyz",
        "mint.fun",
        "magiceden.io",
        "highlight.xyz",
    ];
    if legit.contains(&host) {
        return Ok(());
    }
    // 1. jarak edit 1 ke seluruh domain resmi.
    for l in legit {
        if edit_distance_one(host, l) {
            return Err(ResolveError::BadUrl(format!(
                "domain {host} mencurigakan ( mirip {l} — kemungkinan typosquat )"
            )));
        }
    }
    // 2. SLD sama, TLD berbeda.
    let Some((sld, tld)) = host.split_once('.') else {
        return Ok(());
    };
    for l in legit {
        let Some((lsld, ltld)) = l.split_once('.') else {
            continue;
        };
        if sld == lsld && tld != ltld {
            return Err(ResolveError::BadUrl(format!(
                "domain {host} mencurigakan ( nama {sld} tapi TLD bukan .{ltld} — typosquat )"
            )));
        }
        // 3. SLD jarak edit 1 dengan TLD resmi ( `opnsea.io` ).
        if tld == ltld && edit_distance_one(sld, lsld) {
            return Err(ResolveError::BadUrl(format!(
                "domain {host} mencurigakan ( mirip {sld}.{ltld} — typosquat )"
            )));
        }
    }
    Ok(())
}

/// True jika `a` dan `b` berjarak edit tepat 1 ( swap / hapus / sisip ).
fn edit_distance_one(a: &str, b: &str) -> bool {
    let (a, b) = if a.len() < b.len() { (a, b) } else { (b, a) };
    let av: Vec<char> = a.chars().collect();
    let bv: Vec<char> = b.chars().collect();
    if bv.len() - av.len() > 1 {
        return false;
    }
    if bv.len() == av.len() {
        let diffs = av.iter().zip(bv.iter()).filter(|(x, y)| x != y).count();
        return diffs == 1;
    }
    // bv lebih panjang 1: cek hapus satu char.
    (0..bv.len()).any(|i| {
        let mut t = bv.clone();
        t.remove(i);
        t == av
    })
}

/// Ekstrak alamat kontrak dari halaman: cari hex 0x40 yang berdekatan
/// dengan kata kunci `contract`/`address`/`collection` ( JSON tersemat
/// atau meta content ). Mengembalikan match pertama yang valid.
fn extract_contract(body: &str) -> Option<Address> {
    let lower = body.to_lowercase();
    // Cari semua alamat 0x... panjang 40 hex.
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + 42 <= bytes.len() {
        if bytes[i] == b'0' && bytes[i + 1] == b'x' {
            let cand = &body[i..i + 42];
            if cand.len() == 42
                && cand[2..].chars().all(|c| c.is_ascii_hexdigit())
                && cand[2..].chars().any(|c| !c.is_ascii_digit())
            {
                let ctx_start = i.saturating_sub(120);
                let ctx = &lower[ctx_start..i.min(lower.len())];
                if ctx.contains("contract")
                    || ctx.contains("address")
                    || ctx.contains("collection")
                    || ctx.contains("nft")
                    || ctx.contains("drop")
                {
                    if let Ok(addr) = cand.parse::<Address>() {
                        return Some(addr);
                    }
                }
            }
            i += 42;
        } else {
            i += 1;
        }
    }
    None
}

/// Ekstrak chain id dari JSON tersemat ( `chainId` / `network` ).
fn extract_chain_id(body: &str) -> Option<u64> {
    let lower = body.to_lowercase();
    for (key, chain) in [
        ("\"chainid\":8453", 8453u64),
        ("\"chainid\":1", 1u64),
        ("\"chainid\":42161", 42161u64),
        ("base", 8453u64),
        ("ethereum", 1u64),
        ("arbitrum", 42161u64),
    ] {
        if lower.contains(key) {
            return Some(chain);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_dari_domain() {
        assert_eq!(Platform::from_domain("opensea.io"), Platform::OpenSea);
        assert_eq!(Platform::from_domain("www.zora.co"), Platform::Zora);
        assert_eq!(Platform::from_domain("magiceden.io"), Platform::MagicEden);
        assert_eq!(Platform::from_domain("proyek.xyz"), Platform::Website);
    }

    #[test]
    fn edit_distance_one_kasus() {
        assert!(edit_distance_one("open-sea.io", "opensea.io")); // sisip
        assert!(!edit_distance_one("opensea.io", "opensea.io")); // sama
        assert!(edit_distance_one("opnsea.io", "opensea.io")); // hilang 1
        assert!(!edit_distance_one("google.com", "opensea.io")); // jauh
        assert!(!edit_distance_one("opensea.om", "opensea.io")); // jarak 2, ditangani SLD check
    }

    #[test]
    fn typosquat_ditolak() {
        assert!(typosquat_check("opensea.io").is_ok());
        assert!(typosquat_check("open-sea.io").is_err());
        assert!(typosquat_check("opnsea.io").is_err());
        assert!(typosquat_check("zora.co").is_ok());
        assert!(typosquat_check("magiceden.io").is_ok());
        assert!(typosquat_check("magiceden.om").is_err()); // SLD sama, TLD beda
        assert!(typosquat_check("opensea.om").is_err());
        // domain umum tanpa kemiripan: lolos ( tetap Website ).
        assert!(typosquat_check("proyek-nft.xyz").is_ok());
    }

    #[test]
    fn extract_contract_dari_json() {
        let html = r#"<html><script type="application/json">{"collection":{"contract":"0xff5C0E51081CAb1bB5a82E40Bc0eF2d0C5e1A1b2"}}</script></html>"#;
        let addr = extract_contract(html);
        assert!(addr.is_some(), "alamat harus ditemukan");
    }

    #[test]
    fn extract_contract_dari_meta() {
        let html = r#"<meta property="og:nft:contract" content="0x1f9840a85d5aF5bf1D1762F925BDADdC4201F984">"#;
        assert!(extract_contract(html).is_some());
    }

    #[test]
    fn extract_chain_id_base() {
        let body = r#"{"chainId":8453,"name":"Base"}"#;
        assert_eq!(extract_chain_id(body), Some(8453));
        let body2 = r#"network:ethereum"#;
        assert_eq!(extract_chain_id(body2), Some(1));
        assert_eq!(extract_chain_id("nothing"), None);
    }

    #[tokio::test]
    async fn resolver_url_invalid_error() {
        let res = resolve_mint_link("bukan-url").await;
        // "bukan-url" diparse sebagai path ( scheme missing ) -> BadUrl.
        assert!(res.is_err());
        let err = res.unwrap_err().to_string();
        assert!(
            err.contains("URL") || err.contains("scheme"),
            "dapat: {err}"
        );
    }

    #[tokio::test]
    async fn resolver_domain_tidak_terjangkau_error_jelas() {
        let res = resolve_mint_link("http://127.0.0.1:9/drop").await;
        assert!(res.is_err());
        let err = res.unwrap_err().to_string();
        // port 9 = discard: connection refused → Fetch.
        assert!(
            err.contains("halaman") || err.contains("URL"),
            "dapat: {err}"
        );
    }
}
