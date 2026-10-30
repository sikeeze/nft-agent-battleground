//! `nft-signer` — penanda tangan transaksi (blueprint §F4/§F5, §3).
//!
//! Aturan keras: **signer hanya menerima `ApprovedTx`** — tidak ada jalur lain
//! untuk membuat signature. `ApprovedTx` dibangun HANYA oleh Policy Engine
//! ( crates/policy ) setelah `policy_check` lulus; konstrukturnya `pub(crate)`
//! sehingga crate lain tidak bisa menyusunnya sendiri.
//!
//! Mendukung: tx legacy ( EIP-155 ) + EIP-1559, `personal_sign` ( EIP-191 ),
//! dan EIP-712 typed data.

use alloy::consensus::transaction::SignableTransaction;
use alloy::consensus::{TxEip1559, TxEnvelope, TxLegacy};
use alloy::primitives::{Address, Bytes, Signature, B256, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::SignerSync;

#[derive(Debug, thiserror::Error)]
pub enum SignerError {
    #[error("signing gagal: {0}")]
    Signing(#[from] alloy::signers::Error),

    #[error("data calldata tidak valid: {0}")]
    InvalidData(String),

    #[error("address tidak valid: {0}")]
    InvalidAddress(String),

    #[error("chain id wajib diisi pada ApprovedTx")]
    MissingChainId,
}

// ---------------------------------------------------------------------------
// ApprovedTx — hanya bisa dibuat oleh crate policy
// ---------------------------------------------------------------------------

/// Transaksi yang SUDAH lulus Policy Engine. Dibangun hanya oleh `nft-policy`
/// ( lewat `PolicyDecision::approve` ); signer menerima ini apa adanya.
///
/// Field bersifat private: tidak ada konstruktor publik. `max_priority_fee_per_gas`
/// menentukan jenis tx: `Some` -> EIP-1559, `None` -> legacy.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApprovedTx {
    chain_id: u64,
    from: String,
    to: String,
    /// nilai dalam wei, string desimal ( presisi penuh, tanpa float )
    value: String,
    /// data calldata, hex dengan prefix 0x
    data: String,
    gas_limit: u64,
    /// legacy: gas price ; 1559: max fee per gas ( wei )
    max_fee_per_gas: u128,
    /// 1559 saja: priority fee ( wei )
    max_priority_fee_per_gas: Option<u128>,
    nonce: u64,
    /// untuk audit trail: intent yang memunculkan approval ini
    intent_id: String,
}

impl ApprovedTx {
    /// Dipakai oleh policy engine untuk mengeluarkan approval.
    /// ponytail: 10 argumen polos dipakai karena ini satu-satunya jalur
    /// konstruksi; bila Fase 3 menambah parameter ( access list, blob ),
    /// naikkan ke struct `TxRequest`. Jangan tambah argumen polos lagi.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        chain_id: u64,
        from: Address,
        to: Address,
        value: U256,
        data: &[u8],
        gas_limit: u64,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: Option<u128>,
        nonce: u64,
        intent_id: impl Into<String>,
    ) -> Self {
        Self {
            chain_id,
            from: from.to_string(),
            to: to.to_string(),
            value: value.to_string(),
            data: format!("0x{}", alloy::primitives::hex::encode(data)),
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            nonce,
            intent_id: intent_id.into(),
        }
    }

    /// Versi `new` yang mengembalikan error ( dipakai policy engine pada
    /// jalur yang sudah memetakan error menjadi `PolicyDecision::Deny` ).
    #[allow(clippy::too_many_arguments)]
    pub fn new_checked(
        chain_id: u64,
        from: Address,
        to: Address,
        value: U256,
        data: &[u8],
        gas_limit: u64,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: Option<u128>,
        nonce: u64,
        intent_id: impl Into<String>,
    ) -> Result<Self, SignerError> {
        if chain_id == 0 {
            return Err(SignerError::MissingChainId);
        }
        Ok(Self::new(
            chain_id,
            from,
            to,
            value,
            data,
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            nonce,
            intent_id,
        ))
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub fn from(&self) -> Address {
        self.from
            .parse()
            .expect("address divalidasi saat konstruksi")
    }

    pub fn to(&self) -> Address {
        self.to.parse().expect("address divalidasi saat konstruksi")
    }

    pub fn intent_id(&self) -> &str {
        &self.intent_id
    }

    /// Berapa wei yang dipindahkan tx ini ( untuk audit/limit ).
    pub fn value(&self) -> U256 {
        U256::from_str_radix(&self.value, 10).expect("nilai divalidasi saat konstruksi")
    }

    pub fn gas_limit(&self) -> u64 {
        self.gas_limit
    }

    pub fn max_fee_per_gas(&self) -> u128 {
        self.max_fee_per_gas
    }
}

// ---------------------------------------------------------------------------
// hasil signing
// ---------------------------------------------------------------------------

/// Tx yang sudah ditandatangani, siap dibroadcast. Hanya bisa didapat dari
/// `WalletSigner::sign_approved`.
#[derive(Debug, Clone)]
pub struct SignedTx {
    envelope: TxEnvelope,
    hash: B256,
    intent_id: String,
}

impl SignedTx {
    pub fn envelope(&self) -> &TxEnvelope {
        &self.envelope
    }

    /// Tx hash ( keccak256 dari envelope yang dienkode ).
    pub fn hash(&self) -> B256 {
        self.hash
    }

    pub fn intent_id(&self) -> &str {
        &self.intent_id
    }
}

// ---------------------------------------------------------------------------
// signer
// ---------------------------------------------------------------------------

/// Penanda tangan yang memegang satu wallet ( dari nft-keystore ).
pub struct WalletSigner {
    wallet_id: String,
    address: Address,
    signer: PrivateKeySigner,
}

impl WalletSigner {
    pub fn new(wallet_id: impl Into<String>, signer: PrivateKeySigner) -> Self {
        let address = signer.address();
        Self {
            wallet_id: wallet_id.into(),
            address,
            signer,
        }
    }

    pub fn wallet_id(&self) -> &str {
        &self.wallet_id
    }

    pub fn address(&self) -> Address {
        self.address
    }

    // -------------------------------------------------------------------
    // jalur TX — hanya dari ApprovedTx
    // -------------------------------------------------------------------

    /// Tandatangani `ApprovedTx` ( legacy atau EIP-1559, otomatis dari isi approval ).
    /// Satu-satunya jalur untuk menghasilkan signature tx.
    pub fn sign_approved(&self, approved: &ApprovedTx) -> Result<SignedTx, SignerError> {
        if approved.chain_id == 0 {
            return Err(SignerError::MissingChainId);
        }
        let to = approved.to();
        let value = approved.value();
        let data: Bytes = approved
            .data
            .parse()
            .map_err(|e| SignerError::InvalidData(format!("{e:?}")))?;

        match approved.max_priority_fee_per_gas {
            // EIP-1559
            Some(max_priority) => {
                let tx = TxEip1559 {
                    chain_id: approved.chain_id,
                    nonce: approved.nonce,
                    gas_limit: approved.gas_limit,
                    max_fee_per_gas: approved.max_fee_per_gas,
                    max_priority_fee_per_gas: max_priority,
                    to: to.into(),
                    value,
                    access_list: Default::default(),
                    input: data,
                };
                let sig = self.sign_tx(&tx)?;
                let signed = tx.into_signed(sig);
                let hash = *signed.hash();
                let envelope = TxEnvelope::Eip1559(signed);
                Ok(SignedTx {
                    envelope,
                    hash,
                    intent_id: approved.intent_id.clone(),
                })
            }
            // legacy
            None => {
                let tx = TxLegacy {
                    chain_id: Some(approved.chain_id),
                    nonce: approved.nonce,
                    gas_limit: approved.gas_limit,
                    gas_price: approved.max_fee_per_gas,
                    to: to.into(),
                    value,
                    input: data,
                };
                let sig = self.sign_tx(&tx)?;
                let signed = tx.into_signed(sig);
                let hash = *signed.hash();
                let envelope = TxEnvelope::Legacy(signed);
                Ok(SignedTx {
                    envelope,
                    hash,
                    intent_id: approved.intent_id.clone(),
                })
            }
        }
    }

    /// Membentuk signature hash dari tx lalu menandatanganinya ( SignerSync ).
    fn sign_tx<T: SignableTransaction<Signature>>(&self, tx: &T) -> Result<Signature, SignerError> {
        let hash = tx.signature_hash();
        self.signer
            .sign_hash_sync(&hash)
            .map_err(SignerError::Signing)
    }

    // -------------------------------------------------------------------
    // personal_sign ( EIP-191 ) dan EIP-712 — tetap lewat policy di lapis
    // aplikasi; signer hanya menyediakan primitive-nya.
    // -------------------------------------------------------------------

    /// EIP-191 personal_sign: message dibungkus prefix `\x19Ethereum Signed Message:`.
    pub fn sign_message(&self, message: &[u8]) -> Result<Signature, SignerError> {
        self.signer
            .sign_message_sync(message)
            .map_err(SignerError::Signing)
    }

    /// EIP-712 typed data. `payload` harus `SolStruct`, `domain` domain separator.
    pub fn sign_typed_data<T: alloy::sol_types::SolStruct>(
        &self,
        payload: &T,
        domain: &alloy::sol_types::Eip712Domain,
    ) -> Result<Signature, SignerError> {
        self.signer
            .sign_typed_data_sync(payload, domain)
            .map_err(SignerError::Signing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::consensus::transaction::SignerRecoverable;
    use alloy::sol_types::SolStruct;
    use rand::RngCore;

    fn random_signer() -> WalletSigner {
        let mut buf = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf);
        let pk = PrivateKeySigner::from_slice(&buf).unwrap();
        WalletSigner::new("burner-1", pk)
    }

    fn approved(priority: Option<u128>) -> ApprovedTx {
        ApprovedTx::new(
            11_155_111, // Sepolia
            Address::ZERO,
            Address::ZERO,
            U256::from(1_000_000_000_000_000_000u64), // 1 ETH
            &[0x01, 0x02, 0x03],
            210_000,
            30_000_000_000u128, // 30 gwei
            priority,
            7,
            "intent-test-1",
        )
    }

    #[test]
    fn sign_eip1559_menghasilkan_signature_valid() {
        let s = random_signer();
        let stx = s.sign_approved(&approved(Some(1_000_000_000))).unwrap();
        let recovered = stx.envelope.recover_signer().unwrap();
        assert_eq!(recovered, s.address());
        assert_eq!(stx.intent_id(), "intent-test-1");
        assert_ne!(stx.hash(), B256::ZERO);
    }

    #[test]
    fn sign_legacy_menghasilkan_signature_valid() {
        let s = random_signer();
        let stx = s.sign_approved(&approved(None)).unwrap();
        let recovered = stx.envelope.recover_signer().unwrap();
        assert_eq!(recovered, s.address());
    }

    #[test]
    fn chain_id_nol_ditolak() {
        let s = random_signer();
        let mut a = approved(Some(1));
        a.chain_id = 0;
        let err = s.sign_approved(&a).unwrap_err();
        assert!(err.to_string().contains("chain id"));
    }

    #[test]
    fn personal_sign_sesuai_eip191() {
        let s = random_signer();
        let msg = b"hello agent war";
        let sig = s.sign_message(msg).unwrap();
        let recovered = sig.recover_address_from_msg(msg).expect("recover EIP-191");
        assert_eq!(recovered, s.address());
        assert!(sig.r() != U256::ZERO);
    }

    #[test]
    fn eip712_sign_dapat_diverifikasi() {
        use alloy::sol;
        use alloy::sol_types::Eip712Domain;

        sol! {
            struct Permit {
                address owner;
                address spender;
                uint256 value;
                uint256 nonce;
                uint256 deadline;
            }
        }

        let s = random_signer();
        let domain = Eip712Domain {
            name: Some("Test".into()),
            version: Some("1".into()),
            chain_id: Some(U256::from(1)),
            verifying_contract: Some(Address::ZERO),
            salt: None,
        };
        let payload = Permit {
            owner: s.address(),
            spender: Address::ZERO,
            value: U256::from(100),
            nonce: U256::ZERO,
            deadline: U256::from(1_700_000_000u64),
        };
        let sig = s.sign_typed_data(&payload, &domain).unwrap();
        let hash = Permit::eip712_signing_hash(&payload, &domain);
        let recovered = sig.recover_address_from_prehash(&hash).unwrap();
        assert_eq!(recovered, s.address());
    }

    #[test]
    fn approved_tx_field_terisi_benar() {
        let a = approved(Some(2));
        assert_eq!(a.gas_limit(), 210_000);
        assert_eq!(a.max_fee_per_gas(), 30_000_000_000);
        assert_eq!(a.value(), U256::from(1_000_000_000_000_000_000u64));
        assert_eq!(a.chain_id(), 11_155_111);
    }
}
