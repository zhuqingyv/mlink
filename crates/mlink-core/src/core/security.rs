use std::fs;
use std::path::{Path, PathBuf};

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey, X25519};
use ring::hkdf;
use ring::rand::SystemRandom;
use serde::{Deserialize, Serialize};

use crate::protocol::errors::{MlinkError, Result};

pub struct KeyPair {
    pub private: EphemeralPrivateKey,
    pub public_key_bytes: Vec<u8>,
}

pub fn generate_keypair() -> Result<KeyPair> {
    let rng = SystemRandom::new();
    let private = EphemeralPrivateKey::generate(&X25519, &rng)
        .map_err(|_| MlinkError::SecurityError("failed to generate X25519 private key".into()))?;
    let public = private
        .compute_public_key()
        .map_err(|_| MlinkError::SecurityError("failed to derive X25519 public key".into()))?;
    Ok(KeyPair {
        private,
        public_key_bytes: public.as_ref().to_vec(),
    })
}

pub fn derive_shared_secret(
    my_private: EphemeralPrivateKey,
    peer_public: &[u8],
) -> Result<Vec<u8>> {
    let peer = UnparsedPublicKey::new(&X25519, peer_public);
    agreement::agree_ephemeral(my_private, &peer, |shared| shared.to_vec())
        .map_err(|_| MlinkError::SecurityError("ECDH agreement failed".into()))
}

pub fn derive_verification_code(shared_secret: &[u8]) -> u32 {
    let mut buf = [0u8; 4];
    let n = shared_secret.len().min(4);
    buf[..n].copy_from_slice(&shared_secret[..n]);
    u32::from_be_bytes(buf) % 1_000_000
}

const HKDF_INFO_AES: &[u8] = b"mlink/aes-256-gcm/v1";

pub fn derive_aes_key(shared_secret: &[u8]) -> Vec<u8> {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, b"mlink-v1");
    let prk = salt.extract(shared_secret);
    let okm = prk
        .expand(&[HKDF_INFO_AES], hkdf::HKDF_SHA256)
        .expect("HKDF expand with valid length");
    let mut key = [0u8; 32];
    okm.fill(&mut key).expect("HKDF fill 32 bytes");
    key.to_vec()
}

fn seq_nonce(seq: u16) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[10..12].copy_from_slice(&seq.to_be_bytes());
    nonce
}

fn aead_key(key: &[u8]) -> Result<LessSafeKey> {
    if key.len() != 32 {
        return Err(MlinkError::SecurityError(format!(
            "aes key must be 32 bytes, got {}",
            key.len()
        )));
    }
    let unbound = UnboundKey::new(&AES_256_GCM, key)
        .map_err(|_| MlinkError::SecurityError("failed to build AES-256-GCM key".into()))?;
    Ok(LessSafeKey::new(unbound))
}

pub fn encrypt(data: &[u8], key: &[u8], seq: u16) -> Result<Vec<u8>> {
    let sealing = aead_key(key)?;
    let nonce = Nonce::assume_unique_for_key(seq_nonce(seq));
    let mut buf = data.to_vec();
    sealing
        .seal_in_place_append_tag(nonce, Aad::empty(), &mut buf)
        .map_err(|_| MlinkError::SecurityError("AES-256-GCM encrypt failed".into()))?;
    Ok(buf)
}

pub fn decrypt(data: &[u8], key: &[u8], seq: u16) -> Result<Vec<u8>> {
    let opening = aead_key(key)?;
    let nonce = Nonce::assume_unique_for_key(seq_nonce(seq));
    let mut buf = data.to_vec();
    let plaintext = opening
        .open_in_place(nonce, Aad::empty(), &mut buf)
        .map_err(|_| MlinkError::SecurityError("AES-256-GCM decrypt failed".into()))?;
    Ok(plaintext.to_vec())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedPeer {
    pub app_uuid: String,
    pub public_key: Vec<u8>,
    pub name: String,
    pub trusted_at: String,
}

#[derive(Debug)]
pub struct TrustStore {
    path: PathBuf,
    peers: Vec<TrustedPeer>,
}

impl TrustStore {
    pub fn new(path: PathBuf) -> Result<Self> {
        let peers = if path.exists() {
            let bytes = fs::read(&path)?;
            if bytes.is_empty() {
                Vec::new()
            } else {
                serde_json::from_slice(&bytes)
                    .map_err(|e| MlinkError::CodecError(format!("trust store parse: {}", e)))?
            }
        } else {
            Vec::new()
        };
        Ok(Self { path, peers })
    }

    pub fn default_path() -> Result<PathBuf> {
        let home = std::env::var("HOME")
            .map_err(|_| MlinkError::SecurityError("HOME env var not set".into()))?;
        let mut p = PathBuf::from(home);
        p.push(".mlink");
        p.push("trusted_peers.json");
        Ok(p)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_trusted(&self, app_uuid: &str) -> bool {
        self.peers.iter().any(|p| p.app_uuid == app_uuid)
    }

    pub fn get(&self, app_uuid: &str) -> Option<&TrustedPeer> {
        self.peers.iter().find(|p| p.app_uuid == app_uuid)
    }

    pub fn add(&mut self, peer: TrustedPeer) -> Result<()> {
        if let Some(existing) = self.peers.iter_mut().find(|p| p.app_uuid == peer.app_uuid) {
            *existing = peer;
        } else {
            self.peers.push(peer);
        }
        self.persist()
    }

    pub fn remove(&mut self, app_uuid: &str) -> Result<()> {
        self.peers.retain(|p| p.app_uuid != app_uuid);
        self.persist()
    }

    pub fn list(&self) -> &[TrustedPeer] {
        &self.peers
    }

    fn persist(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&self.peers)
            .map_err(|e| MlinkError::CodecError(format!("trust store serialize: {}", e)))?;
        fs::write(&self.path, bytes)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_peer(uuid: &str) -> TrustedPeer {
        TrustedPeer {
            app_uuid: uuid.into(),
            public_key: vec![1, 2, 3],
            name: format!("peer-{}", uuid),
            trusted_at: "2026-04-19T00:00:00Z".into(),
        }
    }

    #[test]
    fn keypair_has_32_byte_public() {
        let kp = generate_keypair().expect("gen");
        assert_eq!(kp.public_key_bytes.len(), 32);
    }

    #[test]
    fn ecdh_both_sides_agree() {
        let a = generate_keypair().expect("a");
        let b = generate_keypair().expect("b");
        let a_pub = a.public_key_bytes.clone();
        let b_pub = b.public_key_bytes.clone();

        let a_shared = derive_shared_secret(a.private, &b_pub).expect("a shared");
        let b_shared = derive_shared_secret(b.private, &a_pub).expect("b shared");

        assert_eq!(a_shared, b_shared);
        assert!(!a_shared.is_empty());
    }

    #[test]
    fn verification_code_is_six_digits() {
        let secret = vec![0xFF, 0xFF, 0xFF, 0xFF, 0xAA, 0xBB];
        let code = derive_verification_code(&secret);
        assert!(code < 1_000_000);
    }

    #[test]
    fn verification_code_same_secret_same_code() {
        let secret = vec![0x12, 0x34, 0x56, 0x78];
        assert_eq!(
            derive_verification_code(&secret),
            derive_verification_code(&secret)
        );
    }

    #[test]
    fn verification_code_handles_short_secret() {
        let short = vec![0x01, 0x02];
        let code = derive_verification_code(&short);
        assert!(code < 1_000_000);
    }

    #[test]
    fn derive_aes_key_is_32_bytes_and_stable() {
        let secret = b"some-shared-secret-from-ecdh";
        let k1 = derive_aes_key(secret);
        let k2 = derive_aes_key(secret);
        assert_eq!(k1.len(), 32);
        assert_eq!(k1, k2);
    }

    #[test]
    fn different_secrets_give_different_keys() {
        let a = derive_aes_key(b"secret-a");
        let b = derive_aes_key(b"secret-b");
        assert_ne!(a, b);
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = derive_aes_key(b"shared");
        let plaintext = b"hello mlink";
        let ct = encrypt(plaintext, &key, 42).expect("encrypt");
        assert_ne!(&ct[..], plaintext);
        assert!(ct.len() > plaintext.len(), "tag should be appended");
        let pt = decrypt(&ct, &key, 42).expect("decrypt");
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn decrypt_wrong_seq_fails() {
        let key = derive_aes_key(b"shared");
        let ct = encrypt(b"payload", &key, 10).expect("encrypt");
        let err = decrypt(&ct, &key, 11).unwrap_err();
        assert!(matches!(err, MlinkError::SecurityError(_)));
    }

    #[test]
    fn decrypt_wrong_key_fails() {
        let key_a = derive_aes_key(b"a");
        let key_b = derive_aes_key(b"b");
        let ct = encrypt(b"payload", &key_a, 1).expect("encrypt");
        let err = decrypt(&ct, &key_b, 1).unwrap_err();
        assert!(matches!(err, MlinkError::SecurityError(_)));
    }

    #[test]
    fn encrypt_rejects_bad_key_len() {
        let err = encrypt(b"x", &[0u8; 16], 0).unwrap_err();
        assert!(matches!(err, MlinkError::SecurityError(_)));
    }

    #[test]
    fn trust_store_new_on_missing_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("trusted.json");
        let store = TrustStore::new(path).expect("new");
        assert!(store.list().is_empty());
    }

    #[test]
    fn trust_store_add_and_query() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("trusted.json");
        let mut store = TrustStore::new(path).expect("new");

        store.add(make_peer("uuid-a")).expect("add");
        assert!(store.is_trusted("uuid-a"));
        assert!(!store.is_trusted("uuid-b"));
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn trust_store_add_updates_existing() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("trusted.json");
        let mut store = TrustStore::new(path).expect("new");

        store.add(make_peer("uuid-a")).expect("add");
        let mut updated = make_peer("uuid-a");
        updated.name = "renamed".into();
        store.add(updated).expect("readd");

        assert_eq!(store.list().len(), 1);
        assert_eq!(store.get("uuid-a").unwrap().name, "renamed");
    }

    #[test]
    fn trust_store_remove() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("trusted.json");
        let mut store = TrustStore::new(path).expect("new");

        store.add(make_peer("uuid-a")).expect("add a");
        store.add(make_peer("uuid-b")).expect("add b");
        store.remove("uuid-a").expect("remove");

        assert!(!store.is_trusted("uuid-a"));
        assert!(store.is_trusted("uuid-b"));
    }

    #[test]
    fn trust_store_persists_across_instances() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("trusted.json");

        {
            let mut store = TrustStore::new(path.clone()).expect("new");
            store.add(make_peer("uuid-a")).expect("add");
        }

        let store2 = TrustStore::new(path).expect("reload");
        assert!(store2.is_trusted("uuid-a"));
        assert_eq!(store2.list().len(), 1);
    }

    #[test]
    fn trust_store_creates_parent_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("nested/dir/trusted.json");
        let mut store = TrustStore::new(path.clone()).expect("new");
        store.add(make_peer("uuid-a")).expect("add");
        assert!(path.exists());
    }
}
