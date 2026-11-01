//! Parser SIWE ( Sign-In with Ethereum, EIP-4361 ) -- blueprint 3B:
//! "parsing pesan, pastikan domain di pesan = domain halaman, nonce segar,
//! tidak ada permintaan tersembunyi. Pesan yang tidak cocok ditolak."
use std::collections::BTreeSet;

use thiserror::Error;

/// Awalan field-field kunci EIP-4361 ( dipakai mendeteksi batas statement ).
const FIELD_KEYS: [&str; 8] = [
    "URI:",
    "Version:",
    "Chain ID:",
    "Nonce:",
    "Issued At:",
    "Expiration Time:",
    "Not Before:",
    "Request ID:",
];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SiweError {
    #[error("domain di pesan ('{message}') != domain halaman ('{origin}')")]
    DomainMismatch { message: String, origin: String },

    #[error("nonce tidak ada atau kosong")]
    MissingNonce,

    #[error("nonce tidak segar: '{nonce}' sudah dipakai")]
    NonceReused { nonce: String },

    #[error("nonce tidak cocok dengan yang dikeluarkan untuk sesi ini")]
    NonceMismatch,

    #[error("statement mengandung permintaan tersembunyi: {0}")]
    HiddenRequest(String),

    #[error("format pesan SIWE tidak valid: {0}")]
    Malformed(String),

    #[error("URI di pesan ('{message}') != origin halaman ('{origin}')")]
    UriMismatch { message: String, origin: String },

    #[error("chain id di pesan ({message}) tidak diizinkan policy")]
    ChainNotAllowed { message: u64 },

    #[error("pesan kedaluwarsa ( expiration time sudah lewat )")]
    Expired,

    #[error("issue time di masa depan")]
    NotYetValid,

    #[error("permintaan material berbahaya: {0}")]
    DangerousRequest(String),
}

/// Pesan SIWE hasil parse ( field wajib EIP-4361 ).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiweMessage {
    /// Domain yang diminta login ( dari pesan ).
    pub domain: String,
    /// Alamat wallet yang diminta sign ( baris kedua pesan ).
    pub address: String,
    pub statement: Option<String>,
    pub uri: String,
    pub version: u32,
    pub chain_id: u64,
    pub nonce: String,
    pub issued_at: String,
    pub expiration_time: Option<String>,
    pub not_before: Option<String>,
    pub request_id: Option<String>,
    pub resources: Vec<String>,
}

impl SiweMessage {
    /// Parse pesan SIWE mentah ( EIP-4361 ).
    ///
    /// Format:
    /// ```text
    /// {domain} wants you to sign in with your Ethereum account:
    /// {address}
    ///
    /// {statement}
    ///
    /// URI: {uri}
    /// Version: {version}
    /// Chain ID: {chain}
    /// Nonce: {nonce}
    /// Issued At: {issued}
    /// Expiration Time: {exp}      (opsional)
    /// Not Before: {nbf}           (opsional)
    /// Request ID: {rid}           (opsional)
    /// Resources:
    /// - {res}                     (opsional, banyak)
    /// ```
    pub fn parse(raw: &str) -> Result<Self, SiweError> {
        let raw = raw.trim();
        let mut lines = raw.lines();

        // Baris 1: `{domain} wants you to sign in with your Ethereum account:`
        let first = lines
            .next()
            .ok_or_else(|| SiweError::Malformed("pesan kosong".into()))?;
        let domain = first
            .strip_suffix(" wants you to sign in with your Ethereum account:")
            .ok_or_else(|| SiweError::Malformed("baris pertama bukan header SIWE".into()))?
            .to_string();

        // Baris 2: address
        let address = lines
            .next()
            .filter(|l| !l.trim().is_empty())
            .ok_or_else(|| SiweError::Malformed("alamat tidak ada".into()))?
            .trim()
            .to_string();

        // Sisa baris: statement bebas ( opsional ) lalu field key:value.
        // Pemisah antara address/field dan statement adalah baris kosong.
        let rest: Vec<&str> = lines.map(str::trim).collect();
        let field_start = rest
            .iter()
            .position(|l| FIELD_KEYS.iter().any(|k| l.starts_with(k)))
            .unwrap_or(rest.len());

        // Statement = baris non-kosong sebelum field pertama. Bila baris
        // non-kosong pertama sudah merupakan field, tidak ada statement.
        let statement: Option<String> = {
            let stmt: Vec<&str> = rest[..field_start]
                .iter()
                .copied()
                .filter(|l| !l.is_empty())
                .collect();
            if stmt.is_empty() {
                None
            } else {
                Some(stmt.join("\n"))
            }
        };

        // Field key:value ( dari field_start sampai akhir ).
        let mut uri = None;
        let mut version = None;
        let mut chain_id = None;
        let mut nonce = None;
        let mut issued_at = None;
        let mut expiration_time = None;
        let mut not_before = None;
        let mut request_id = None;
        let mut resources: Vec<String> = Vec::new();

        let mut in_resources = false;
        for line in &rest[field_start..] {
            if line.is_empty() {
                continue;
            }
            if in_resources {
                if let Some(res) = line.strip_prefix("- ") {
                    resources.push(res.trim().to_string());
                }
                continue;
            }
            if *line == "Resources:" {
                in_resources = true;
                continue;
            }
            let (key, value) = line
                .split_once(':')
                .ok_or_else(|| SiweError::Malformed(format!("field tanpa nilai: {line}")))?;
            let value = value.trim();
            match key {
                "URI" => uri = Some(value.to_string()),
                "Version" => version = Some(value.to_string()),
                "Chain ID" => chain_id = Some(value.to_string()),
                "Nonce" => nonce = Some(value.to_string()),
                "Issued At" => issued_at = Some(value.to_string()),
                "Expiration Time" => expiration_time = Some(value.to_string()),
                "Not Before" => not_before = Some(value.to_string()),
                "Request ID" => request_id = Some(value.to_string()),
                _ => {
                    // Field asing = permintaan tersembunyi ( 3B ).
                    return Err(SiweError::Malformed(format!("field tidak dikenal: {key}")));
                }
            }
        }

        let uri = uri.ok_or_else(|| SiweError::Malformed("URI wajib".into()))?;
        let version: u32 = version
            .ok_or_else(|| SiweError::Malformed("Version wajib".into()))?
            .parse()
            .map_err(|_| SiweError::Malformed("Version bukan angka".into()))?;
        let nonce = nonce.ok_or(SiweError::MissingNonce)?;
        let issued_at = issued_at.ok_or_else(|| SiweError::Malformed("Issued At wajib".into()))?;
        let chain_id: u64 = chain_id
            .ok_or_else(|| SiweError::Malformed("Chain ID wajib".into()))?
            .parse()
            .map_err(|_| SiweError::Malformed("Chain ID bukan angka".into()))?;

        Ok(Self {
            domain,
            address,
            statement,
            uri,
            version,
            chain_id,
            nonce,
            issued_at,
            expiration_time,
            not_before,
            request_id,
            resources,
        })
    }

    /// Validasi pesan SIWE terhadap konteks halaman ( 3B poin: domain, URI,
    /// nonce segar, chain diizinkan ).
    pub fn validate(
        &self,
        origin: &str,
        issued_nonce: &str,
        allowed_chains: &[u64],
        used_nonces: &BTreeSet<String>,
    ) -> Result<(), SiweError> {
        // 1. Domain di pesan = domain halaman.
        let origin_lower = origin.to_ascii_lowercase();
        let origin_domain = host_of(&origin_lower);
        if self.domain.to_ascii_lowercase() != origin_domain {
            return Err(SiweError::DomainMismatch {
                message: self.domain.clone(),
                origin: origin_domain.to_string(),
            });
        }

        // 2. URI di pesan harus berawalan origin halaman ( EIP-4361: URI
        //    mengidentifikasi resource yang diminta; domainnya wajib sama ).
        let uri_lower = self.uri.to_ascii_lowercase();
        let uri_host = host_of(&uri_lower);
        if uri_host != origin_domain {
            return Err(SiweError::UriMismatch {
                message: self.uri.clone(),
                origin: origin.into(),
            });
        }

        // 3. Nonce segar: sama dengan yang dikeluarkan & belum dipakai.
        if self.nonce.is_empty() || issued_nonce.is_empty() {
            return Err(SiweError::MissingNonce);
        }
        if self.nonce != issued_nonce {
            return Err(SiweError::NonceMismatch);
        }
        if used_nonces.contains(&self.nonce) {
            return Err(SiweError::NonceReused {
                nonce: self.nonce.clone(),
            });
        }

        // 4. Chain diizinkan policy.
        if !allowed_chains.contains(&self.chain_id) {
            return Err(SiweError::ChainNotAllowed {
                message: self.chain_id,
            });
        }

        // 5. Statement dengan permintaan tersembunyi ( heuristik kasar, 3B:
        //    "tidak ada permintaan tersembunyi" ).
        if let Some(stmt) = &self.statement {
            check_hidden_request(stmt)?;
        }

        // 6. Resources: bila ada, harus URI http(s) atau EIP-155 chain.
        for res in &self.resources {
            if res.starts_with("http://") || res.starts_with("https://") {
                continue;
            }
            if res.starts_with("chain://") {
                continue;
            }
            return Err(SiweError::DangerousRequest(format!(
                "resource bukan URI standar: {res}"
            )));
        }

        Ok(())
    }
}

/// Deteksi permintaan tersembunyi di statement ( kata kphi bahaya umum ).
fn check_hidden_request(statement: &str) -> Result<(), SiweError> {
    let lower = statement.to_ascii_lowercase();
    const RED_FLAG: [&str; 6] = [
        "approve all",
        "setapprovalforall",
        "unlimited",
        "transfer all",
        "withdraw all",
        "sign this to drain",
    ];
    for flag in RED_FLAG {
        if lower.contains(flag) {
            return Err(SiweError::HiddenRequest(format!(
                "statement mengandung '{flag}'"
            )));
        }
    }
    Ok(())
}

/// Ambil host ( domain ) dari URL/origin, lower-case, tanpa jalur.
fn host_of(url: &str) -> &str {
    let stripped = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    stripped.split('/').next().unwrap_or(stripped)
}

/// Validasi waktu ( RFC 3339 ):
/// - `expiration_time` sudah lewat -> Expired
/// - `not_before` di masa depan -> NotYetValid
pub fn validate_times(msg: &SiweMessage, now_unix: i64) -> Result<(), SiweError> {
    if let Some(exp) = &msg.expiration_time {
        if parse_rfc3339_to_unix(exp).is_some_and(|t| t < now_unix) {
            return Err(SiweError::Expired);
        }
    }
    if let Some(nbf) = &msg.not_before {
        if parse_rfc3339_to_unix(nbf).is_some_and(|t| t > now_unix) {
            return Err(SiweError::NotYetValid);
        }
    }
    Ok(())
}

/// Parse RFC 3339 ke unix detik ( toleran terhadap offset Z/+00:00 ).
fn parse_rfc3339_to_unix(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

/// Cetak ulang pesan SIWE ( untuk verifikasi/audit -- tidak menambah field ).
pub fn render(msg: &SiweMessage) -> String {
    let mut out = format!(
        "{} wants you to sign in with your Ethereum account:\n{}\n\n",
        msg.domain, msg.address
    );
    if let Some(stmt) = &msg.statement {
        out.push_str(stmt);
        out.push_str("\n\n");
    }
    out.push_str(&format!("URI: {}\n", msg.uri));
    out.push_str(&format!("Version: {}\n", msg.version));
    out.push_str(&format!("Chain ID: {}\n", msg.chain_id));
    out.push_str(&format!("Nonce: {}\n", msg.nonce));
    out.push_str(&format!("Issued At: {}", msg.issued_at));
    if let Some(exp) = &msg.expiration_time {
        out.push_str(&format!("\nExpiration Time: {exp}"));
    }
    if let Some(nbf) = &msg.not_before {
        out.push_str(&format!("\nNot Before: {nbf}"));
    }
    if let Some(rid) = &msg.request_id {
        out.push_str(&format!("\nRequest ID: {rid}"));
    }
    if !msg.resources.is_empty() {
        out.push_str("\nResources:");
        for r in &msg.resources {
            out.push_str(&format!("\n- {r}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_MSG: &str = "service.xyz wants you to sign in with your Ethereum account:\n\
0xAbCdEf0123456789AbCdEf0123456789AbCdEf01\n\
\n\
I accept the ServiceOrg Terms of Service.\n\
\n\
URI: https://service.xyz/login\n\
Version: 1\n\
Chain ID: 1\n\
Nonce: 32891756\n\
Issued At: 2026-10-31T12:00:00Z\n\
Expiration Time: 2026-10-31T12:10:00Z\n\
Resources:\n\
- https://service.xyz/terms\n\
- chain://eip155:1";

    const ISSUED_NONCE: &str = "32891756";

    fn parse_valid() -> SiweMessage {
        SiweMessage::parse(VALID_MSG).expect("pesan valid harus terurai")
    }

    fn allowed_chains() -> Vec<u64> {
        vec![1, 8453]
    }

    fn used() -> BTreeSet<String> {
        BTreeSet::new()
    }

    #[test]
    fn parse_mengisi_semua_field() {
        let m = parse_valid();
        assert_eq!(m.domain, "service.xyz");
        assert_eq!(m.address, "0xAbCdEf0123456789AbCdEf0123456789AbCdEf01");
        assert_eq!(
            m.statement.as_deref(),
            Some("I accept the ServiceOrg Terms of Service.")
        );
        assert_eq!(m.uri, "https://service.xyz/login");
        assert_eq!(m.version, 1);
        assert_eq!(m.chain_id, 1);
        assert_eq!(m.nonce, "32891756");
        assert!(m.expiration_time.is_some());
        assert_eq!(m.resources.len(), 2);
    }

    #[test]
    fn parse_tanpa_statement_dan_opsional() {
        let raw = "service.xyz wants you to sign in with your Ethereum account:\n\
0xAb\n\
\n\
URI: https://service.xyz/login\n\
Version: 1\n\
Chain ID: 8453\n\
Nonce: abc\n\
Issued At: 2026-10-31T12:00:00Z";
        let m = SiweMessage::parse(raw).unwrap();
        assert!(m.statement.is_none());
        assert!(m.expiration_time.is_none());
        assert!(m.resources.is_empty());
    }

    #[test]
    fn parse_tanpa_blank_line_setelah_address() {
        // Beberapa implementasi menulis statement langsung tanpa blank line.
        let raw = "service.xyz wants you to sign in with your Ethereum account:\n\
0xAb\n\
URI: https://service.xyz/login\n\
Version: 1\n\
Chain ID: 1\n\
Nonce: abc\n\
Issued At: 2026-10-31T12:00:00Z";
        let m = SiweMessage::parse(raw).unwrap();
        assert!(m.statement.is_none());
        assert_eq!(m.nonce, "abc");
    }

    #[test]
    fn parse_header_tidak_valid_ditolak() {
        let err = SiweMessage::parse("halo dunia").unwrap_err();
        assert!(matches!(err, SiweError::Malformed(_)));
    }

    #[test]
    fn parse_field_asing_ditolak() {
        let raw = VALID_MSG.replace("Nonce: 32891756", "Nonce: 32891756\nSneaky: please");
        let err = SiweMessage::parse(&raw).unwrap_err();
        assert!(matches!(err, SiweError::Malformed(_)));
    }

    #[test]
    fn parse_field_wajib_hilang_ditolak() {
        let raw = VALID_MSG.replace("Nonce: 32891756\n", "");
        assert!(matches!(
            SiweMessage::parse(&raw).unwrap_err(),
            SiweError::MissingNonce
        ));
        let raw = VALID_MSG.replace("Version: 1\n", "");
        assert!(matches!(
            SiweMessage::parse(&raw).unwrap_err(),
            SiweError::Malformed(_)
        ));
    }

    #[test]
    fn validate_domain_berbeda_ditolak() {
        let m = parse_valid();
        let err = m
            .validate("https://evil.xyz", ISSUED_NONCE, &allowed_chains(), &used())
            .unwrap_err();
        assert!(matches!(err, SiweError::DomainMismatch { .. }));
    }

    #[test]
    fn validate_uri_berbeda_ditolak() {
        let m = parse_valid();
        let err = m
            .validate("https://service.xyz", "salah", &allowed_chains(), &used())
            .unwrap_err();
        assert!(matches!(err, SiweError::NonceMismatch));
    }

    #[test]
    fn validate_nonce_sudah_dipakai_ditolak() {
        let m = parse_valid();
        let mut used = used();
        used.insert(ISSUED_NONCE.to_string());
        let err = m
            .validate(
                "https://service.xyz",
                ISSUED_NONCE,
                &allowed_chains(),
                &used,
            )
            .unwrap_err();
        assert!(matches!(err, SiweError::NonceReused { .. }));
    }

    #[test]
    fn validate_chain_tidak_diizinkan_ditolak() {
        let mut m = parse_valid();
        m.chain_id = 999_999; // chain yang tidak ada di allowed_chains
        let err = m
            .validate("https://service.xyz", ISSUED_NONCE, &[1, 8453], &used())
            .unwrap_err();
        assert!(matches!(err, SiweError::ChainNotAllowed { .. }));
    }

    #[test]
    fn validate_lulus_untuk_pesan_benar() {
        let m = parse_valid();
        m.validate(
            "https://service.xyz",
            ISSUED_NONCE,
            &allowed_chains(),
            &used(),
        )
        .unwrap();
    }

    #[test]
    fn validate_origin_dengan_huruf_besar_sama() {
        let m = parse_valid();
        m.validate(
            "https://Service.XYZ/login",
            ISSUED_NONCE,
            &allowed_chains(),
            &used(),
        )
        .unwrap();
    }

    #[test]
    fn statement_permintaan_tersembunyi_ditolak() {
        let mut m = parse_valid();
        m.statement = Some("Please approve all your tokens".into());
        let err = m
            .validate(
                "https://service.xyz",
                ISSUED_NONCE,
                &allowed_chains(),
                &used(),
            )
            .unwrap_err();
        assert!(matches!(err, SiweError::HiddenRequest(_)));
    }

    #[test]
    fn resource_aneh_ditolak() {
        let mut m = parse_valid();
        m.resources.push("javascript:alert(1)".into());
        let err = m
            .validate(
                "https://service.xyz",
                ISSUED_NONCE,
                &allowed_chains(),
                &used(),
            )
            .unwrap_err();
        assert!(matches!(err, SiweError::DangerousRequest(_)));
    }

    #[test]
    fn waktu_kedaluwarsa_dan_belum_valid() {
        let m = parse_valid();
        // expiration 2026-10-31T12:10:00Z -> sebelum itu masih berlaku.
        let exp = parse_rfc3339_to_unix("2026-10-31T12:10:00Z").unwrap();
        assert!(validate_times(&m, exp - 1).is_ok());
        assert!(matches!(
            validate_times(&m, exp + 1).unwrap_err(),
            SiweError::Expired
        ));

        let mut m2 = m.clone();
        m2.not_before = Some("2030-01-01T00:00:00Z".into());
        m2.expiration_time = None; // agar cek NotBefore yang berlaku
        let nbf = parse_rfc3339_to_unix("2030-01-01T00:00:00Z").unwrap();
        assert!(matches!(
            validate_times(&m2, nbf - 1).unwrap_err(),
            SiweError::NotYetValid
        ));
    }

    #[test]
    fn render_menghasilkan_equivalen() {
        let m = parse_valid();
        let rendered = render(&m);
        let m2 = SiweMessage::parse(&rendered).unwrap();
        assert_eq!(m2.domain, m.domain);
        assert_eq!(m2.nonce, m.nonce);
        assert_eq!(m2.resources, m.resources);
        assert_eq!(m2.statement, m.statement);
    }

    #[test]
    fn scheme_http_diterima_di_domain() {
        // http origin masih dianggap cocok bila domain sama.
        let m = parse_valid();
        m.validate(
            "http://service.xyz/login",
            ISSUED_NONCE,
            &allowed_chains(),
            &used(),
        )
        .unwrap();
    }

    #[test]
    fn nonce_kosong_ditolak() {
        let mut m = parse_valid();
        m.nonce = "".into();
        assert!(matches!(
            m.validate("https://service.xyz", "", &allowed_chains(), &used())
                .unwrap_err(),
            SiweError::MissingNonce
        ));
    }

    #[test]
    fn parse_rfc3339_menangani_z_dan_offset() {
        assert_eq!(
            parse_rfc3339_to_unix("2026-10-31T12:00:00Z"),
            parse_rfc3339_to_unix("2026-10-31T14:00:00+02:00")
        );
    }
}
