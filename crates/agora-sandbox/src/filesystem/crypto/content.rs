use super::TAG_SIZE;
use anyhow::{Context, Result, bail};
use ring::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    hkdf,
    rand::{SecureRandom, SystemRandom},
};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;

const CONTENT_MAGIC: &[u8; 8] = b"AGORAFS\0";
const CONTENT_VERSION: u8 = 2;
const FILE_ID_SIZE: usize = 16;
const NONCE_SIZE: usize = 12;
const HEADER_SLOT_SIZE: usize = 80;
const HEADER_AUTHENTICATED_SIZE: usize = 52;
const HEADER_NONCE_OFFSET: usize = HEADER_AUTHENTICATED_SIZE;
const HEADER_TAG_OFFSET: usize = HEADER_NONCE_OFFSET + NONCE_SIZE;
const HEADER_SLOTS: usize = 2;
const BLOCK_NONCE_OFFSET: usize = 0;
const BLOCK_CIPHERTEXT_OFFSET: usize = NONCE_SIZE;
const CONTENT_KEY_INFO: &[u8] = b"AGORA-FILESYSTEM-CONTENT-KEY\0\x02";
const HEADER_AAD: &[u8] = b"AGORA-FILESYSTEM-HEADER\0\x02";
const BLOCK_AAD: &[u8] = b"AGORA-FILESYSTEM-BLOCK\0\x02";

pub(crate) const PLAINTEXT_BLOCK_SIZE: usize = 4 * 1024;
pub(crate) const CIPHERTEXT_BLOCK_SIZE: usize = NONCE_SIZE + PLAINTEXT_BLOCK_SIZE + TAG_SIZE;
pub(crate) const CONTENT_HEADER_SIZE: usize = HEADER_SLOT_SIZE * HEADER_SLOTS;

#[derive(Clone, Copy)]
struct Header {
    file_id: [u8; FILE_ID_SIZE],
    generation: u64,
    logical_len: u64,
    slot: u8,
}

pub(crate) struct EncryptedFile {
    file: File,
    key: LessSafeKey,
    file_id: [u8; FILE_ID_SIZE],
    generation: u64,
    logical_len: u64,
    active_header: u8,
}

impl EncryptedFile {
    pub(super) fn create(path: &Path, master_key: &[u8; 32]) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        let mut file_id = [0_u8; FILE_ID_SIZE];
        SystemRandom::new()
            .fill(&mut file_id)
            .map_err(|_| anyhow::anyhow!("failed to generate encrypted filesystem file ID"))?;
        let key = derive_file_key(master_key, &file_id)?;
        let mut encrypted = Self {
            file,
            key,
            file_id,
            generation: 1,
            logical_len: 0,
            active_header: 0,
        };
        encrypted.write_header(0, 1, 0)?;
        encrypted.write_header(1, 0, 0)?;
        encrypted.file.set_len(CONTENT_HEADER_SIZE as u64)?;
        Ok(encrypted)
    }

    pub(super) fn open(path: &Path, master_key: &[u8; 32]) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut selected: Option<(Header, LessSafeKey)> = None;
        for slot in 0..HEADER_SLOTS as u8 {
            let mut bytes = [0_u8; HEADER_SLOT_SIZE];
            if read_exact_at(&file, &mut bytes, u64::from(slot) * HEADER_SLOT_SIZE as u64).is_err()
            {
                continue;
            }
            let Ok((header, key)) = decode_header(&bytes, slot, master_key) else {
                continue;
            };
            if selected
                .as_ref()
                .is_none_or(|(current, _)| header.generation > current.generation)
            {
                selected = Some((header, key));
            }
        }
        let (header, key) = selected
            .context("encrypted filesystem header is incomplete or failed authentication")?;
        let encrypted = Self {
            file,
            key,
            file_id: header.file_id,
            generation: header.generation,
            logical_len: header.logical_len,
            active_header: header.slot,
        };
        let expected = encrypted.expected_physical_len(header.logical_len)?;
        let actual = encrypted.file.metadata()?.len();
        if actual > expected {
            encrypted.file.set_len(expected)?;
        }
        Ok(encrypted)
    }

    pub(crate) fn len(&self) -> u64 {
        self.logical_len
    }

    pub(crate) fn read_at(&self, output: &mut [u8], offset: u64) -> Result<usize> {
        if output.is_empty() || offset >= self.logical_len {
            return Ok(0);
        }
        let available = self.logical_len - offset;
        let requested = usize::try_from(available.min(output.len() as u64))
            .context("encrypted filesystem read length does not fit in memory")?;
        let mut completed = 0;
        let mut block = [0_u8; PLAINTEXT_BLOCK_SIZE];
        while completed < requested {
            let position = offset + completed as u64;
            let index = position / PLAINTEXT_BLOCK_SIZE as u64;
            let within = (position % PLAINTEXT_BLOCK_SIZE as u64) as usize;
            self.read_block(index, &mut block)?;
            let copied = (PLAINTEXT_BLOCK_SIZE - within).min(requested - completed);
            output[completed..completed + copied].copy_from_slice(&block[within..within + copied]);
            completed += copied;
        }
        Ok(completed)
    }

    pub(crate) fn write_at(&mut self, input: &[u8], offset: u64) -> Result<usize> {
        if input.is_empty() {
            return Ok(0);
        }
        let end = offset
            .checked_add(input.len() as u64)
            .context("encrypted filesystem file is too large")?;
        let mut completed = 0;
        let mut block = [0_u8; PLAINTEXT_BLOCK_SIZE];
        while completed < input.len() {
            let position = offset + completed as u64;
            let index = position / PLAINTEXT_BLOCK_SIZE as u64;
            let within = (position % PLAINTEXT_BLOCK_SIZE as u64) as usize;
            let copied = (PLAINTEXT_BLOCK_SIZE - within).min(input.len() - completed);
            if within != 0 || copied != PLAINTEXT_BLOCK_SIZE {
                self.read_block(index, &mut block)?;
            } else {
                block.fill(0);
            }
            block[within..within + copied].copy_from_slice(&input[completed..completed + copied]);
            self.write_block(index, &block)?;
            completed += copied;
        }
        if end > self.logical_len {
            self.commit_length(end)?;
        }
        Ok(completed)
    }

    pub(crate) fn set_len(&mut self, length: u64) -> Result<()> {
        if length == self.logical_len {
            return Ok(());
        }
        if length < self.logical_len && !length.is_multiple_of(PLAINTEXT_BLOCK_SIZE as u64) {
            let index = length / PLAINTEXT_BLOCK_SIZE as u64;
            let retained = (length % PLAINTEXT_BLOCK_SIZE as u64) as usize;
            let mut block = [0_u8; PLAINTEXT_BLOCK_SIZE];
            self.read_block(index, &mut block)?;
            block[retained..].fill(0);
            self.write_block(index, &block)?;
        }
        self.commit_length(length)?;
        let physical = self.expected_physical_len(length)?;
        if self.file.metadata()?.len() > physical {
            self.file.set_len(physical)?;
        }
        Ok(())
    }

    pub(crate) fn sync_all(&mut self) -> Result<()> {
        self.commit_length(self.logical_len)?;
        self.file.sync_all().map_err(Into::into)
    }

    pub(crate) fn backing_file(&self) -> &File {
        &self.file
    }

    fn commit_length(&mut self, logical_len: u64) -> Result<()> {
        let generation = self
            .generation
            .checked_add(1)
            .context("encrypted filesystem header generation overflowed")?;
        let slot = 1 - self.active_header;
        self.write_header(slot, generation, logical_len)?;
        self.generation = generation;
        self.logical_len = logical_len;
        self.active_header = slot;
        Ok(())
    }

    fn write_header(&mut self, slot: u8, generation: u64, logical_len: u64) -> Result<()> {
        let bytes = encode_header(&self.key, self.file_id, slot, generation, logical_len)?;
        write_all_at(
            &self.file,
            &bytes,
            u64::from(slot) * HEADER_SLOT_SIZE as u64,
        )
    }

    fn read_block(&self, index: u64, plaintext: &mut [u8; PLAINTEXT_BLOCK_SIZE]) -> Result<()> {
        let mut sealed = [0_u8; CIPHERTEXT_BLOCK_SIZE];
        let offset = block_offset(index)?;
        match read_slot_at(&self.file, &mut sealed, offset)? {
            SlotRead::Missing => {
                plaintext.fill(0);
                return Ok(());
            }
            SlotRead::Present => {}
        }
        if sealed.iter().all(|byte| *byte == 0) {
            plaintext.fill(0);
            return Ok(());
        }
        let nonce: [u8; NONCE_SIZE] = sealed[BLOCK_NONCE_OFFSET..BLOCK_CIPHERTEXT_OFFSET]
            .try_into()
            .expect("block nonce range has a fixed size");
        let aad = block_aad(self.file_id, index);
        let opened = self
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad.as_slice()),
                &mut sealed[BLOCK_CIPHERTEXT_OFFSET..],
            )
            .map_err(|_| anyhow::anyhow!("encrypted filesystem block authentication failed"))?;
        if opened.len() != PLAINTEXT_BLOCK_SIZE {
            bail!("encrypted filesystem block has an invalid plaintext size");
        }
        plaintext.copy_from_slice(opened);
        Ok(())
    }

    fn write_block(&self, index: u64, plaintext: &[u8; PLAINTEXT_BLOCK_SIZE]) -> Result<()> {
        let mut nonce = [0_u8; NONCE_SIZE];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| anyhow::anyhow!("failed to generate encrypted filesystem block nonce"))?;
        let mut ciphertext = plaintext.to_vec();
        let aad = block_aad(self.file_id, index);
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad.as_slice()),
                &mut ciphertext,
            )
            .map_err(|_| anyhow::anyhow!("failed to encrypt filesystem block"))?;
        let mut sealed = Vec::with_capacity(CIPHERTEXT_BLOCK_SIZE);
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);
        debug_assert_eq!(sealed.len(), CIPHERTEXT_BLOCK_SIZE);
        write_all_at(&self.file, &sealed, block_offset(index)?)
    }

    fn expected_physical_len(&self, logical_len: u64) -> Result<u64> {
        let blocks = logical_len.div_ceil(PLAINTEXT_BLOCK_SIZE as u64);
        blocks
            .checked_mul(CIPHERTEXT_BLOCK_SIZE as u64)
            .and_then(|body| (CONTENT_HEADER_SIZE as u64).checked_add(body))
            .context("encrypted filesystem file is too large")
    }
}

fn derive_file_key(master_key: &[u8; 32], file_id: &[u8; FILE_ID_SIZE]) -> Result<LessSafeKey> {
    struct KeyLength;
    impl hkdf::KeyType for KeyLength {
        fn len(&self) -> usize {
            32
        }
    }

    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, CONTENT_KEY_INFO);
    let prk = salt.extract(master_key);
    let info = [CONTENT_KEY_INFO, file_id.as_slice()];
    let okm = prk
        .expand(&info, KeyLength)
        .map_err(|_| anyhow::anyhow!("failed to derive encrypted filesystem file key"))?;
    let mut key = [0_u8; 32];
    okm.fill(&mut key)
        .map_err(|_| anyhow::anyhow!("failed to derive encrypted filesystem file key"))?;
    let key = UnboundKey::new(&aead::AES_256_GCM, &key)
        .map_err(|_| anyhow::anyhow!("failed to initialize encrypted filesystem file key"))?;
    Ok(LessSafeKey::new(key))
}

fn encode_header(
    key: &LessSafeKey,
    file_id: [u8; FILE_ID_SIZE],
    slot: u8,
    generation: u64,
    logical_len: u64,
) -> Result<[u8; HEADER_SLOT_SIZE]> {
    let mut header = [0_u8; HEADER_SLOT_SIZE];
    header[..CONTENT_MAGIC.len()].copy_from_slice(CONTENT_MAGIC);
    header[8] = CONTENT_VERSION;
    header[9] = slot;
    header[16..32].copy_from_slice(&file_id);
    header[32..40].copy_from_slice(&generation.to_be_bytes());
    header[40..48].copy_from_slice(&logical_len.to_be_bytes());
    let mut nonce = [0_u8; NONCE_SIZE];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("failed to generate encrypted filesystem header nonce"))?;
    header[HEADER_NONCE_OFFSET..HEADER_TAG_OFFSET].copy_from_slice(&nonce);
    let aad = header_aad(&header[..HEADER_AUTHENTICATED_SIZE]);
    let mut tag = Vec::with_capacity(TAG_SIZE);
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(aad.as_slice()),
        &mut tag,
    )
    .map_err(|_| anyhow::anyhow!("failed to authenticate encrypted filesystem header"))?;
    header[HEADER_TAG_OFFSET..].copy_from_slice(&tag);
    Ok(header)
}

fn decode_header(
    header: &[u8; HEADER_SLOT_SIZE],
    expected_slot: u8,
    master_key: &[u8; 32],
) -> Result<(Header, LessSafeKey)> {
    if &header[..CONTENT_MAGIC.len()] != CONTENT_MAGIC
        || header[8] != CONTENT_VERSION
        || header[9] != expected_slot
        || header[10..16].iter().any(|byte| *byte != 0)
        || header[48..52].iter().any(|byte| *byte != 0)
    {
        bail!("unsupported encrypted filesystem file format");
    }
    let file_id: [u8; FILE_ID_SIZE] = header[16..32]
        .try_into()
        .expect("file ID range has a fixed size");
    let key = derive_file_key(master_key, &file_id)?;
    let nonce: [u8; NONCE_SIZE] = header[HEADER_NONCE_OFFSET..HEADER_TAG_OFFSET]
        .try_into()
        .expect("header nonce range has a fixed size");
    let aad = header_aad(&header[..HEADER_AUTHENTICATED_SIZE]);
    let mut tag = header[HEADER_TAG_OFFSET..].to_vec();
    let opened = key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad.as_slice()),
            &mut tag,
        )
        .map_err(|_| anyhow::anyhow!("encrypted filesystem header authentication failed"))?;
    if !opened.is_empty() {
        bail!("encrypted filesystem header authentication payload is invalid");
    }
    Ok((
        Header {
            file_id,
            generation: u64::from_be_bytes(header[32..40].try_into().unwrap()),
            logical_len: u64::from_be_bytes(header[40..48].try_into().unwrap()),
            slot: expected_slot,
        },
        key,
    ))
}

fn header_aad(header: &[u8]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(HEADER_AAD.len() + header.len());
    aad.extend_from_slice(HEADER_AAD);
    aad.extend_from_slice(header);
    aad
}

fn block_aad(file_id: [u8; FILE_ID_SIZE], index: u64) -> Vec<u8> {
    let mut aad = Vec::with_capacity(BLOCK_AAD.len() + FILE_ID_SIZE + 8);
    aad.extend_from_slice(BLOCK_AAD);
    aad.extend_from_slice(&file_id);
    aad.extend_from_slice(&index.to_be_bytes());
    aad
}

fn block_offset(index: u64) -> Result<u64> {
    index
        .checked_mul(CIPHERTEXT_BLOCK_SIZE as u64)
        .and_then(|offset| offset.checked_add(CONTENT_HEADER_SIZE as u64))
        .context("encrypted filesystem block offset overflowed")
}

enum SlotRead {
    Missing,
    Present,
}

fn read_slot_at(file: &File, buffer: &mut [u8], offset: u64) -> Result<SlotRead> {
    let mut completed = 0;
    while completed < buffer.len() {
        let read = file.read_at(&mut buffer[completed..], offset + completed as u64)?;
        if read == 0 {
            if completed == 0 {
                return Ok(SlotRead::Missing);
            }
            bail!("encrypted filesystem block is incomplete");
        }
        completed += read;
    }
    Ok(SlotRead::Present)
}

fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> Result<()> {
    match read_slot_at(file, buffer, offset)? {
        SlotRead::Present => Ok(()),
        SlotRead::Missing => bail!("encrypted filesystem data is incomplete"),
    }
}

fn write_all_at(file: &File, buffer: &[u8], offset: u64) -> Result<()> {
    let mut completed = 0;
    while completed < buffer.len() {
        let written = file.write_at(&buffer[completed..], offset + completed as u64)?;
        if written == 0 {
            bail!("failed to make progress writing encrypted filesystem data");
        }
        completed += written;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_random_access_and_block_offset_overflow_are_safe() {
        let directory = tempfile::tempdir().unwrap();
        let mut file =
            EncryptedFile::create(&directory.path().join("encrypted"), &[7_u8; 32]).unwrap();
        let mut empty = [];

        assert_eq!(file.read_at(&mut empty, 0).unwrap(), 0);
        assert_eq!(file.write_at(&[], u64::MAX).unwrap(), 0);
        assert!(block_offset(u64::MAX).is_err());
    }
}
