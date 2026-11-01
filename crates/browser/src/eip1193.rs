//! Permintaan/respons EIP-1193 (blueprint 3B "Wallet connect lewat browser worker").
//!
//! Halaman web memanggil `window.ethereum.request({ method, params })`.
//! Agent menyadap panggilan itu, memvalidasi, lalu meneruskannya ke
//! Policy Engine. **Tidak ada yang langsung ke signer** (aturan keras #2).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Metode EIP-1193 yang dikenal agent. Method lain diizinkan secara read-only
/// bila terdaftar di `READONLY_METHODS`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Eip1193Method {
    EthRequestAccounts,
    EthAccounts,
    EthChainId,
    PersonalSign,
    EthSignTypedDataV4,
    EthSendTransaction,
    EthSign,
    EthCall,
    EthGetBalance,
    EthBlockNumber,
    WalletWatchAsset,
    Unknown(String),
}

impl Eip1193Method {
    /// Parse nama method EIP-1193 menjadi enum.
    pub fn parse(name: &str) -> Self {
        match name {
            "eth_requestAccounts" => Self::EthRequestAccounts,
            "eth_accounts" => Self::EthAccounts,
            "eth_chainId" => Self::EthChainId,
            "personal_sign" => Self::PersonalSign,
            "eth_signTypedData_v4" => Self::EthSignTypedDataV4,
            "eth_sendTransaction" => Self::EthSendTransaction,
            "eth_sign" => Self::EthSign,
            "eth_call" => Self::EthCall,
            "eth_getBalance" => Self::EthGetBalance,
            "eth_blockNumber" => Self::EthBlockNumber,
            "wallet_watchAsset" => Self::WalletWatchAsset,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Nama wire EIP-1193.
    pub fn as_str(&self) -> &str {
        match self {
            Self::EthRequestAccounts => "eth_requestAccounts",
            Self::EthAccounts => "eth_accounts",
            Self::EthChainId => "eth_chainId",
            Self::PersonalSign => "personal_sign",
            Self::EthSignTypedDataV4 => "eth_signTypedData_v4",
            Self::EthSendTransaction => "eth_sendTransaction",
            Self::EthSign => "eth_sign",
            Self::EthCall => "eth_call",
            Self::EthGetBalance => "eth_getBalance",
            Self::EthBlockNumber => "eth_blockNumber",
            Self::WalletWatchAsset => "wallet_watchAsset",
            Self::Unknown(s) => s.as_str(),
        }
    }

    /// Metode ini mengeksekusi tx on-chain / minta tanda tangan -- wajib
    /// lewat Policy Engine ( 3B, aturan keras #2 ).
    pub fn requires_policy(&self) -> bool {
        matches!(
            self,
            Self::PersonalSign
                | Self::EthSignTypedDataV4
                | Self::EthSendTransaction
                | Self::EthSign
        )
    }

    /// Metode read-only lainnya ( tidak menyentuh signer ).
    pub fn is_readonly(&self) -> bool {
        matches!(
            self,
            Self::EthAccounts
                | Self::EthChainId
                | Self::EthCall
                | Self::EthGetBalance
                | Self::EthBlockNumber
        )
    }
}

/// Request EIP-1193 dari halaman web.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Eip1193Request {
    /// `method` di JSON-RPC 2.0.
    pub method: String,
    /// `params` mentah ( array atau objek ).
    #[serde(default)]
    pub params: serde_json::Value,
    /// Origin halaman yang meminta ( dari `window.location` / CDP ).
    #[serde(default)]
    pub origin: Option<String>,
}

impl Eip1193Request {
    pub fn method_enum(&self) -> Eip1193Method {
        Eip1193Method::parse(&self.method)
    }

    /// Domain origin ( `https://opensea.io` -> `opensea.io` ), lower-case.
    pub fn origin_domain(&self) -> Option<String> {
        let origin = self.origin.as_ref()?;
        let stripped = origin
            .strip_prefix("https://")
            .or_else(|| origin.strip_prefix("http://"))?;
        let host = stripped.split('/').next().unwrap_or(stripped);
        Some(host.to_ascii_lowercase())
    }
}

/// Respons EIP-1193 ke halaman web.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Eip1193Response {
    /// `result` bila sukses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// `error` bila ditolak ( kode + pesan EIP-1193 ).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RequestError>,
}

impl Eip1193Response {
    pub fn ok(result: serde_json::Value) -> Self {
        Self {
            result: Some(result),
            error: None,
        }
    }

    pub fn denied(code: i64, message: impl Into<String>) -> Self {
        Self {
            result: None,
            error: Some(RequestError {
                code,
                message: message.into(),
            }),
        }
    }

    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }
}

/// Error EIP-1193 ( kode standar: 4001 user reject, 4100 unauthorized,
/// 4200 method tidak didukung ).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[error("EIP-1193 error {code}: {message}")]
pub struct RequestError {
    pub code: i64,
    pub message: String,
}

/// Kode error EIP-1193 yang dipakai provider.
pub mod codes {
    /// User menolak permintaan.
    pub const USER_REJECTED: i64 = 4001;
    /// Tidak terotorisasi ( halaman belum punya akun ).
    pub const UNAUTHORIZED: i64 = 4100;
    /// Method tidak didukung.
    pub const UNSUPPORTED_METHOD: i64 = 4200;
    /// Policy engine menolak ( batas, allowlist, approval tak terbatas ).
    pub const POLICY_DENIED: i64 = -32001;
}

/// Tx hasil parse dari `eth_sendTransaction` params ( dipakai policy gate ).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserTx {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub gas: Option<String>,
    #[serde(default)]
    pub max_fee_per_gas: Option<String>,
}

impl BrowserTx {
    /// Apakah calldata meminta approval tak terbatas ( approve(MAX) atau
    /// setApprovalForAll )? Situs yang meminta ini ke alamat asing ditolak
    /// ( 3B: "Situs yang meminta setApprovalForAll atau approve tak terbatas
    /// ke alamat asing otomatis ditolak" ).
    pub fn is_unlimited_approval(&self) -> bool {
        let Some(data) = self.data.as_deref() else {
            return false;
        };
        let data = data.strip_prefix("0x").unwrap_or(data);
        if data.len() < 8 {
            return false;
        }
        let sel = &data[..8].to_ascii_lowercase();
        // approve(address,uint256) = 0x095ea7b3 -- argumen kedua ( uint256 )
        // max = 2^256-1 -> 64 hex 'f'.
        if sel == "095ea7b3" {
            return data.len() >= 8 + 128 && data[72..136].chars().all(|c| c == 'f');
        }
        // setApprovalForAll(address,bool) = 0xa22cb465 -- argumen kedua ( bool )
        // true -> 64 hex dengan byte terakhir 1.
        if sel == "a22cb465" {
            return data.len() >= 8 + 128 && data[72 + 63..136].ends_with('1');
        }
        false
    }
}

/// Parse params `eth_sendTransaction` array menjadi `BrowserTx`.
pub fn parse_send_transaction_params(params: &serde_json::Value) -> Option<BrowserTx> {
    let obj = match params {
        serde_json::Value::Array(arr) => arr.first()?,
        serde_json::Value::Object(_) => params,
        _ => return None,
    };
    let from = obj.get("from")?.as_str()?.to_string();
    let to = obj.get("to")?.as_str()?.to_string();
    Some(BrowserTx {
        from,
        to,
        value: obj
            .get("value")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        data: obj.get("data").and_then(|v| v.as_str()).map(str::to_string),
        gas: obj.get("gas").and_then(|v| v.as_str()).map(str::to_string),
        max_fee_per_gas: obj
            .get("maxFeePerGas")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// Mapping origin -> daftar alamat yang sudah diizinkan halaman lihat
/// ( setelah `eth_requestAccounts` disetujui user ). 3B: satu profil per
/// akun, jadi mapping ini per browser worker.
#[derive(Debug, Default, Clone)]
pub struct OriginAccounts {
    inner: BTreeMap<String, Vec<String>>,
}

impl OriginAccounts {
    pub fn grant(&mut self, origin_domain: impl Into<String>, address: impl Into<String>) {
        self.inner
            .entry(origin_domain.into())
            .or_default()
            .push(address.into());
    }

    pub fn addresses(&self, origin_domain: &str) -> Option<&[String]> {
        self.inner.get(origin_domain).map(|v| v.as_slice())
    }

    pub fn is_authorized(&self, origin_domain: &str, address: &str) -> bool {
        self.inner
            .get(origin_domain)
            .map(|v| v.iter().any(|a| a.eq_ignore_ascii_case(address)))
            .unwrap_or(false)
    }

    pub fn revoke(&mut self, origin_domain: &str) {
        self.inner.remove(origin_domain);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn method_parse_dan_str_roundtrip() {
        for (name, expected) in [
            ("eth_requestAccounts", Eip1193Method::EthRequestAccounts),
            ("personal_sign", Eip1193Method::PersonalSign),
            ("eth_signTypedData_v4", Eip1193Method::EthSignTypedDataV4),
            ("eth_sendTransaction", Eip1193Method::EthSendTransaction),
            ("eth_chainId", Eip1193Method::EthChainId),
            ("eth_call", Eip1193Method::EthCall),
        ] {
            let m = Eip1193Method::parse(name);
            assert_eq!(m, expected);
            assert_eq!(m.as_str(), name);
        }
        let u = Eip1193Method::parse("wallet_permision");
        assert!(matches!(u, Eip1193Method::Unknown(_)));
    }

    #[test]
    fn method_requires_policy_hanya_yang_menyentuh_signer() {
        assert!(Eip1193Method::PersonalSign.requires_policy());
        assert!(Eip1193Method::EthSignTypedDataV4.requires_policy());
        assert!(Eip1193Method::EthSendTransaction.requires_policy());
        assert!(!Eip1193Method::EthChainId.requires_policy());
        assert!(!Eip1193Method::EthAccounts.requires_policy());
    }

    #[test]
    fn origin_domain_menormalisasi() {
        let r = Eip1193Request {
            method: "personal_sign".into(),
            params: json!([]),
            origin: Some("https://Opensea.io/drop/x".into()),
        };
        assert_eq!(r.origin_domain().as_deref(), Some("opensea.io"));
        let r2 = Eip1193Request {
            method: "personal_sign".into(),
            params: json!([]),
            origin: None,
        };
        assert!(r2.origin_domain().is_none());
    }

    #[test]
    fn response_ok_dan_denied() {
        let ok = Eip1193Response::ok(json!("0x1"));
        assert!(!ok.is_error());
        let err = Eip1193Response::denied(codes::USER_REJECTED, "ditolak user");
        assert!(err.is_error());
        let json_str = serde_json::to_string(&err).unwrap();
        assert!(json_str.contains("4001"));
    }

    #[test]
    fn parse_send_transaction_params_berhasil() {
        let params = json!([{
            "from": "0xAb12",
            "to": "0xCd34",
            "value": "0x0",
            "data": "0x"
        }]);
        let tx = parse_send_transaction_params(&params).unwrap();
        assert_eq!(tx.from, "0xAb12");
        assert_eq!(tx.to, "0xCd34");
        assert_eq!(tx.data.as_deref(), Some("0x"));
    }

    #[test]
    fn parse_params_invalid_mengembalikan_none() {
        assert!(parse_send_transaction_params(&json!("bukan")).is_none());
        assert!(parse_send_transaction_params(&json!([{}])).is_none());
    }

    #[test]
    fn approve_max_dideteksi() {
        // approve(address,uint256 max): selector + address 64 hex + uint256 64 hex.
        let data = format!("0x095ea7b3{}{}", "0".repeat(64), "f".repeat(64));
        let tx = BrowserTx {
            from: "0x1".into(),
            to: "0x2".into(),
            value: None,
            data: Some(data),
            gas: None,
            max_fee_per_gas: None,
        };
        assert!(tx.is_unlimited_approval());
    }

    #[test]
    fn approve_terbatas_bukan_unlimited() {
        let amount = format!("{:064x}", 1_000u64);
        let data = format!("0x095ea7b3{}{}", "0".repeat(64), amount);
        let tx = BrowserTx {
            from: "0x1".into(),
            to: "0x2".into(),
            value: None,
            data: Some(data),
            gas: None,
            max_fee_per_gas: None,
        };
        assert!(!tx.is_unlimited_approval());
    }

    #[test]
    fn set_approval_for_all_true_dideteksi() {
        // setApprovalForAll(address,true): selector + address 64 hex + bool 64 hex.
        let flag = format!("{:064x}", 1u8);
        let data = format!("0xa22cb465{}{}", "0".repeat(64), flag);
        let tx = BrowserTx {
            from: "0x1".into(),
            to: "0x2".into(),
            value: None,
            data: Some(data),
            gas: None,
            max_fee_per_gas: None,
        };
        assert!(tx.is_unlimited_approval());
    }

    #[test]
    fn set_approval_for_all_false_bukan_unlimited() {
        let data = format!("0xa22cb465{}{}", "0".repeat(64), "0".repeat(64));
        let tx = BrowserTx {
            from: "0x1".into(),
            to: "0x2".into(),
            value: None,
            data: Some(data),
            gas: None,
            max_fee_per_gas: None,
        };
        assert!(!tx.is_unlimited_approval());
    }

    #[test]
    fn tx_tanpa_data_bukan_approval() {
        let tx = BrowserTx {
            from: "0x1".into(),
            to: "0x2".into(),
            value: None,
            data: None,
            gas: None,
            max_fee_per_gas: None,
        };
        assert!(!tx.is_unlimited_approval());
    }

    #[test]
    fn origin_accounts_grant_cek_revoke() {
        let mut oa = OriginAccounts::default();
        oa.grant("opensea.io", "0xAb12");
        assert!(oa.is_authorized("opensea.io", "0xab12"));
        assert!(!oa.is_authorized("opensea.io", "0x99"));
        assert!(!oa.is_authorized("other.io", "0xAb12"));
        oa.revoke("opensea.io");
        assert!(!oa.is_authorized("opensea.io", "0xAb12"));
    }

    #[test]
    fn request_serde_roundtrip() {
        let r = Eip1193Request {
            method: "eth_sendTransaction".into(),
            params: json!([{"from": "0x1", "to": "0x2"}]),
            origin: Some("https://x.io".into()),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: Eip1193Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back.method, "eth_sendTransaction");
    }
}
