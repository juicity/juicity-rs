use chacha20poly1305::aead::KeyInit;
use chacha20poly1305::ChaCha20Poly1305;
use sha2::{Digest, Sha256};

/// Generate a SHA-256 hash chain from certificate raw bytes
/// (same algorithm as the Go version: hash each cert, then hash accumulated result)
pub fn generate_cert_chain_hash(raw_certs: &[&[u8]]) -> Vec<u8> {
    // Use a fixed-size array for intermediate results (SHA-256 output is always 32 bytes)
    // to avoid heap allocation per iteration.
    let mut chain: Option<[u8; 32]> = None;
    for cert in raw_certs {
        let cert_hash: [u8; 32] = Sha256::digest(cert).into();
        chain = Some(match chain {
            None => cert_hash,
            Some(prev) => {
                let mut combined = [0u8; 64];
                combined[..32].copy_from_slice(&prev);
                combined[32..].copy_from_slice(&cert_hash);
                Sha256::digest(&combined).into()
            }
        });
    }
    chain.map(|h| h.to_vec()).unwrap_or_default()
}

/// Deduplicate a slice while preserving order
#[allow(dead_code)]
pub fn deduplicate<T: Eq + std::hash::Hash + Clone>(list: &[T]) -> Vec<T> {
    let mut seen = std::collections::HashSet::new();
    list.iter()
        .filter(|item| seen.insert((*item).clone()))
        .cloned()
        .collect()
}

/// AES-128-GCM encryption/decryption for shadowsocks-style underlay UDP
pub mod aead {
    use aes_gcm::aead::{AeadInPlace, KeyInit};
    use aes_gcm::{Aes128Gcm, Nonce};
    use hkdf::Hkdf;
    use rand::RngCore;
    use sha2::Sha256;

    /// Derive a key using standard HKDF with SHA-256
    pub fn derive_key(password: &[u8], salt: &[u8], info: &[u8]) -> [u8; 16] {
        let hkdf = Hkdf::<Sha256>::new(Some(salt), password);
        let mut key = [0u8; 16];
        hkdf.expand(info, &mut key)
            .expect("HKDF expand should not fail with valid output length");
        key
    }

    /// Encrypt plaintext using AES-128-GCM in-place, appending tag to `buffer`.
    /// `buffer` must contain the plaintext and have additional capacity for the tag (16 bytes).
    /// Returns the number of bytes written (plaintext_len + tag_len).
    pub fn encrypt_in_place(
        key: &[u8; 16],
        buffer: &mut Vec<u8>,
        nonce: &[u8; 12],
    ) -> Result<usize, aes_gcm::Error> {
        let key_typed = aes_gcm::Key::<Aes128Gcm>::from_slice(key);
        let cipher = Aes128Gcm::new(key_typed);
        let nonce_typed = Nonce::from_slice(nonce);
        cipher.encrypt_in_place(nonce_typed, b"", buffer)?;
        Ok(buffer.len())
    }

    /// Decrypt ciphertext using AES-128-GCM in-place.
    /// `buffer` must contain ciphertext + tag (last 16 bytes).
    /// On success, the tag is removed and the buffer contains only plaintext.
    /// Returns the plaintext length.
    pub fn decrypt_in_place(
        key: &[u8; 16],
        buffer: &mut Vec<u8>,
        nonce: &[u8; 12],
    ) -> Result<usize, aes_gcm::Error> {
        let key_typed = aes_gcm::Key::<Aes128Gcm>::from_slice(key);
        let cipher = Aes128Gcm::new(key_typed);
        let nonce_typed = Nonce::from_slice(nonce);
        // AeadInPlace::decrypt_in_place removes the tag and shrinks the buffer to plaintext.
        cipher.decrypt_in_place(nonce_typed, b"", buffer)?;
        Ok(buffer.len())
    }

    /// Encrypt plaintext using AES-128-GCM (returns a new Vec, convenience wrapper)
    pub fn encrypt(
        key: &[u8; 16],
        plaintext: &[u8],
        nonce: &[u8; 12],
    ) -> Result<Vec<u8>, aes_gcm::Error> {
        let mut buffer = plaintext.to_vec();
        encrypt_in_place(key, &mut buffer, nonce)?;
        Ok(buffer)
    }

    /// Decrypt ciphertext using AES-128-GCM (returns a new Vec, convenience wrapper)
    pub fn decrypt(key: &[u8; 16], ciphertext: &[u8], nonce: &[u8; 12]) -> Option<Vec<u8>> {
        let mut buffer = ciphertext.to_vec();
        decrypt_in_place(key, &mut buffer, nonce).ok()?;
        // `decrypt_in_place` already removes the 16-byte tag via AeadInPlace,
        // so `buffer` now contains only plaintext — no further truncation needed.
        Some(buffer)
    }

    /// Generate random bytes
    pub fn random_bytes<const N: usize>() -> [u8; N] {
        let mut buf = [0u8; N];
        rand::thread_rng().fill_bytes(&mut buf);
        buf
    }
}

/// A pre-computed ChaCha20Poly1305 cipher for a given subkey.
/// Avoids re-deriving the subkey and re-creating the cipher for every packet.
#[derive(Clone)]
pub struct UnderlayCipher {
    cipher: ChaCha20Poly1305,
}

impl UnderlayCipher {
    /// Create a new UnderlayCipher from a pre-derived subkey.
    pub fn from_subkey(subkey: &[u8; 32]) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(subkey.into()),
        }
    }

    /// Decrypt a packet in-place.
    /// `packet` should be [salt(32)][ciphertext+tag].
    /// On success, the buffer is shrunk to contain only the plaintext (no allocation).
    pub fn decrypt_in_place(&self, packet: &mut Vec<u8>) -> anyhow::Result<()> {
        use chacha20poly1305::aead::AeadInPlace;
        use crate::crypto::juicity_underlay::SALT_LEN;

        if packet.len() <= SALT_LEN {
            anyhow::bail!("underlay packet too short to contain salt");
        }

        let nonce = chacha20poly1305::Nonce::from_slice(&[0u8; 12]);
        // drain(..SALT_LEN) removes the salt prefix by shifting bytes left in-place —
        // no heap allocation, unlike split_off which creates a second Vec.
        packet.drain(..SALT_LEN);
        self.cipher
            .decrypt_in_place(nonce, b"", packet)
            .map_err(|e| anyhow::anyhow!("underlay decrypt failed: {:?}", e))?;
        // AeadInPlace removes the tag: packet now contains only plaintext.
        Ok(())
    }

    /// Encrypt a packet in-place, prepending salt.
    /// `plaintext` contains the data to encrypt; on return it holds [salt(32)][ciphertext+tag].
    ///
    /// Optimized to pre-allocate the full capacity upfront, avoiding multiple Vec resizes:
    /// - `encrypt_in_place` appends a 16-byte AEAD tag
    /// - Then we prepend a 32-byte salt
    /// By reserving `plaintext.len() + SALT_LEN + TAG_LEN` upfront, we ensure a single allocation.
    pub fn encrypt_in_place(&self, plaintext: &mut Vec<u8>, salt: &[u8; 32]) -> anyhow::Result<()> {
        use chacha20poly1305::aead::AeadInPlace;
        use crate::crypto::juicity_underlay::{SALT_LEN, TAG_LEN};

        let nonce = chacha20poly1305::Nonce::from_slice(&[0u8; 12]);

        // Pre-allocate full capacity: plaintext + salt + AEAD tag — single allocation.
        plaintext.reserve(SALT_LEN + TAG_LEN);

        // Step 1: encrypt plaintext in-place (appends 16-byte tag).
        self.cipher
            .encrypt_in_place(nonce, b"", plaintext)
            .map_err(|e| anyhow::anyhow!("underlay encrypt failed: {:?}", e))?;

        // Step 2: prepend salt by shifting ciphertext right by SALT_LEN.
        let ct_len = plaintext.len();
        plaintext.resize(ct_len + SALT_LEN, 0);
        plaintext.copy_within(..ct_len, SALT_LEN);
        plaintext[..SALT_LEN].copy_from_slice(salt);
        Ok(())
    }
}

/// Juicity underlay UDP crypto compatible with upstream outbound/shadowsocks usage:
/// subkey = HKDF-SHA1(master_key=psk, salt, info="juicity-reused-info"),
/// cipher = chacha20-poly1305, nonce = all zero.
pub mod juicity_underlay {
    use chacha20poly1305::aead::Aead;
    use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
    use hkdf::Hkdf;
    use rand::RngCore;
    use sha1::Sha1;

    pub const SALT_LEN: usize = 32;
    pub const KEY_LEN: usize = 32;
    pub const TAG_LEN: usize = 16;
    pub const REUSED_INFO: &[u8] = b"juicity-reused-info";

    /// Derive a subkey from PSK and salt using HKDF-SHA1.
    /// This is kept public so callers can cache the result.
    pub fn derive_subkey(psk: &[u8], salt: &[u8; SALT_LEN]) -> anyhow::Result<[u8; KEY_LEN]> {
        anyhow::ensure!(
            psk.len() == KEY_LEN,
            "invalid underlay psk length: expected {}, got {}",
            KEY_LEN,
            psk.len()
        );

        let hkdf = Hkdf::<Sha1>::new(Some(salt), psk);
        let mut subkey = [0u8; KEY_LEN];
        hkdf.expand(REUSED_INFO, &mut subkey)
            .map_err(|_| anyhow::anyhow!("hkdf expand failed for underlay subkey"))?;
        Ok(subkey)
    }

    pub fn generate_underlay_salt() -> [u8; SALT_LEN] {
        let mut salt = [0u8; SALT_LEN];
        // Keep this behavior aligned with upstream implementation.
        salt[0] = 0;
        salt[1] = 0;
        rand::thread_rng().fill_bytes(&mut salt[2..]);
        salt
    }

    pub fn decrypt_udp(psk: &[u8], packet: &[u8]) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            packet.len() >= SALT_LEN + TAG_LEN,
            "underlay packet too short: {}",
            packet.len()
        );

        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&packet[..SALT_LEN]);
        let ciphertext = &packet[SALT_LEN..];

        let subkey = derive_subkey(psk, &salt)?;
        let cipher = ChaCha20Poly1305::new((&subkey).into());
        let nonce = Nonce::from_slice(&[0u8; 12]);

        cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| anyhow::anyhow!("underlay decrypt failed: {:?}", e))
    }

    pub fn encrypt_udp(psk: &[u8], plaintext: &[u8], salt: &[u8; SALT_LEN]) -> anyhow::Result<Vec<u8>> {
        let subkey = derive_subkey(psk, salt)?;
        let cipher = ChaCha20Poly1305::new((&subkey).into());
        let nonce = Nonce::from_slice(&[0u8; 12]);

        let mut out = Vec::with_capacity(SALT_LEN + plaintext.len() + TAG_LEN);
        out.extend_from_slice(salt);
        out.extend(
            cipher
                .encrypt(nonce, plaintext)
                .map_err(|e| anyhow::anyhow!("underlay encrypt failed: {:?}", e))?,
        );
        Ok(out)
    }
}
