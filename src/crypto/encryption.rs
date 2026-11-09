//! RakNet encryption (gophertunnel `minecraft/conn.go` `enableEncryption`, `packet/encryption.go`).
//!
//! The server sends a 16-byte salt in an ES384 JWT. Both sides derive
//! `key = SHA-256(salt || ECDH shared secret)` and run AES-256-CTR from IV `key[0..12] || 00 00 00 02`,
//! one continuous stream per direction. Each packet (everything after the 0xfe byte) carries an
//! 8-byte checksum, the start of `SHA-256(LE64(counter) || plaintext || key)`, with separate send
//! and receive counters. Why CTR and not CFB8 is in `.claude/docs/network.md`.

use std::fmt;

use aws_lc_rs::cipher::{AES_256, EncryptingKey, EncryptionContext, UnboundCipherKey};
use aws_lc_rs::constant_time::verify_slices_are_equal;
use aws_lc_rs::iv::FixedLength;
use serde::Serialize;

use super::{
    Error, PublicKey, RAW_STD, Result, SHA256_BYTES, ServerKey, fill_random, sha256, sign_jwt,
};

/// Bytes in an AES block.
const BLOCK_BYTES: usize = 16;
/// Bytes of the checksum at the end of each encrypted packet.
pub(crate) const CHECKSUM_BYTES: usize = 8;
/// The counter the first block of a stream starts at: the IV's last four bytes, big-endian.
const IV_FIRST_COUNTER: u128 = 2;
/// Bytes of the key that start the IV.
const IV_KEY_BYTES: usize = 12;
/// Bytes of the AES-256 key, which is the SHA-256 digest.
const KEY_BYTES: usize = SHA256_BYTES;
/// Bytes of the random salt.
const SALT_BYTES: usize = 16;

/// The server half of the encryption handshake.
#[derive(Debug)]
pub(crate) struct Handshake {
    /// The `ServerToClientHandshake` JWT.
    pub(crate) jwt: String,
    /// Encrypts what the server sends, after the handshake packet itself went out in plain.
    pub(crate) encryptor: Encryptor,
    /// Decrypts what the client sends from its `ClientToServerHandshake` on.
    pub(crate) decryptor: Decryptor,
}

impl Handshake {
    /// A handshake with `client` (the login's `cpk`) under a fresh random salt.
    pub(crate) fn new(server: &ServerKey, client: &PublicKey) -> Result<Self> {
        let mut salt = [0_u8; SALT_BYTES];
        fill_random(&mut salt)?;
        Self::with_salt(server, client, salt)
    }

    fn with_salt(server: &ServerKey, client: &PublicKey, salt: [u8; SALT_BYTES]) -> Result<Self> {
        let jwt = sign_jwt(
            server,
            &SaltClaims {
                salt: &RAW_STD.encode(&salt),
            },
        )?;
        let key = sha256(&[&salt, &server.agree(client)?]);
        Ok(Self {
            jwt,
            encryptor: Encryptor(Direction::new(key)?),
            decryptor: Decryptor(Direction::new(key)?),
        })
    }
}

/// Encrypts outgoing packets.
#[derive(Debug)]
pub(crate) struct Encryptor(Direction);

impl Encryptor {
    /// Appends the checksum to `data` (the bytes after 0xfe) and encrypts it all in place.
    /// Reserve [`CHECKSUM_BYTES`] spare bytes of capacity to keep this free of allocation.
    pub(crate) fn encrypt(&mut self, data: &mut Vec<u8>) -> Result<()> {
        let checksum = self.0.checksum(data);
        data.extend_from_slice(&checksum);
        self.0.stream.apply(data)
    }
}

/// Decrypts and checks incoming packets.
#[derive(Debug)]
pub(crate) struct Decryptor(Direction);

impl Decryptor {
    /// Decrypts `data` (the bytes after 0xfe) in place and checks its checksum.
    /// Returns the plaintext length; the checksum bytes follow it. A mismatch is an error, and
    /// the stream cannot continue after one (the caller disconnects).
    pub(crate) fn decrypt(&mut self, data: &mut [u8]) -> Result<usize> {
        self.0.stream.apply(data)?;
        let len = data
            .len()
            .checked_sub(CHECKSUM_BYTES)
            .ok_or(Error::Checksum)?;
        let (plaintext, sum) = data.split_at(len);
        let expected = self.0.checksum(plaintext);
        verify_slices_are_equal(&expected, sum).map_err(|_| Error::Checksum)?;
        Ok(len)
    }
}

/// One direction: the CTR stream, the packet counter and the key the checksums hash.
struct Direction {
    stream: Keystream,
    key: [u8; KEY_BYTES],
    counter: u64,
}

impl Direction {
    fn new(key: [u8; KEY_BYTES]) -> Result<Self> {
        Ok(Self {
            stream: Keystream::new(&key)?,
            key,
            counter: 0,
        })
    }

    /// The checksum of `plaintext` under the next counter value.
    fn checksum(&mut self, plaintext: &[u8]) -> [u8; CHECKSUM_BYTES] {
        let digest = sha256(&[&self.counter.to_le_bytes(), plaintext, &self.key]);
        self.counter = self.counter.wrapping_add(1);
        let mut out = [0_u8; CHECKSUM_BYTES];
        for (o, d) in out.iter_mut().zip(digest) {
            *o = d;
        }
        out
    }
}

impl fmt::Debug for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Direction")
            .field("counter", &self.counter)
            .finish_non_exhaustive()
    }
}

/// An AES-256-CTR keystream that continues across calls.
/// aws-lc's one-shot CTR restarts at a block boundary, so a call that ends inside a block keeps
/// that block's keystream for the next.
struct Keystream {
    key: EncryptingKey,
    /// The counter block of the next keystream block not yet started.
    counter: u128,
    /// Keystream of the block the last call ended in, and how much of it is used (0 = none).
    partial: [u8; BLOCK_BYTES],
    used: usize,
}

impl Keystream {
    fn new(key: &[u8; KEY_BYTES]) -> Result<Self> {
        let unbound = UnboundCipherKey::new(&AES_256, key).map_err(|_| Error::Internal)?;
        let mut iv = [0_u8; BLOCK_BYTES];
        for (i, k) in iv.iter_mut().zip(key.iter().take(IV_KEY_BYTES)) {
            *i = *k;
        }
        Ok(Self {
            key: EncryptingKey::ctr(unbound).map_err(|_| Error::Internal)?,
            counter: u128::from_be_bytes(iv) | IV_FIRST_COUNTER,
            partial: [0; BLOCK_BYTES],
            used: 0,
        })
    }

    fn xor_from_counter(&self, data: &mut [u8]) -> Result<()> {
        let iv = EncryptionContext::Iv128(FixedLength::from(self.counter.to_be_bytes()));
        self.key
            .less_safe_encrypt(data, iv)
            .map(|_| ())
            .map_err(|_| Error::Internal)
    }

    fn apply(&mut self, mut data: &mut [u8]) -> Result<()> {
        if self.used != 0 {
            let take = (BLOCK_BYTES - self.used).min(data.len());
            let (head, tail) = data.split_at_mut(take);
            for (b, k) in head.iter_mut().zip(self.partial.iter().skip(self.used)) {
                *b ^= k;
            }
            self.used = (self.used + take) % BLOCK_BYTES;
            data = tail;
        }
        if data.is_empty() {
            return Ok(());
        }
        self.xor_from_counter(data)?;
        self.counter = self
            .counter
            .wrapping_add((data.len() / BLOCK_BYTES) as u128);
        let rem = data.len() % BLOCK_BYTES;
        if rem != 0 {
            let mut partial = [0_u8; BLOCK_BYTES];
            self.xor_from_counter(&mut partial)?;
            self.partial = partial;
            self.used = rem;
            self.counter = self.counter.wrapping_add(1);
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct SaltClaims<'a> {
    salt: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde::Deserialize;

    use crate::crypto::{STD, Verifier, parse_jws, verify_jws};

    #[derive(Deserialize)]
    struct Step {
        plain: String,
        cipher: String,
    }

    /// The goldens in `testdata/encryption.json`.
    /// They were made with the pinned gophertunnel: fixed P-384 keys (SHA-384 of a seed as the
    /// scalar), a fixed salt, the key from gophertunnel's `encryptionKey` steps, and
    /// `packet.Encoder` batches with and without `EnableEncryption` (the bytes after 0xfe).
    /// `plain` and `cipher` are hex.
    #[derive(Deserialize)]
    struct Golden {
        server_pkcs8: String,
        client_pkcs8: String,
        client_spki: String,
        server_spki: String,
        salt: String,
        key: String,
        server_to_client: Vec<Step>,
        client_to_server: Vec<Step>,
    }

    fn from_hex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    fn golden() -> Golden {
        serde_json::from_str(include_str!("testdata/encryption.json")).unwrap()
    }

    fn keys(g: &Golden) -> (ServerKey, PublicKey) {
        let server = ServerKey::from_pkcs8(&STD.decode(&g.server_pkcs8, 1024).unwrap()).unwrap();
        assert_eq!(server.public_key_base64(), g.server_spki);
        (server, PublicKey::from_base64_spki(&g.client_spki).unwrap())
    }

    fn salt(g: &Golden) -> [u8; SALT_BYTES] {
        STD.decode(&g.salt, SALT_BYTES).unwrap().try_into().unwrap()
    }

    #[test]
    fn keys_and_ciphertexts_match_gophertunnel() {
        let g = golden();
        let (server, client) = keys(&g);
        let mut handshake = Handshake::with_salt(&server, &client, salt(&g)).unwrap();
        assert_eq!(handshake.encryptor.0.key.to_vec(), from_hex(&g.key));
        for step in &g.server_to_client {
            let mut data = from_hex(&step.plain);
            data.reserve(CHECKSUM_BYTES);
            let capacity = data.capacity();
            handshake.encryptor.encrypt(&mut data).unwrap();
            assert_eq!(data, from_hex(&step.cipher));
            assert_eq!(data.capacity(), capacity);
        }
        for step in &g.client_to_server {
            let mut data = from_hex(&step.cipher);
            let len = handshake.decryptor.decrypt(&mut data).unwrap();
            assert_eq!(data[..len], from_hex(&step.plain));
        }
    }

    #[test]
    fn the_handshake_jwt_carries_a_verifiable_salt() {
        let g = golden();
        let (server, client) = keys(&g);
        let handshake = Handshake::with_salt(&server, &client, salt(&g)).unwrap();
        let jws = parse_jws(&handshake.jwt, 64).unwrap();
        let x5u = jws.header.x5u.as_deref().unwrap();
        assert_eq!(x5u, g.server_spki);
        verify_jws(
            &jws,
            Verifier::Es384(&PublicKey::from_base64_spki(x5u).unwrap()),
        )
        .unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&jws.payload).unwrap();
        assert_eq!(claims["salt"], RAW_STD.encode(&salt(&g)));
        // The client derives the same key from its own private key and the server's x5u.
        let client_key =
            ServerKey::from_pkcs8(&STD.decode(&g.client_pkcs8, 1024).unwrap()).unwrap();
        let server_public = PublicKey::from_base64_spki(x5u).unwrap();
        let theirs = sha256(&[&salt(&g), &client_key.agree(&server_public).unwrap()]);
        assert_eq!(theirs.to_vec(), from_hex(&g.key));
    }

    #[test]
    fn tampering_and_counter_skew_fail_the_checksum() {
        let g = golden();
        let (server, client) = keys(&g);
        let fresh = || {
            Handshake::with_salt(&server, &client, salt(&g))
                .unwrap()
                .decryptor
        };
        let first = from_hex(&g.client_to_server[0].cipher);
        for bit in 0..first.len() * 8 {
            let mut data = first.clone();
            data[bit / 8] ^= 1 << (bit % 8);
            assert!(
                matches!(fresh().decrypt(&mut data), Err(Error::Checksum)),
                "bit {bit}"
            );
        }
        // The second packet decrypted as if it were the first: wrong counter and keystream.
        let mut second = from_hex(&g.client_to_server[1].cipher);
        assert!(fresh().decrypt(&mut second).is_err());
        // Right keystream position, wrong counter.
        let mut decryptor = fresh();
        decryptor.0.counter = 1;
        let mut data = first.clone();
        assert!(matches!(decryptor.decrypt(&mut data), Err(Error::Checksum)));
        assert!(matches!(
            fresh().decrypt(&mut [0_u8; 7]),
            Err(Error::Checksum)
        ));
    }

    #[test]
    fn the_stream_continues_across_any_split() {
        let key = [7_u8; KEY_BYTES];
        let mut whole = vec![0_u8; 100];
        Keystream::new(&key).unwrap().apply(&mut whole).unwrap();
        for split in [1, 5, 15, 16, 17, 33, 99] {
            let mut stream = Keystream::new(&key).unwrap();
            let mut parts = vec![0_u8; 100];
            let (a, b) = parts.split_at_mut(split);
            stream.apply(a).unwrap();
            stream.apply(&mut []).unwrap();
            for chunk in b.chunks_mut(3) {
                stream.apply(chunk).unwrap();
            }
            assert_eq!(parts, whole, "split {split}");
        }
    }

    #[test]
    fn handshakes_use_fresh_salts() {
        let g = golden();
        let (server, client) = keys(&g);
        let a = Handshake::new(&server, &client).unwrap();
        let b = Handshake::new(&server, &client).unwrap();
        assert_ne!(a.encryptor.0.key, b.encryptor.0.key);
    }
}
