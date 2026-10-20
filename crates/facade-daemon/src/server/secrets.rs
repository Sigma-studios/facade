//! At-rest encryption for env values and private keys, with a master key file
//! that never leaves the server (and is not part of exports).

use std::path::Path;

use anyhow::{Context, anyhow};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::RngCore;

const NONCE_LEN: usize = 12;

pub struct SecretBox(ChaCha20Poly1305);

impl SecretBox {
    pub fn load_or_create(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            let mut key = [0u8; 32];
            rand::rng().fill_bytes(&mut key);
            crate::tls::write_private(path, hex::encode(key).as_bytes())?;
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading master key {}", path.display()))?;
        let key = hex::decode(raw.trim()).context("master key is not hex")?;
        if key.len() != 32 {
            return Err(anyhow!("master key must be 32 bytes"));
        }
        Ok(Self(ChaCha20Poly1305::new(Key::from_slice(&key))))
    }

    pub fn seal(&self, plaintext: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce);
        let mut out = nonce.to_vec();
        out.extend(
            self.0
                .encrypt(Nonce::from_slice(&nonce), plaintext)
                .expect("encryption cannot fail"),
        );
        out
    }

    pub fn open(&self, sealed: &[u8]) -> anyhow::Result<Vec<u8>> {
        if sealed.len() < NONCE_LEN {
            return Err(anyhow!("sealed value too short"));
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        self.0
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| anyhow!("cannot decrypt value (wrong master key?)"))
    }

    pub fn open_string(&self, sealed: &[u8]) -> anyhow::Result<String> {
        Ok(String::from_utf8(self.open(sealed)?)?)
    }
}
