use super::*;
use std::os::fd::AsRawFd;

#[test]
fn unsupported_mutation_helpers_cover_recursive_native_and_invalid_paths() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = FilesystemHookRuntime::new(directory.path().join("workdir/fs")).unwrap();
    let native = c"/dev/null";
    let descriptor = std::fs::File::open("/dev/null").unwrap();

    with_test_runtime(&runtime, || unsafe {
        {
            let _guard = FilesystemHookGuard::enter().unwrap();
            assert_eq!(
                sandbox_unsupported_descriptor_mutation(descriptor.as_raw_fd(), |_| 71),
                71
            );
            assert_eq!(
                sandbox_unsupported_pair_mutation(
                    native.as_ptr(),
                    libc::AT_FDCWD,
                    native.as_ptr(),
                    libc::AT_FDCWD,
                    |_, _, _, _| 72,
                ),
                72
            );
        }

        assert_eq!(
            sandbox_unsupported_path_mutation(native.as_ptr(), libc::AT_FDCWD, |directory, _| {
                assert_eq!(directory, libc::AT_FDCWD);
                73
            }),
            73
        );
        assert_eq!(
            sandbox_unsupported_descriptor_mutation(descriptor.as_raw_fd(), |_| 74),
            74
        );
        assert_eq!(
            sandbox_unsupported_pair_mutation(
                native.as_ptr(),
                libc::AT_FDCWD,
                native.as_ptr(),
                libc::AT_FDCWD,
                |first_directory, _, second_directory, _| {
                    assert_eq!(first_directory, libc::AT_FDCWD);
                    assert_eq!(second_directory, libc::AT_FDCWD);
                    75
                },
            ),
            75
        );

        assert_eq!(
            sandbox_unsupported_path_mutation(std::ptr::null(), libc::AT_FDCWD, |_, _| 0),
            -1
        );
        assert_eq!(
            sandbox_unsupported_pair_mutation(
                std::ptr::null(),
                libc::AT_FDCWD,
                native.as_ptr(),
                libc::AT_FDCWD,
                |_, _, _, _| 0,
            ),
            -1
        );
    });
}
