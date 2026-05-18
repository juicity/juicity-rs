use chacha20poly1305::aead::KeyInit;
use chacha20poly1305::ChaCha20Poly1305;
use sha2::{Digest, Sha256};

/// Generate a SHA-256 hash chain from certificate raw bytes
/// (same algorithm as the Go version: hash each cert, then hash accumulated result)
pub fn generate_cert_chain_hash(raw_certs: &[&[u8]]) -> Vec<u8> {
    let mut chain_hash = Vec::new();
    for cert in raw_certs {
        let cert_hash = Sha256::digest(cert);
        if chain_hash.is_empty() {
            chain_hash = cert_hash.to_vec();
        } else {
            chain_hash.extend_from_slice(&cert_hash);
            chain_hash = Sha256::digest(&chain_hash).to_vec();
        }
    }
    chain_hash
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
    /// Returns the plaintext length on success.
    pub fn decrypt_in_place(
        key: &[u8; 16],
        buffer: &mut Vec<u8>,
        nonce: &[u8; 12],
    ) -> Result<usize, aes_gcm::Error> {
        let key_typed = aes_gcm::Key::<Aes128Gcm>::from_slice(key);
        let cipher = Aes128Gcm::new(key_typed);
        let nonce_typed = Nonce::from_slice(nonce);
        cipher.decrypt_in_place(nonce_typed, b"", buffer)?;
        let pt_len = buffer.len() - 16; // tag is 16 bytes for AES-128-GCM
        Ok(pt_len)
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
        buffer.truncate(buffer.len() - 16);
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
    /// On success, the packet buffer is replaced with plaintext.
    pub fn decrypt_in_place(&self, packet: &mut Vec<u8>) -> anyhow::Result<()> {
        use chacha20poly1305::aead::AeadInPlace;
        use crate::crypto::juicity_underlay::SALT_LEN;

        let nonce = chacha20poly1305::Nonce::from_slice(&[0u8; 12]);
        // Remove the salt prefix, decrypt in-place.
        // We use `Vec::split_off` to get a Vec<u8> for the ciphertext portion,
        // because AeadInPlace requires &mut dyn Buffer (implemented for Vec<u8> but not [u8]).
        let mut ciphertext = packet.split_off(SALT_LEN);
        self.cipher
            .decrypt_in_place(nonce, b"", &mut ciphertext)
            .map_err(|e| anyhow::anyhow!("underlay decrypt failed: {:?}", e))?;
        // Truncate the tag (16 bytes for ChaCha20Poly1305)
        let pt_len = ciphertext.len() - 16;
        ciphertext.truncate(pt_len);
        // Replace packet with plaintext
        *packet = ciphertext;
        Ok(())
    }

    /// Encrypt a packet in-place, prepending salt.
    /// `plaintext` is the data to encrypt. The salt is prepended and tag appended.
    pub fn encrypt_in_place(&self, plaintext: &mut Vec<u8>, salt: &[u8; 32]) -> anyhow::Result<()> {
        use chacha20poly1305::aead::AeadInPlace;
        use crate::crypto::juicity_underlay::SALT_LEN;

        let nonce = chacha20poly1305::Nonce::from_slice(&[0u8; 12]);
        // Reserve space for salt at the front
        let original_len = plaintext.len();
        plaintext.reserve(SALT_LEN + 16);
        // Shift content right by SALT_LEN to make room for salt
        plaintext.resize(original_len + SALT_LEN, 0);
        plaintext.copy_within(..original_len, SALT_LEN);
        plaintext[..SALT_LEN].copy_from_slice(salt);

        // Encrypt in-place (the ciphertext portion starts after salt).
        // We use split_off to get a Vec<u8> for the ciphertext portion,
        // because AeadInPlace requires &mut dyn Buffer (implemented for Vec<u8> but not [u8]).
        let mut ciphertext = plaintext.split_off(SALT_LEN);
        self.cipher
            .encrypt_in_place(nonce, b"", &mut ciphertext)
            .map_err(|e| anyhow::anyhow!("underlay encrypt failed: {:?}", e))?;
        // Reassemble: salt + encrypted ciphertext
        plaintext.extend_from_slice(&ciphertext);
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
