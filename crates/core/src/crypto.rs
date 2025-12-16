//! Cryptographic utilities

use crate::{Hotkey, MiniChainError, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Keypair for signing
#[derive(Clone)]
pub struct Keypair {
    signing_key: SigningKey,
    verifying_key: VerifyingKey,
}

impl Keypair {
    /// Generate a new random keypair
    pub fn generate() -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        Self {
            signing_key,
            verifying_key,
        }
    }

    /// Create from secret key bytes
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<Self> {
        let signing_key = SigningKey::from_bytes(bytes);
        let verifying_key = signing_key.verifying_key();
        Ok(Self {
            signing_key,
            verifying_key,
        })
    }

    /// Get the public key as Hotkey
    pub fn hotkey(&self) -> Hotkey {
        Hotkey(self.verifying_key.to_bytes())
    }

    /// Get secret key bytes
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }

    /// Sign a message
    pub fn sign(&self, message: &[u8]) -> SignedMessage {
        let signature = self.signing_key.sign(message);
        SignedMessage {
            message: message.to_vec(),
            signature: signature.to_bytes().to_vec(),
            signer: self.hotkey(),
        }
    }

    /// Sign with additional data (for structured messages)
    pub fn sign_data<T: Serialize>(&self, data: &T) -> Result<SignedMessage> {
        let message =
            bincode::serialize(data).map_err(|e| MiniChainError::Serialization(e.to_string()))?;
        Ok(self.sign(&message))
    }
}

impl std::fmt::Debug for Keypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Keypair({})", self.hotkey())
    }
}

/// A signed message
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignedMessage {
    pub message: Vec<u8>,
    pub signature: Vec<u8>,
    pub signer: Hotkey,
}

impl SignedMessage {
    /// Verify the signature
    pub fn verify(&self) -> Result<bool> {
        let verifying_key = VerifyingKey::from_bytes(self.signer.as_bytes())
            .map_err(|e| MiniChainError::Crypto(format!("Invalid public key: {}", e)))?;

        if self.signature.len() != 64 {
            return Err(MiniChainError::Crypto("Invalid signature length".into()));
        }

        let mut sig_bytes = [0u8; 64];
        sig_bytes.copy_from_slice(&self.signature);
        let signature = Signature::from_bytes(&sig_bytes);

        Ok(verifying_key.verify(&self.message, &signature).is_ok())
    }

    /// Deserialize the message content
    pub fn deserialize<T: for<'de> Deserialize<'de>>(&self) -> Result<T> {
        bincode::deserialize(&self.message)
            .map_err(|e| MiniChainError::Serialization(e.to_string()))
    }
}

/// Hash data using SHA256
pub fn hash(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Hash and return as hex string
pub fn hash_hex(data: &[u8]) -> String {
    hex::encode(hash(data))
}

/// Hash serializable data
pub fn hash_data<T: Serialize>(data: &T) -> Result<[u8; 32]> {
    let bytes =
        bincode::serialize(data).map_err(|e| MiniChainError::Serialization(e.to_string()))?;
    Ok(hash(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keypair_generation() {
        let kp1 = Keypair::generate();
        let kp2 = Keypair::generate();
        assert_ne!(kp1.hotkey(), kp2.hotkey());
    }

    #[test]
    fn test_sign_verify() {
        let kp = Keypair::generate();
        let message = b"Hello, world!";
        let signed = kp.sign(message);

        assert!(signed.verify().unwrap());
        assert_eq!(signed.signer, kp.hotkey());
    }

    #[test]
    fn test_sign_data() {
        let kp = Keypair::generate();

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct TestData {
            value: u64,
            name: String,
        }

        let data = TestData {
            value: 42,
            name: "test".into(),
        };

        let signed = kp.sign_data(&data).unwrap();
        assert!(signed.verify().unwrap());

        let recovered: TestData = signed.deserialize().unwrap();
        assert_eq!(recovered, data);
    }

    #[test]
    fn test_hash() {
        let data = b"test data";
        let h1 = hash(data);
        let h2 = hash(data);
        assert_eq!(h1, h2);

        let h3 = hash(b"different data");
        assert_ne!(h1, h3);
    }

    #[test]
    fn test_keypair_from_bytes() {
        let kp1 = Keypair::generate();
        let secret = kp1.secret_bytes();
        let kp2 = Keypair::from_bytes(&secret).unwrap();
        assert_eq!(kp1.hotkey(), kp2.hotkey());
    }

    #[test]
    fn test_keypair_debug() {
        let kp = Keypair::generate();
        let debug = format!("{:?}", kp);
        assert!(debug.contains("Keypair"));
    }

    #[test]
    fn test_signed_message_invalid_signature() {
        let kp = Keypair::generate();
        let mut signed = kp.sign(b"test message");
        signed.signature[0] ^= 0xff; // Corrupt signature
        assert!(!signed.verify().unwrap());
    }

    #[test]
    fn test_signed_message_wrong_signer() {
        let kp1 = Keypair::generate();
        let kp2 = Keypair::generate();
        let mut signed = kp1.sign(b"test");
        signed.signer = kp2.hotkey(); // Wrong signer
        assert!(!signed.verify().unwrap());
    }

    #[test]
    fn test_signed_message_invalid_signature_length() {
        let mut signed = SignedMessage {
            message: b"test".to_vec(),
            signature: vec![0; 32], // Wrong length
            signer: Hotkey([0; 32]),
        };
        assert!(signed.verify().is_err());
    }

    #[test]
    fn test_hash_hex() {
        let data = b"hello";
        let hex = hash_hex(data);
        assert_eq!(hex.len(), 64); // SHA256 = 32 bytes = 64 hex chars
    }

    #[test]
    fn test_hash_data() {
        #[derive(Serialize)]
        struct Data {
            x: u32,
        }

        let d1 = Data { x: 42 };
        let d2 = Data { x: 42 };
        let d3 = Data { x: 99 };

        assert_eq!(hash_data(&d1).unwrap(), hash_data(&d2).unwrap());
        assert_ne!(hash_data(&d1).unwrap(), hash_data(&d3).unwrap());
    }

    #[test]
    fn test_hash_empty() {
        let h = hash(b"");
        assert_eq!(h.len(), 32);
    }
}
