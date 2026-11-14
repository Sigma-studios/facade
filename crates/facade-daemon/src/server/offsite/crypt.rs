//! What the offsite copy is encrypted with.
//!
//! The local backup store is not encrypted (it sits next to the database that
//! holds everything anyway), so everything is encrypted on its way out, with a
//! key the server generates once and the user keeps somewhere else (a password
//! manager): without it the copy cannot be read, by the target's owner or by
//! anyone restoring from it.
//!
//! The scheme:
//!
//! * The offsite key is 32 random bytes, in `<data_dir>/offsite.key` (hex,
//!   root only). Two keys are derived from it with BLAKE3's key derivation, one
//!   per use, so that no key ever serves two purposes.
//! * Every object written to the target (chunk, manifest, the marker) is sealed
//!   with XChaCha20-Poly1305 (the RustCrypto `chacha20poly1305` crate) under the
//!   encryption key, with a random 24-byte nonce: random nonces that long never
//!   repeat in practice, so no counter needs to be kept anywhere. The object's
//!   name on the target is the associated data: an object moved or swapped for
//!   another (an old manifest put in place of a new one, one chunk for another)
//!   fails to open instead of restoring the wrong data.
//! * Chunks are named on the target by a keyed BLAKE3 hash of their local name
//!   (itself the BLAKE3 hash of their contents). Plain content hashes would let
//!   whoever reads the target check whether a known file is in a backup; keyed
//!   ones say nothing without the key, while equal chunks still get equal names,
//!   so each is uploaded once.
//!
//! What stays visible on the target: app and volume names, when each backup was
//! taken, and roughly how big each chunk is.
//!
//! A sealed object is `FOFF`, a format byte (1), the nonce, then the ciphertext
//! and its 16-byte tag. A chunk's plaintext is its local file (zstd-compressed),
//! a manifest's is its local file (JSON header, then one chunk per line).

use std::path::Path;

use anyhow::{Context, anyhow, bail};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rand::RngCore;

use crate::server::backup_store::ChunkId;

const MAGIC: &[u8; 4] = b"FOFF";
const FORMAT: u8 = 1;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = MAGIC.len() + 1 + NONCE_LEN;

/// BLAKE3 key derivation contexts: fixed forever, or old copies stop opening.
const ENCRYPTION_CONTEXT: &str = "facade offsite copies 2026-11-14 encryption";
const NAMES_CONTEXT: &str = "facade offsite copies 2026-11-14 chunk names";

/// The file the key lives in, under the data directory.
pub const KEY_FILE: &str = "offsite.key";

pub struct Keys {
    cipher: XChaCha20Poly1305,
    names: [u8; 32],
    /// The key as the user writes it down.
    hex: String,
}

impl Keys {
    pub fn from_secret(secret: &[u8; 32]) -> Self {
        let encryption = blake3::derive_key(ENCRYPTION_CONTEXT, secret);
        Self {
            cipher: XChaCha20Poly1305::new(Key::from_slice(&encryption)),
            names: blake3::derive_key(NAMES_CONTEXT, secret),
            hex: hex::encode(secret),
        }
    }

    /// The key written as the user keeps it: 64 hex digits, spaces and line
    /// breaks ignored (it may come back typed in by hand).
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let digits: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let mut secret = [0u8; 32];
        hex::decode_to_slice(&digits, &mut secret)
            .map_err(|_| anyhow!("an offsite key is 64 hexadecimal digits"))?;
        Ok(Self::from_secret(&secret))
    }

    /// The key in `path`, created first when there is none. Says whether it was.
    pub fn load_or_create(path: &Path) -> anyhow::Result<(Self, bool)> {
        if path.exists() {
            return Ok((Self::load(path)?, false));
        }
        let mut secret = [0u8; 32];
        rand::rng().fill_bytes(&mut secret);
        crate::tls::write_private(path, format!("{}\n", hex::encode(secret)).as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok((Self::from_secret(&secret), true))
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the offsite key {}", path.display()))?;
        Self::parse(&text).with_context(|| path.display().to_string())
    }

    pub fn hex(&self) -> &str {
        &self.hex
    }

    /// `name`'s contents, sealed.
    pub fn seal(&self, name: &str, plaintext: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce);
        let mut out = Vec::with_capacity(HEADER_LEN + plaintext.len() + TAG_LEN);
        out.extend_from_slice(MAGIC);
        out.push(FORMAT);
        out.extend_from_slice(&nonce);
        let sealed = self
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: name.as_bytes(),
                },
            )
            .expect("encryption cannot fail");
        out.extend_from_slice(&sealed);
        out
    }

    /// The contents of the object `name`, if it opens: sealed with this key, and
    /// for this name.
    pub fn open(&self, name: &str, sealed: &[u8]) -> anyhow::Result<Vec<u8>> {
        if sealed.len() < HEADER_LEN + TAG_LEN || &sealed[..MAGIC.len()] != MAGIC {
            bail!("{name} is not an offsite object of facade");
        }
        if sealed[MAGIC.len()] != FORMAT {
            bail!(
                "{name} has format {}, which this version of facade cannot read",
                sealed[MAGIC.len()]
            );
        }
        let nonce = XNonce::from_slice(&sealed[MAGIC.len() + 1..HEADER_LEN]);
        self.cipher
            .decrypt(
                nonce,
                Payload {
                    msg: &sealed[HEADER_LEN..],
                    aad: name.as_bytes(),
                },
            )
            .map_err(|_| anyhow!("{name} does not open (damaged, or sealed with another key)"))
    }

    /// The name of the chunk `id` on the target.
    pub fn chunk_name(&self, id: &ChunkId) -> String {
        hex::encode(blake3::keyed_hash(&self.names, id).as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_and_opens() {
        let keys = Keys::from_secret(&[7; 32]);
        let sealed = keys.seal("chunks/ab/abcd", b"hello");
        assert_eq!(sealed.len(), HEADER_LEN + 5 + TAG_LEN);
        assert_eq!(keys.open("chunks/ab/abcd", &sealed).unwrap(), b"hello");
        // Same plaintext, another nonce.
        assert_ne!(keys.seal("chunks/ab/abcd", b"hello"), sealed);

        // Under another name, with another key, or damaged: it does not open.
        assert!(keys.open("chunks/ab/abce", &sealed).is_err());
        assert!(
            Keys::from_secret(&[8; 32])
                .open("chunks/ab/abcd", &sealed)
                .is_err()
        );
        let mut damaged = sealed.clone();
        *damaged.last_mut().unwrap() ^= 1;
        assert!(keys.open("chunks/ab/abcd", &damaged).is_err());
        assert!(keys.open("x", b"FOFF").is_err());
    }

    #[test]
    fn names_depend_on_the_key() {
        let a = Keys::from_secret(&[1; 32]);
        let b = Keys::from_secret(&[2; 32]);
        let id = *blake3::hash(b"data").as_bytes();
        assert_eq!(a.chunk_name(&id), a.chunk_name(&id));
        assert_ne!(a.chunk_name(&id), b.chunk_name(&id));
        assert_ne!(a.chunk_name(&id), hex::encode(id));
        assert_eq!(a.chunk_name(&id).len(), 64);
    }

    #[test]
    fn parses_what_the_user_kept() {
        let keys = Keys::from_secret(&[0xab; 32]);
        let typed = format!("{}\n {}", &keys.hex()[..32], &keys.hex()[32..]);
        let again = Keys::parse(&typed).unwrap();
        let sealed = keys.seal("n", b"x");
        assert_eq!(again.open("n", &sealed).unwrap(), b"x");
        assert!(Keys::parse("abcd").is_err());

        let dir = std::env::temp_dir().join(format!("facade-offsite-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(KEY_FILE);
        let (created, new) = Keys::load_or_create(&path).unwrap();
        assert!(new);
        let (loaded, new) = Keys::load_or_create(&path).unwrap();
        assert!(!new);
        assert_eq!(created.hex(), loaded.hex());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
