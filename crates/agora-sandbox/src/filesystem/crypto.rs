use anyhow::{Context, Result, bail};
use base64::Engine;
use ring::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    digest, pbkdf2,
    rand::{SecureRandom, SystemRandom},
};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::num::NonZeroU32;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use uuid::Uuid;

const TAG_SIZE: usize = 16;
const NAME_NONCE_SIZE: usize = 12;
const NAME_AAD: &[u8] = b"AGORA-FILENAME\0\x01";
pub(super) const ENCRYPTED_NAME_PREFIX: &str = "enc_";
const FILESYSTEM_NAME_MAX: usize = 255;
const PBKDF2_ITERATIONS: u32 = 100_000;

mod content;

#[cfg(test)]
pub(super) use content::{CIPHERTEXT_BLOCK_SIZE, CONTENT_HEADER_SIZE};
pub(crate) use content::{EncryptedFile, PLAINTEXT_BLOCK_SIZE};

#[derive(Clone)]
pub(crate) struct FileCipher {
    key: LessSafeKey,
    key_material: [u8; 32],
    key_id: String,
}

impl std::fmt::Debug for FileCipher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("FileCipher").finish_non_exhaustive()
    }
}

impl FileCipher {
    pub(crate) fn derive(passphrase: &[u8], salt: &[u8]) -> Result<Self> {
        if passphrase.is_empty() {
            bail!("sandbox filesystem key cannot be empty");
        }
        if salt.len() < 16 {
            bail!("sandbox filesystem salt must contain at least 16 bytes");
        }
        let iterations = NonZeroU32::new(PBKDF2_ITERATIONS).expect("iteration count is non-zero");
        let mut key = [0_u8; 32];
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            iterations,
            salt,
            passphrase,
            &mut key,
        );
        Self::from_key(&key)
    }

    pub(crate) fn from_key(key: &[u8]) -> Result<Self> {
        let key_material: [u8; 32] = key
            .try_into()
            .map_err(|_| anyhow::anyhow!("sandbox filesystem cipher key must contain 32 bytes"))?;
        let key_id = Self::hex(digest::digest(&digest::SHA256, key).as_ref());
        let key = UnboundKey::new(&aead::AES_256_GCM, &key_material)
            .map_err(|_| anyhow::anyhow!("failed to initialize filesystem cipher"))?;
        Ok(Self {
            key: LessSafeKey::new(key),
            key_material,
            key_id,
        })
    }

    pub(crate) fn key_material(&self) -> &[u8; 32] {
        &self.key_material
    }

    pub(crate) fn key_id(&self) -> &str {
        &self.key_id
    }

    pub(crate) fn encrypt_name(&self, plaintext: &[u8]) -> Result<String> {
        let mut nonce = [0_u8; NAME_NONCE_SIZE];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| anyhow::anyhow!("failed to generate filesystem filename nonce"))?;
        let mut sealed = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(NAME_AAD),
                &mut sealed,
            )
            .map_err(|_| anyhow::anyhow!("failed to encrypt filesystem filename"))?;
        let mut payload = Vec::with_capacity(NAME_NONCE_SIZE + sealed.len());
        payload.extend_from_slice(&nonce);
        payload.extend_from_slice(&sealed);
        let encoded = format!(
            "{ENCRYPTED_NAME_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
        );
        if encoded.len() > FILESYSTEM_NAME_MAX {
            return Err(std::io::Error::from_raw_os_error(libc::ENAMETOOLONG).into());
        }
        Ok(encoded)
    }

    pub(crate) fn decrypt_name(&self, encoded: &str) -> Result<Vec<u8>> {
        let encoded = encoded
            .strip_prefix(ENCRYPTED_NAME_PREFIX)
            .context("filesystem filename is not encrypted")?;
        let mut payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .context("filesystem filename ciphertext is not valid Base64")?;
        if payload.len() < NAME_NONCE_SIZE + TAG_SIZE {
            bail!("filesystem filename ciphertext is incomplete");
        }
        let nonce: [u8; NAME_NONCE_SIZE] = payload[..NAME_NONCE_SIZE]
            .try_into()
            .expect("filename nonce length was checked");
        let opened = self
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(NAME_AAD),
                &mut payload[NAME_NONCE_SIZE..],
            )
            .map_err(|_| anyhow::anyhow!("filesystem filename authentication failed"))?;
        Ok(opened.to_vec())
    }

    pub(crate) fn open_file(&self, path: &Path) -> Result<EncryptedFile> {
        EncryptedFile::open(path, &self.key_material)
    }

    pub(crate) fn create_file(&self, path: &Path) -> Result<EncryptedFile> {
        EncryptedFile::create(path, &self.key_material)
    }

    pub(crate) fn encrypt(&self, plaintext: &mut File, destination: &Path) -> Result<()> {
        let parent = destination
            .parent()
            .context("encrypted filesystem destination has no parent")?;
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create encrypted filesystem directory {}",
                parent.display()
            )
        })?;
        let temporary = parent.join(format!(".agora-encrypted-{}.tmp", Uuid::new_v4().simple()));
        let result = (|| {
            let mut encrypted = self.create_file(&temporary).with_context(|| {
                format!(
                    "failed to create encrypted filesystem file {}",
                    temporary.display()
                )
            })?;
            plaintext.seek(SeekFrom::Start(0))?;
            let mut offset = 0_u64;
            let mut buffer = vec![0_u8; PLAINTEXT_BLOCK_SIZE * 16];
            loop {
                let read = plaintext.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                encrypted.write_at(&buffer[..read], offset)?;
                offset = offset
                    .checked_add(read as u64)
                    .context("encrypted filesystem file is too large")?;
            }
            encrypted
                .backing_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .context("failed to secure encrypted filesystem file")?;
            encrypted
                .sync_all()
                .context("failed to sync encrypted filesystem file")?;
            fs::rename(&temporary, destination).with_context(|| {
                format!(
                    "failed to publish encrypted filesystem file {}",
                    destination.display()
                )
            })?;
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .with_context(|| {
                    format!(
                        "failed to sync encrypted filesystem directory {}",
                        parent.display()
                    )
                })
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub(crate) fn decrypt(&self, source: &Path, plaintext: &mut File) -> Result<()> {
        let encrypted = self.open_file(source).with_context(|| {
            format!(
                "failed to open encrypted filesystem file {}",
                source.display()
            )
        })?;
        let mut verified =
            tempfile::tempfile().context("failed to create anonymous filesystem plaintext file")?;
        let mut offset = 0_u64;
        let mut buffer = vec![0_u8; PLAINTEXT_BLOCK_SIZE * 16];
        while offset < encrypted.len() {
            let read = encrypted.read_at(&mut buffer, offset).with_context(|| {
                format!(
                    "failed to decrypt encrypted filesystem file {}",
                    source.display()
                )
            })?;
            if read == 0 {
                break;
            }
            verified.write_all(&buffer[..read])?;
            offset += read as u64;
        }
        verified.seek(SeekFrom::Start(0))?;
        plaintext.set_len(0)?;
        plaintext.seek(SeekFrom::Start(0))?;
        std::io::copy(&mut verified, plaintext)?;
        plaintext.seek(SeekFrom::Start(0))?;
        Ok(())
    }

    pub(crate) fn overwrite(&self, plaintext: &mut File, destination: &Path) -> Result<()> {
        let mut encrypted = self.open_file(destination)?;
        encrypted.set_len(0)?;
        plaintext.seek(SeekFrom::Start(0))?;
        let mut offset = 0_u64;
        let mut buffer = vec![0_u8; PLAINTEXT_BLOCK_SIZE * 16];
        loop {
            let read = plaintext.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            encrypted.write_at(&buffer[..read], offset)?;
            offset = offset
                .checked_add(read as u64)
                .context("encrypted filesystem file is too large")?;
        }
        encrypted.set_len(offset)?;
        encrypted.sync_all()
    }

    fn hex(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            output.push(HEX[(byte >> 4) as usize] as char);
            output.push(HEX[(byte & 0x0f) as usize] as char);
        }
        output
    }
}

#[cfg(test)]
mod tests;
