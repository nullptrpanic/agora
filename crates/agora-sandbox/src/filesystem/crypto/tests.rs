use super::FileCipher;
use base64::Engine as _;
use std::io::{Read, Seek, SeekFrom, Write};

fn temporary_directory(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "agora-filesystem-crypto-{name}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn encrypted_file_round_trip_never_writes_plaintext_to_the_backing_file() {
    let root = temporary_directory("round-trip");
    let encrypted = root.join("encrypted");
    let marker = b"plaintext marker that must not reach backing storage";
    let cipher = FileCipher::derive(b"workspace key", b"0123456789abcdef").unwrap();
    let mut plaintext = tempfile::tempfile().unwrap();
    plaintext.write_all(marker).unwrap();
    plaintext.seek(SeekFrom::Start(0)).unwrap();

    cipher.encrypt(&mut plaintext, &encrypted).unwrap();

    let stored = std::fs::read(&encrypted).unwrap();
    assert!(!stored.windows(marker.len()).any(|window| window == marker));
    let mut decrypted = tempfile::tempfile().unwrap();
    cipher.decrypt(&encrypted, &mut decrypted).unwrap();
    decrypted.seek(SeekFrom::Start(0)).unwrap();
    let mut restored = Vec::new();
    decrypted.read_to_end(&mut restored).unwrap();
    assert_eq!(restored, marker);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn encrypted_file_random_overwrite_changes_only_the_affected_ciphertext_block() {
    let root = temporary_directory("random-overwrite");
    let encrypted = root.join("encrypted");
    let cipher = FileCipher::derive(b"workspace key", b"0123456789abcdef").unwrap();
    let original = vec![b'a'; super::PLAINTEXT_BLOCK_SIZE * 3];
    let mut plaintext = tempfile::tempfile().unwrap();
    plaintext.write_all(&original).unwrap();
    plaintext.seek(SeekFrom::Start(0)).unwrap();
    cipher.encrypt(&mut plaintext, &encrypted).unwrap();
    let before = std::fs::read(&encrypted).unwrap();

    let mut file = cipher.open_file(&encrypted).unwrap();
    file.write_at(b"changed", (super::PLAINTEXT_BLOCK_SIZE + 17) as u64)
        .unwrap();
    file.sync_all().unwrap();
    let after = std::fs::read(&encrypted).unwrap();

    let first =
        super::CONTENT_HEADER_SIZE..super::CONTENT_HEADER_SIZE + super::CIPHERTEXT_BLOCK_SIZE;
    let third_start = super::CONTENT_HEADER_SIZE + super::CIPHERTEXT_BLOCK_SIZE * 2;
    let third = third_start..third_start + super::CIPHERTEXT_BLOCK_SIZE;
    assert_eq!(&before[first.clone()], &after[first]);
    assert_eq!(&before[third.clone()], &after[third]);
    assert_ne!(before, after);

    let mut restored = vec![0; original.len()];
    assert_eq!(file.read_at(&mut restored, 0).unwrap(), original.len());
    let mut expected = original;
    expected[super::PLAINTEXT_BLOCK_SIZE + 17..super::PLAINTEXT_BLOCK_SIZE + 24]
        .copy_from_slice(b"changed");
    assert_eq!(restored, expected);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn encrypted_file_supports_sparse_extension_and_truncation() {
    let root = temporary_directory("resize");
    let encrypted = root.join("encrypted");
    let cipher = FileCipher::derive(b"workspace key", b"0123456789abcdef").unwrap();
    let mut plaintext = tempfile::tempfile().unwrap();
    plaintext.write_all(b"prefix").unwrap();
    cipher.encrypt(&mut plaintext, &encrypted).unwrap();
    let offset = (super::PLAINTEXT_BLOCK_SIZE * 3 + 11) as u64;

    let mut file = cipher.open_file(&encrypted).unwrap();
    file.write_at(b"tail", offset).unwrap();
    assert_eq!(file.len(), offset + 4);
    let mut hole = vec![1; offset as usize - 6];
    assert_eq!(file.read_at(&mut hole, 6).unwrap(), hole.len());
    assert!(hole.iter().all(|byte| *byte == 0));

    file.set_len(3).unwrap();
    file.sync_all().unwrap();
    assert_eq!(file.len(), 3);
    let mut restored = [0; 8];
    assert_eq!(file.read_at(&mut restored, 0).unwrap(), 3);
    assert_eq!(&restored[..3], b"pre");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn encrypted_file_authenticates_each_random_access_block() {
    let root = temporary_directory("block-authentication");
    let encrypted = root.join("encrypted");
    let cipher = FileCipher::derive(b"workspace key", b"0123456789abcdef").unwrap();
    let mut plaintext = tempfile::tempfile().unwrap();
    plaintext
        .write_all(&vec![b'x'; super::PLAINTEXT_BLOCK_SIZE * 2])
        .unwrap();
    cipher.encrypt(&mut plaintext, &encrypted).unwrap();
    let mut bytes = std::fs::read(&encrypted).unwrap();
    bytes[super::CONTENT_HEADER_SIZE + 20] ^= 0x40;
    std::fs::write(&encrypted, bytes).unwrap();

    let file = cipher.open_file(&encrypted).unwrap();
    let mut block = vec![0; super::PLAINTEXT_BLOCK_SIZE];
    assert!(file.read_at(&mut block, 0).is_err());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn encrypted_file_rejects_the_wrong_key_without_returning_plaintext() {
    let root = temporary_directory("wrong-key");
    let encrypted = root.join("encrypted");
    let cipher = FileCipher::derive(b"workspace key", b"0123456789abcdef").unwrap();
    let wrong = FileCipher::derive(b"wrong key", b"0123456789abcdef").unwrap();
    let mut plaintext = tempfile::tempfile().unwrap();
    plaintext.write_all(b"secret").unwrap();
    plaintext.seek(SeekFrom::Start(0)).unwrap();
    cipher.encrypt(&mut plaintext, &encrypted).unwrap();
    let mut decrypted = tempfile::tempfile().unwrap();

    assert!(wrong.decrypt(&encrypted, &mut decrypted).is_err());
    assert_eq!(decrypted.metadata().unwrap().len(), 0);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cipher_rejects_invalid_derivation_inputs_and_redacts_debug_output() {
    assert!(FileCipher::derive(b"", b"0123456789abcdef").is_err());
    assert!(FileCipher::derive(b"key", b"short").is_err());

    let cipher = FileCipher::derive(b"secret key", b"0123456789abcdef").unwrap();
    let debug = format!("{cipher:?}");
    assert!(debug.contains("FileCipher"));
    assert!(!debug.contains("secret key"));
    assert!(!debug.contains(cipher.key_id()));
}

#[test]
fn cipher_key_material_can_be_reused_without_repeating_pbkdf2() {
    let derived = FileCipher::derive(b"secret key", b"0123456789abcdef").unwrap();
    let restored = FileCipher::from_key(derived.key_material()).unwrap();

    assert_eq!(restored.key_id(), derived.key_id());
    assert!(FileCipher::from_key(b"short").is_err());
}

#[test]
fn filename_encryption_is_randomized_authenticated_and_byte_preserving() {
    let cipher = FileCipher::derive(b"workspace key", b"0123456789abcdef").unwrap();
    let wrong = FileCipher::derive(b"wrong key", b"0123456789abcdef").unwrap();
    let name = b"\xe5\xae\x89\xe5\x85\xa8-\x80.docx";

    let first = cipher.encrypt_name(name).unwrap();
    let second = cipher.encrypt_name(name).unwrap();

    assert_ne!(first, second);
    assert_eq!(cipher.decrypt_name(&first).unwrap(), name);
    assert_eq!(cipher.decrypt_name(&second).unwrap(), name);
    assert!(wrong.decrypt_name(&first).is_err());
    assert!(cipher.decrypt_name("").is_err());
    assert!(
        cipher
            .decrypt_name(&format!("{}A", super::ENCRYPTED_NAME_PREFIX))
            .is_err()
    );
    let incomplete = format!(
        "{}{}",
        super::ENCRYPTED_NAME_PREFIX,
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0_u8])
    );
    assert!(cipher.decrypt_name(&incomplete).is_err());
    assert!(cipher.encrypt_name(&vec![b'x'; 300]).is_err());

    let mut corrupted = first.into_bytes();
    let last = corrupted.last_mut().unwrap();
    *last = if *last == b'A' { b'B' } else { b'A' };
    assert!(
        cipher
            .decrypt_name(std::str::from_utf8(&corrupted).unwrap())
            .is_err()
    );
}

#[test]
fn encryption_failures_remove_temporary_ciphertext_files() {
    let root = temporary_directory("publish-failure");
    let destination = root.join("occupied");
    std::fs::create_dir(&destination).unwrap();
    let mut plaintext = tempfile::tempfile().unwrap();
    plaintext.write_all(b"secret").unwrap();
    let cipher = FileCipher::derive(b"key", b"0123456789abcdef").unwrap();

    assert!(cipher.encrypt(&mut plaintext, &destination).is_err());
    assert!(destination.is_dir());
    assert!(std::fs::read_dir(&root).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".agora-encrypted-")
    }));

    let blocked_parent = root.join("blocked");
    std::fs::write(&blocked_parent, b"file").unwrap();
    assert!(
        cipher
            .encrypt(&mut plaintext, &blocked_parent.join("child"))
            .is_err()
    );

    if unsafe { libc::geteuid() } != 0 {
        use std::os::unix::fs::PermissionsExt as _;

        let readonly = root.join("readonly");
        std::fs::create_dir(&readonly).unwrap();
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o500)).unwrap();
        assert!(
            cipher
                .encrypt(&mut plaintext, &readonly.join("child"))
                .is_err()
        );
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn encrypted_file_overwrite_replaces_existing_plaintext() {
    let root = temporary_directory("overwrite");
    let encrypted = root.join("encrypted");
    let cipher = FileCipher::derive(b"key", b"0123456789abcdef").unwrap();
    let mut original = tempfile::tempfile().unwrap();
    original.write_all(b"original").unwrap();
    cipher.encrypt(&mut original, &encrypted).unwrap();
    let mut replacement = tempfile::tempfile().unwrap();
    replacement.write_all(b"replacement contents").unwrap();

    cipher.overwrite(&mut replacement, &encrypted).unwrap();

    let mut plaintext = tempfile::tempfile().unwrap();
    cipher.decrypt(&encrypted, &mut plaintext).unwrap();
    plaintext.rewind().unwrap();
    let mut contents = String::new();
    plaintext.read_to_string(&mut contents).unwrap();
    assert_eq!(contents, "replacement contents");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn decryption_rejects_malformed_headers_and_incomplete_blocks() {
    let root = temporary_directory("malformed");
    let cipher = FileCipher::derive(b"key", b"0123456789abcdef").unwrap();
    let decrypt = |path: &std::path::Path| {
        let mut plaintext = tempfile::tempfile().unwrap();
        cipher
            .decrypt(path, &mut plaintext)
            .unwrap_err()
            .to_string()
    };

    assert!(decrypt(&root.join("missing")).contains("failed to open"));

    let trailing = root.join("trailing-data");
    let mut plaintext = tempfile::tempfile().unwrap();
    plaintext.write_all(b"contents").unwrap();
    cipher.encrypt(&mut plaintext, &trailing).unwrap();
    let expected_length = std::fs::metadata(&trailing).unwrap().len();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&trailing)
        .unwrap()
        .write_all(b"trailing bytes")
        .unwrap();
    assert!(std::fs::metadata(&trailing).unwrap().len() > expected_length);
    drop(cipher.open_file(&trailing).unwrap());
    assert_eq!(std::fs::metadata(&trailing).unwrap().len(), expected_length);

    let incomplete_header = root.join("incomplete-header");
    std::fs::write(&incomplete_header, b"short").unwrap();
    assert!(decrypt(&incomplete_header).contains("encrypted filesystem"));

    let invalid_format = root.join("invalid-format");
    std::fs::write(&invalid_format, [0_u8; super::CONTENT_HEADER_SIZE]).unwrap();
    assert!(decrypt(&invalid_format).contains("encrypted filesystem"));

    let incomplete_block = root.join("incomplete-block");
    let mut plaintext = tempfile::tempfile().unwrap();
    plaintext
        .write_all(&vec![b'x'; super::PLAINTEXT_BLOCK_SIZE])
        .unwrap();
    cipher.encrypt(&mut plaintext, &incomplete_block).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&incomplete_block)
        .unwrap()
        .set_len((super::CONTENT_HEADER_SIZE + super::CIPHERTEXT_BLOCK_SIZE / 2) as u64)
        .unwrap();
    assert!(decrypt(&incomplete_block).contains("failed to decrypt"));

    std::fs::remove_dir_all(root).unwrap();
}
