use super::{
    ArchitectureSelection, CPU_SUBTYPE_ARM64E, CPU_TYPE_ARM64, CS_DYLD_RESTRICTED, CS_RUNTIME,
    ExecutableStore, MACH_64_MAGIC, resolve_shebang,
};
use crate::execution::resolve_executable;
use crate::filesystem::{EntryState, Materializer};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;

#[test]
fn default_executable_path_matches_macos_execvp() {
    assert_eq!(crate::execution::DEFAULT_EXECUTABLE_PATH, "/usr/bin:/bin");
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("agora-store-test-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn code_signing_flags_are_cached_for_an_unchanged_executable() {
    let root = TestDirectory::new();
    let executable = root.path().join("executable");
    fs::write(&executable, b"first").unwrap();
    let inspections = AtomicUsize::new(0);

    let metadata = executable.metadata().unwrap();
    let first = ExecutableStore::cached_code_signing_flags(&metadata, || {
        inspections.fetch_add(1, Ordering::Relaxed);
        Ok(7)
    })
    .unwrap();
    let cached = ExecutableStore::cached_code_signing_flags(&metadata, || {
        inspections.fetch_add(1, Ordering::Relaxed);
        Ok(9)
    })
    .unwrap();

    fs::write(&executable, b"changed contents").unwrap();
    let changed =
        ExecutableStore::cached_code_signing_flags(&executable.metadata().unwrap(), || {
            inspections.fetch_add(1, Ordering::Relaxed);
            Ok(11)
        })
        .unwrap();

    assert_eq!((first, cached, changed), (7, 7, 11));
    assert_eq!(inspections.load(Ordering::Relaxed), 2);
}

#[test]
fn executable_store_prepares_and_caches_a_native_copy() {
    let root = TestDirectory::new();
    let directory = root.path().join("prepared");
    let store = ExecutableStore::new(directory.clone()).unwrap();

    let first = store.prepare(Path::new("/bin/sh")).unwrap();
    let second = store.prepare(Path::new("/bin/sh")).unwrap();

    assert_eq!(first, second);
    assert_eq!(first, directory.join("bin/sh"));
    let source = Path::new("/bin/sh").canonicalize().unwrap();
    let Some(EntryState::Cached {
        checksum,
        materializer,
        source: source_identity,
        variant,
        destination,
    }) = store.overlay.state_for_test(&source).unwrap()
    else {
        panic!("missing executable cache metadata");
    };
    assert!(checksum.is_some());
    assert_eq!(materializer, Materializer::Executable);
    assert!(source_identity.is_some());
    assert!(destination.is_some());
    assert_eq!(
        variant.as_deref(),
        Some(format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH).as_str())
    );
    assert!(!first.with_extension("md5").exists());
    assert!(first.is_file());
    assert_ne!(first, Path::new("/bin/sh"));
    assert_eq!(
        ExecutableStore::architectures(&first).unwrap(),
        [ExecutableStore::native_architecture()]
    );
    assert_eq!(
        directory.metadata().unwrap().permissions().mode() & 0o777,
        0o700
    );

    assert!(directory.is_dir());
    assert!(first.is_file());

    let inode = first.metadata().unwrap().ino();
    let metadata_path = directory.join("bin/.metadata");
    drop(store);

    let reused_store = ExecutableStore::new(directory).unwrap();
    let reused = reused_store.prepare(Path::new("/bin/sh")).unwrap();
    assert_eq!(reused, first);
    assert_eq!(reused.metadata().unwrap().ino(), inode);
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(metadata_path).unwrap()).unwrap();
    assert_eq!(metadata["version"], 3);
    assert!(metadata["entries"].get("sh").is_some());
    assert!(metadata["entries"]["sh"]["entry"]["checksum"].is_string());
    assert!(metadata["entries"]["sh"]["entry"]["destination"].is_object());
    assert_eq!(
        metadata["entries"]["sh"]["entry"]["variant"],
        format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH)
    );
}

#[test]
fn executable_store_cache_hit_skips_architecture_inspection() {
    let root = TestDirectory::new();
    let store = ExecutableStore::new(root.path().join("prepared")).unwrap();

    store.prepare(Path::new("/bin/sh")).unwrap();
    super::ARCHITECTURE_INSPECTIONS.with(|inspections| inspections.set(0));

    store.prepare(Path::new("/bin/sh")).unwrap();

    assert_eq!(
        super::ARCHITECTURE_INSPECTIONS.with(std::cell::Cell::get),
        0
    );
}

#[test]
fn executable_store_keeps_normal_reads_on_lower_before_execution() {
    let root = TestDirectory::new();
    let store = ExecutableStore::new(root.path().join("prepared")).unwrap();
    let source = Path::new("/bin/cat").canonicalize().unwrap();
    assert_eq!(store.overlay.prepare_read(&source).unwrap(), source);
    assert_eq!(store.overlay.state(&source).unwrap(), None);

    let cached = store.prepare(&source).unwrap();
    assert_ne!(cached, source);
    assert!(matches!(
        store.overlay.state(&source).unwrap(),
        Some(EntryState::Cached {
            materializer: Materializer::Executable,
            ..
        })
    ));
}

#[test]
fn executable_store_keeps_unrestricted_binaries_and_scripts_at_their_original_paths() {
    let root = TestDirectory::new();
    let directory = root.path().join("workdir/fs");
    let store = ExecutableStore::new(directory.clone()).unwrap();
    let binary = std::env::current_exe().unwrap().canonicalize().unwrap();

    assert_eq!(store.prepare(&binary).unwrap(), binary);

    let script = root.path().join("client");
    fs::write(&script, b"#!/usr/bin/env node\r\nconsole.log('ok')\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let script = script.canonicalize().unwrap();
    let shebang = resolve_shebang(&script).unwrap().unwrap();

    assert_eq!(shebang.interpreter, Path::new("/usr/bin/env"));
    assert_eq!(shebang.argument.as_deref(), Some(OsStr::new("node")));
    assert_eq!(store.prepare(&script).unwrap(), script);
    assert!(!store.destination(&script).unwrap().exists());
    assert_eq!(fs::read_dir(directory).unwrap().count(), 2);
}

#[test]
fn executable_store_prefers_a_cow_script_over_the_lower_file() {
    let root = TestDirectory::new();
    let store = ExecutableStore::new(root.path().join("workdir/fs")).unwrap();
    let source = root.path().join("script");
    fs::write(&source, b"#!/bin/sh\necho lower\n").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    let mapped = store.overlay.prepare_write(&source, false).unwrap();
    fs::write(&mapped, b"#!/bin/sh\necho sandbox\n").unwrap();
    fs::set_permissions(&mapped, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(store.prepare(&source).unwrap(), mapped);
    assert_eq!(
        fs::read_to_string(&source).unwrap(),
        "#!/bin/sh\necho lower\n"
    );
}

#[test]
fn executable_store_resigns_a_restricted_cow_binary_in_place() {
    let root = TestDirectory::new();
    let source = root.path().join("restricted");
    fs::copy("/bin/sh", &source).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    let status = Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-", "--options", "runtime"])
        .arg(&source)
        .status()
        .unwrap();
    assert!(status.success());
    let store = ExecutableStore::new(root.path().join("workdir/fs")).unwrap();
    let mapped = store.overlay.prepare_write(&source, false).unwrap();

    assert_eq!(store.prepare(&source).unwrap(), mapped);
    assert_eq!(
        ExecutableStore::code_signing_flags(&mapped).unwrap() & CS_DYLD_RESTRICTED,
        0
    );
    assert_ne!(
        ExecutableStore::code_signing_flags(&source).unwrap() & CS_DYLD_RESTRICTED,
        0
    );
}

#[test]
fn executable_store_copies_hardened_runtime_binaries() {
    let root = TestDirectory::new();
    let source = root.path().join("hardened-sh");
    fs::copy("/bin/sh", &source).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    let status = Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-", "--options", "runtime"])
        .arg(&source)
        .status()
        .unwrap();
    assert!(status.success());
    assert_ne!(
        ExecutableStore::code_signing_flags(&source).unwrap() & CS_DYLD_RESTRICTED,
        0
    );
    let store = ExecutableStore::new(root.path().join("workdir/fs")).unwrap();

    let prepared = store.prepare(&source).unwrap();

    assert_ne!(prepared, source);
    assert_eq!(
        ExecutableStore::code_signing_flags(&prepared).unwrap() & CS_DYLD_RESTRICTED,
        0
    );
}

#[test]
fn executable_store_preserves_entitlements_when_resigning() {
    let root = TestDirectory::new();
    let source = root.path().join("entitled-sh");
    let entitlements = root.path().join("entitlements.plist");
    fs::copy("/bin/sh", &source).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        &entitlements,
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
          <plist version=\"1.0\"><dict>\
          <key>com.apple.security.cs.allow-jit</key><true/>\
          </dict></plist>\n",
    )
    .unwrap();
    let status = Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-", "--options", "runtime"])
        .arg("--entitlements")
        .arg(&entitlements)
        .arg(&source)
        .status()
        .unwrap();
    assert!(status.success());
    let store = ExecutableStore::new(root.path().join("workdir/fs")).unwrap();

    let prepared = store.prepare(&source).unwrap();

    let output = Command::new("/usr/bin/codesign")
        .args(["--display", "--entitlements", ":-", "--xml"])
        .arg(&prepared)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("com.apple.security.cs.allow-jit"));
    assert_eq!(
        ExecutableStore::code_signing_flags(&prepared).unwrap() & CS_DYLD_RESTRICTED,
        0
    );
}

#[test]
fn executable_store_preserves_code_identifier_when_resigning() {
    let root = TestDirectory::new();
    let source = root.path().join("identified-sh");
    fs::copy("/bin/sh", &source).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    let status = Command::new("/usr/bin/codesign")
        .args([
            "--force",
            "--sign",
            "-",
            "--options",
            "runtime",
            "--identifier",
            "com.example.agora-fixture",
            "--timestamp=none",
        ])
        .arg(&source)
        .status()
        .unwrap();
    assert!(status.success());
    let store = ExecutableStore::new(root.path().join("workdir/fs")).unwrap();

    let prepared = store.prepare(&source).unwrap();

    let output = Command::new("/usr/bin/codesign")
        .args(["--display", "--verbose=4"])
        .arg(prepared)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .any(|line| line == "Identifier=com.example.agora-fixture")
    );
}

#[test]
fn code_signing_flags_parser_reads_the_runtime_bit() {
    assert_eq!(
        ExecutableStore::parse_code_signing_flags(
            b"CodeDirectory v=20500 size=42 flags=0x10002(adhoc,runtime) hashes=1+0\n"
        )
        .unwrap(),
        CS_RUNTIME | 2
    );
    assert!(ExecutableStore::parse_code_signing_flags(b"unsigned").is_err());
    assert!(
        ExecutableStore::parse_code_signing_flags(b"CodeDirectory flags=0xzz(invalid)")
            .unwrap_err()
            .to_string()
            .contains("invalid CodeDirectory flags")
    );

    let root = TestDirectory::new();
    let unsigned = root.path().join("unsigned");
    fs::write(&unsigned, b"unsigned").unwrap();
    assert_eq!(ExecutableStore::code_signing_flags(&unsigned).unwrap(), 0);
}

#[test]
fn shebang_parser_handles_optional_arguments_and_rejects_invalid_interpreters() {
    let root = TestDirectory::new();
    let script = root.path().join("script");

    fs::write(&script, b"plain text\n").unwrap();
    assert!(resolve_shebang(&script).unwrap().is_none());

    fs::write(&script, b"#!  /bin/sh  \t").unwrap();
    let shebang = resolve_shebang(&script).unwrap().unwrap();
    assert_eq!(shebang.interpreter, Path::new("/bin/sh"));
    assert!(shebang.argument.is_none());

    fs::write(&script, b"#!\n").unwrap();
    assert!(
        resolve_shebang(&script)
            .unwrap_err()
            .to_string()
            .contains("has no interpreter")
    );

    fs::write(&script, b"#!env node\n").unwrap();
    assert!(
        resolve_shebang(&script)
            .unwrap_err()
            .to_string()
            .contains("interpreter is not absolute")
    );

    let mut long = b"#!".to_vec();
    long.resize(super::MAX_SHEBANG_LINE_SIZE, b'x');
    fs::write(&script, long).unwrap();
    assert!(
        resolve_shebang(&script)
            .unwrap_err()
            .to_string()
            .contains("shebang is too long")
    );
}

#[test]
fn executable_store_rejects_non_files_and_non_executable_files() {
    let root = TestDirectory::new();
    let directory = root.path().join("workdir/fs");
    let store = ExecutableStore::new(directory.clone()).unwrap();
    let plain = root.path().join("plain");
    fs::write(&plain, b"not executable").unwrap();

    assert!(
        store
            .prepare(root.path())
            .unwrap_err()
            .to_string()
            .contains("not a file")
    );
    assert!(
        store
            .prepare(&plain)
            .unwrap_err()
            .to_string()
            .contains("not executable")
    );
}

#[test]
fn executable_store_rejects_system_mediated_application_launch() {
    let root = TestDirectory::new();
    let store = ExecutableStore::new(root.path().join("workdir/fs")).unwrap();

    let error = store.prepare(Path::new("/usr/bin/open")).unwrap_err();

    assert_eq!(
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<std::io::Error>())
            .and_then(std::io::Error::raw_os_error),
        Some(libc::ENOTSUP)
    );
    assert!(
        error
            .to_string()
            .contains("execute the application bundle binary directly")
    );
}

#[test]
#[cfg(target_arch = "aarch64")]
fn executable_store_rewrites_a_single_arm64e_slice() {
    let root = TestDirectory::new();
    let source = root.path().join("arm64e-sh");
    let status = Command::new("/usr/bin/lipo")
        .args(["/bin/sh", "-thin", "arm64e", "-output"])
        .arg(&source)
        .status()
        .unwrap();
    assert!(status.success());
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    let store = ExecutableStore::new(root.path().join("prepared")).unwrap();

    let prepared = store.prepare_copy_for_test(&source).unwrap();

    assert_eq!(
        ExecutableStore::architectures(&prepared).unwrap(),
        ["arm64"]
    );
}

#[test]
fn architecture_selection_matches_the_build_target() {
    let architectures = vec!["arm64".to_string(), "x86_64".to_string()];

    let x86 = ExecutableStore::select_architecture("x86_64", &architectures).unwrap();
    let arm = ExecutableStore::select_architecture("arm64", &architectures).unwrap();

    assert_eq!(x86.slice, "x86_64");
    assert!(!x86.rewrite_arm64e);
    assert_eq!(arm.slice, "arm64");
    assert!(!arm.rewrite_arm64e);
}

#[test]
fn arm64e_is_only_an_arm64_fallback() {
    let architectures = vec!["arm64e".to_string()];

    let arm = ExecutableStore::select_architecture("arm64", &architectures).unwrap();

    assert_eq!(arm.slice, "arm64e");
    assert!(arm.rewrite_arm64e);
    assert!(ExecutableStore::select_architecture("x86_64", &architectures).is_err());
}

#[test]
fn executable_store_rejects_a_slice_incompatible_with_the_build_target() {
    let root = TestDirectory::new();
    let source = root.path().join("incompatible-sh");
    let architectures = ExecutableStore::architectures(Path::new("/bin/sh")).unwrap();
    let Some(incompatible) = architectures.into_iter().find(|architecture| {
        ExecutableStore::select_architecture(
            ExecutableStore::native_architecture(),
            std::slice::from_ref(architecture),
        )
        .is_err()
    }) else {
        return;
    };
    let status = Command::new("/usr/bin/lipo")
        .args(["/bin/sh", "-thin", &incompatible, "-output"])
        .arg(&source)
        .status()
        .unwrap();
    assert!(status.success());
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    let store = ExecutableStore::new(root.path().join("prepared")).unwrap();

    let error = store.prepare_copy_for_test(&source).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("incompatible with sandbox build target")
    );
}

#[test]
fn executable_store_reports_directory_creation_errors() {
    let root = TestDirectory::new();
    let parent_file = root.path().join("not-a-directory");
    fs::write(&parent_file, b"file").unwrap();
    assert!(ExecutableStore::new(parent_file.join("prepared")).is_err());
}

#[test]
fn executable_store_reports_cache_entry_access_errors() {
    let root = TestDirectory::new();
    let lock_directory = root.path().join("lock-directory");
    fs::create_dir_all(lock_directory.join(".vfs.lock")).unwrap();
    assert!(
        ExecutableStore::new(lock_directory)
            .err()
            .unwrap()
            .to_string()
            .contains("failed to open overlay lock")
    );
}

#[test]
fn executable_store_reports_temporary_directory_creation_failure_without_artifacts() {
    let root = TestDirectory::new();
    let source = root.path().join("native-sh");
    let architectures = ExecutableStore::architectures(Path::new("/bin/sh")).unwrap();
    let selected = ExecutableStore::select_architecture(
        ExecutableStore::native_architecture(),
        &architectures,
    )
    .unwrap();
    let status = Command::new("/usr/bin/lipo")
        .args(["/bin/sh", "-thin", &selected.slice, "-output"])
        .arg(&source)
        .status()
        .unwrap();
    assert!(status.success());
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    let directory = root.path().join("copy-error");
    let store = ExecutableStore::new(directory.clone()).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o500)).unwrap();

    let error = store.prepare_copy_for_test(&source).unwrap_err();
    assert_eq!(
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<std::io::Error>())
            .and_then(std::io::Error::raw_os_error),
        Some(libc::EACCES)
    );
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn tool_output_errors_include_stderr_and_reject_invalid_utf8() {
    let failed = Command::new("/bin/sh")
        .args(["-c", "printf denied >&2; exit 7"])
        .output()
        .unwrap();
    assert_eq!(
        ExecutableStore::check_output(failed, "tool failed")
            .unwrap_err()
            .to_string(),
        "tool failed: denied"
    );

    let invalid = Command::new("/bin/sh")
        .args(["-c", "printf '\\377'"])
        .output()
        .unwrap();
    assert!(ExecutableStore::check_output(invalid, "invalid output").is_err());
    assert!(
        ExecutableStore::run_tool(
            "/missing/agora-tool",
            std::iter::empty::<&OsStr>(),
            "missing tool",
        )
        .is_err()
    );
}

#[test]
fn arm64e_rewrite_validates_and_updates_the_mach_header() {
    let root = TestDirectory::new();
    let valid = root.path().join("valid");
    let invalid = root.path().join("invalid");
    let mut header = Vec::new();
    header.extend_from_slice(&MACH_64_MAGIC.to_le_bytes());
    header.extend_from_slice(&CPU_TYPE_ARM64.to_le_bytes());
    header.extend_from_slice(&(0x8000_0000 | CPU_SUBTYPE_ARM64E).to_le_bytes());
    fs::write(&valid, &header).unwrap();
    fs::write(&invalid, [0_u8; 12]).unwrap();

    ExecutableStore::rewrite_arm64e_subtype(&valid).unwrap();
    let mut rewritten = Vec::new();
    fs::File::open(&valid)
        .unwrap()
        .read_to_end(&mut rewritten)
        .unwrap();
    assert_eq!(&rewritten[8..12], &0_u32.to_le_bytes());
    assert!(ExecutableStore::rewrite_arm64e_subtype(&invalid).is_err());
}

#[test]
fn destination_mirrors_the_absolute_source_path() {
    let root = TestDirectory::new();
    let store = ExecutableStore::new(root.path().join("prepared")).unwrap();
    let source = root.path().join("a name!");
    fs::write(&source, b"executable").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    let first = store.destination(&source).unwrap();
    let second = store.destination(&source).unwrap();

    assert_eq!(first, second);
    assert_eq!(
        first,
        store
            .overlay
            .root()
            .join(source.strip_prefix(Path::new("/")).unwrap())
    );
    assert!(
        store
            .destination(Path::new("relative"))
            .unwrap_err()
            .to_string()
            .contains("path is not absolute")
    );
}

#[test]
fn executable_content_preparation_reports_copy_and_signing_failures() {
    let root = TestDirectory::new();
    let metadata = Path::new("/bin/sh").metadata().unwrap();
    let selected = ArchitectureSelection {
        slice: ExecutableStore::native_architecture().to_string(),
        rewrite_arm64e: false,
    };
    let missing = root.path().join("missing");
    let temporary = root.path().join("temporary");

    assert!(
        ExecutableStore::prepare_executable_contents(
            &missing,
            &temporary,
            &metadata,
            std::slice::from_ref(&selected.slice),
            &selected,
        )
        .unwrap_err()
        .to_string()
        .contains("failed to copy executable")
    );
    assert!(!temporary.exists());

    assert!(
        ExecutableStore::run_tool(
            "/usr/bin/false",
            std::iter::empty::<&OsStr>(),
            "failed to prepare executable",
        )
        .unwrap_err()
        .to_string()
        .contains("failed to prepare executable")
    );
}

#[test]
fn missing_cached_executable_maps_back_to_its_original_source() {
    let root = TestDirectory::new();
    let store = ExecutableStore::new(root.path().join("prepared")).unwrap();
    let cached = store.overlay.root().join("bin/sh");
    assert!(!cached.exists());

    let prepared = store.prepare(&cached).unwrap();

    assert_eq!(prepared, cached);
    assert!(prepared.is_file());
    assert!(matches!(
        store
            .overlay
            .state_for_test(&Path::new("/bin/sh").canonicalize().unwrap())
            .unwrap(),
        Some(EntryState::Cached {
            materializer: Materializer::Executable,
            ..
        })
    ));
}

#[test]
fn executable_store_rejects_private_workdir_paths_outside_the_backing_root() {
    let root = TestDirectory::new();
    let store = ExecutableStore::new(root.path().join("workdir/fs")).unwrap();
    let private = root.path().join("workdir/private-executable");
    fs::copy("/bin/sh", &private).unwrap();

    let error = store.prepare(&private).unwrap_err();

    assert_eq!(
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<std::io::Error>())
            .and_then(std::io::Error::raw_os_error),
        Some(libc::EACCES)
    );
    assert!(error.to_string().contains("private work directory"));
}

#[test]
fn executable_store_preserves_non_missing_resolution_errors() {
    let root = TestDirectory::new();
    let store = ExecutableStore::new(root.path().join("prepared")).unwrap();
    let looped = root.path().join("loop");
    std::os::unix::fs::symlink(&looped, &looped).unwrap();

    let error = store.resolve_source(&looped).unwrap_err();

    assert_eq!(
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<std::io::Error>())
            .and_then(std::io::Error::raw_os_error),
        Some(libc::ELOOP)
    );
    assert!(error.to_string().contains("failed to resolve executable"));
}

#[test]
fn executable_store_keeps_metadata_for_each_source() {
    let root = TestDirectory::new();
    let source_a = root.path().join("source-a/tool");
    let source_b = root.path().join("source-b/tool");
    fs::create_dir_all(source_a.parent().unwrap()).unwrap();
    fs::create_dir_all(source_b.parent().unwrap()).unwrap();
    fs::copy("/bin/sh", &source_a).unwrap();
    fs::copy("/bin/sh", &source_b).unwrap();
    fs::set_permissions(&source_a, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&source_b, fs::Permissions::from_mode(0o755)).unwrap();
    let directory = root.path().join("prepared");
    let store = ExecutableStore::new(directory.clone()).unwrap();

    store.prepare_copy_for_test(&source_a).unwrap();
    store.prepare_copy_for_test(&source_b).unwrap();

    for source in [&source_a, &source_b] {
        assert!(matches!(
            store
                .overlay
                .state_for_test(&source.canonicalize().unwrap())
                .unwrap(),
            Some(EntryState::Cached {
                materializer: Materializer::Executable,
                ..
            })
        ));
    }
}

#[test]
fn executable_store_rebuilds_when_the_copy_or_metadata_is_missing() {
    let root = TestDirectory::new();
    let directory = root.path().join("prepared");
    let store = ExecutableStore::new(directory.clone()).unwrap();
    let destination = store.prepare(Path::new("/bin/sh")).unwrap();
    let source = Path::new("/bin/sh").canonicalize().unwrap();

    store.overlay.remove_state_for_test(&source).unwrap();
    fs::write(&destination, b"stale executable").unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(store.prepare(Path::new("/bin/sh")).unwrap(), destination);
    assert_ne!(fs::read(&destination).unwrap(), b"stale executable");
    assert!(store.overlay.state_for_test(&source).unwrap().is_some());

    fs::remove_file(&destination).unwrap();
    assert_eq!(store.prepare(Path::new("/bin/sh")).unwrap(), destination);
    assert!(destination.is_file());
    assert!(store.overlay.state_for_test(&source).unwrap().is_some());
}

#[test]
fn executable_store_rebuilds_when_the_source_identity_changes() {
    let root = TestDirectory::new();
    let source = root.path().join("tool");
    fs::copy("/bin/sh", &source).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    let store = ExecutableStore::new(root.path().join("prepared")).unwrap();
    let destination = store.prepare_copy_for_test(&source).unwrap();
    let canonical_source = source.canonicalize().unwrap();
    let first_identity = match store
        .overlay
        .state_for_test(&canonical_source)
        .unwrap()
        .unwrap()
    {
        EntryState::Cached {
            source: Some(source),
            ..
        } => source,
        state => panic!("unexpected state: {state:?}"),
    };
    let first_copy = fs::read(&destination).unwrap();

    fs::copy("/bin/cat", &source).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(store.prepare_copy_for_test(&source).unwrap(), destination);

    assert_ne!(
        match store
            .overlay
            .state_for_test(&canonical_source)
            .unwrap()
            .unwrap()
        {
            EntryState::Cached {
                source: Some(source),
                ..
            } => source,
            state => panic!("unexpected state: {state:?}"),
        },
        first_identity
    );
    assert_ne!(fs::read(destination).unwrap(), first_copy);
}

#[test]
fn executable_store_reuses_the_copy_when_the_source_identity_matches() {
    let root = TestDirectory::new();
    let store = ExecutableStore::new(root.path().join("prepared")).unwrap();
    let destination = store.prepare(Path::new("/bin/sh")).unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o700)).unwrap();

    assert_eq!(store.prepare(Path::new("/bin/sh")).unwrap(), destination);
    assert_eq!(
        destination.metadata().unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[test]
fn executable_store_replaces_invalid_cache_files_and_rejects_non_files() {
    let root = TestDirectory::new();
    let directory = root.path().join("prepared");
    let store = ExecutableStore::new(directory).unwrap();
    let source = Path::new("/bin/sh").canonicalize().unwrap();
    let destination = store.destination(&source).unwrap();
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    fs::write(&destination, b"invalid").unwrap();

    assert_eq!(store.prepare(&source).unwrap(), destination);
    assert_ne!(fs::read(&destination).unwrap(), b"invalid");

    fs::remove_file(&destination).unwrap();
    assert_eq!(store.prepare(&source).unwrap(), destination);
    assert!(destination.is_file());

    store.overlay.remove_state_for_test(&source).unwrap();
    fs::remove_file(&destination).unwrap();
    fs::create_dir_all(&destination).unwrap();
    assert_eq!(store.prepare(&source).unwrap(), destination);
    assert!(destination.is_file());
}

#[test]
fn executable_resolution_supports_direct_relative_and_path_lookup() {
    let root = TestDirectory::new();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let executable = bin.join("tool");
    fs::write(&executable, b"tool").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let environment = BTreeMap::from([(OsString::from("PATH"), OsString::from("bin:"))]);

    assert_eq!(
        resolve_executable(OsStr::new("tool"), Some(root.path()), &environment).unwrap(),
        executable
    );
    let absolute_path = BTreeMap::from([(OsString::from("PATH"), OsString::from("/bin"))]);
    assert_eq!(
        resolve_executable(OsStr::new("sh"), Some(root.path()), &absolute_path).unwrap(),
        PathBuf::from("/bin/sh")
    );
    assert_eq!(
        resolve_executable(OsStr::new("./bin/tool"), Some(root.path()), &environment).unwrap(),
        root.path().join("./bin/tool")
    );
    assert_eq!(
        resolve_executable(executable.as_os_str(), None, &environment).unwrap(),
        executable
    );
    assert!(
        resolve_executable(OsStr::new("missing"), Some(root.path()), &environment)
            .unwrap_err()
            .to_string()
            .contains("not found in PATH")
    );
}
