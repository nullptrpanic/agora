use super::io::sequential_write_range;
use super::mapping::validate_mapping_access;
use std::os::fd::AsRawFd;

#[test]
fn mapping_access_matches_native_private_and_shared_permissions() {
    let write_only =
        validate_mapping_access(libc::O_WRONLY, libc::PROT_READ, libc::MAP_PRIVATE).unwrap_err();
    assert_eq!(write_only.raw_os_error(), Some(libc::EACCES));

    let shared_write = validate_mapping_access(
        libc::O_RDONLY,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED,
    )
    .unwrap_err();
    assert_eq!(shared_write.raw_os_error(), Some(libc::EACCES));

    validate_mapping_access(
        libc::O_RDONLY,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE,
    )
    .unwrap();
    validate_mapping_access(libc::O_RDWR, libc::PROT_WRITE, libc::MAP_SHARED).unwrap();
}

#[test]
fn native_sequential_write_range_covers_shared_offset_progress() {
    let file = tempfile::tempfile().unwrap();
    let descriptor = file.as_raw_fd();
    unsafe {
        assert_eq!(libc::lseek(descriptor, 20, libc::SEEK_SET), 20);
    }

    assert_eq!(
        sequential_write_range(descriptor, Some(0), 10),
        crate::filesystem::ByteRange::new(0, 20).ok()
    );
    assert_eq!(sequential_write_range(-1, None, 1), None);
    assert_eq!(sequential_write_range(descriptor, None, -1), None);
}
