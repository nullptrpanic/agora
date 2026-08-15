use agora_sandbox::callback::{
    Decision, Event, EventType, FileAccessMode, FileEvent, NetworkEvent, NoopCallback,
};
use agora_sandbox::network::{NetworkEnforcement, TlsMode};
use agora_sandbox::runner::{Sandbox, SandboxCommand, SandboxConfig};
#[cfg(target_os = "macos")]
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(target_os = "macos")]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
const TLS_TRUST_ENVIRONMENT: [&str; 5] = [
    "SSL_CERT_FILE",
    "CURL_CA_BUNDLE",
    "REQUESTS_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "GIT_SSL_CAINFO",
];

#[cfg(target_os = "macos")]
const FILESYSTEM_KEY: &str = "test-filesystem-key";

#[cfg(target_os = "macos")]
fn sandbox_lifecycle_timeout(seconds: u64) -> Duration {
    let coverage = std::env::var_os("CARGO_LLVM_COV").is_some();
    let multiplier = match (cfg!(target_arch = "x86_64"), coverage) {
        (true, true) => 8,
        (true, false) | (false, true) => 4,
        (false, false) => 2,
    };
    Duration::from_secs(seconds.saturating_mul(multiplier))
}

#[cfg(target_os = "macos")]
type TestAssociationId = u32;
#[cfg(target_os = "macos")]
type TestConnectionId = u32;

#[cfg(target_os = "macos")]
#[repr(C)]
struct TestSocketEndpoints {
    source_interface: libc::c_uint,
    source_address: *const libc::sockaddr,
    source_address_length: libc::socklen_t,
    destination_address: *const libc::sockaddr,
    destination_address_length: libc::socklen_t,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    static bootstrap_port: libc::mach_port_t;

    fn bootstrap_look_up(
        bootstrap_port: libc::mach_port_t,
        service_name: *const libc::c_char,
        service_port: *mut libc::mach_port_t,
    ) -> libc::kern_return_t;

    fn mach_task_self() -> libc::mach_port_t;
    fn mach_port_deallocate(
        task: libc::mach_port_t,
        name: libc::mach_port_t,
    ) -> libc::kern_return_t;

    fn connectx(
        socket: libc::c_int,
        endpoints: *const TestSocketEndpoints,
        association_id: TestAssociationId,
        flags: libc::c_uint,
        vectors: *const libc::iovec,
        vector_count: libc::c_uint,
        bytes_written: *mut libc::size_t,
        connection_id: *mut TestConnectionId,
    ) -> libc::c_int;

    #[cfg_attr(target_arch = "x86_64", link_name = "fts_open$INODE64")]
    #[cfg_attr(not(target_arch = "x86_64"), link_name = "fts_open")]
    fn fts_open(
        paths: *const *mut libc::c_char,
        options: libc::c_int,
        compare: Option<
            unsafe extern "C" fn(
                *const *const libc::c_void,
                *const *const libc::c_void,
            ) -> libc::c_int,
        >,
    ) -> *mut libc::c_void;
    #[cfg_attr(target_arch = "x86_64", link_name = "fts_children$INODE64")]
    #[cfg_attr(not(target_arch = "x86_64"), link_name = "fts_children")]
    fn fts_children(stream: *mut libc::c_void, options: libc::c_int) -> *mut libc::c_void;
    #[cfg_attr(target_arch = "x86_64", link_name = "fts_read$INODE64")]
    #[cfg_attr(not(target_arch = "x86_64"), link_name = "fts_read")]
    fn fts_read(stream: *mut libc::c_void) -> *mut libc::c_void;
    #[cfg_attr(target_arch = "x86_64", link_name = "fts_close$INODE64")]
    #[cfg_attr(not(target_arch = "x86_64"), link_name = "fts_close")]
    fn fts_close(stream: *mut libc::c_void) -> libc::c_int;
}

#[cfg(target_os = "macos")]
#[test]
fn keychain_lookup_probe_child_process() {
    if std::env::var_os("AGORA_SANDBOX_TEST_KEYCHAIN_LOOKUP").is_none() {
        return;
    }
    let mut port = 0;
    let result = unsafe {
        bootstrap_look_up(
            bootstrap_port,
            c"com.apple.SecurityServer".as_ptr(),
            &mut port,
        )
    };
    assert_eq!(result, libc::KERN_SUCCESS);
    assert_ne!(port, libc::MACH_PORT_NULL as libc::mach_port_t);
    assert_eq!(
        unsafe { mach_port_deallocate(mach_task_self(), port) },
        libc::KERN_SUCCESS
    );
}

#[cfg(target_os = "macos")]
unsafe fn call_interposed_descriptor(
    name: &std::ffi::CStr,
    descriptor: libc::c_int,
) -> libc::c_int {
    type DescriptorFn = unsafe extern "C" fn(libc::c_int) -> libc::c_int;

    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    assert!(!symbol.is_null(), "missing interposed symbol {name:?}");
    let function = unsafe { std::mem::transmute::<*mut libc::c_void, DescriptorFn>(symbol) };
    unsafe { function(descriptor) }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn hook_library() -> PathBuf {
    static HOOK: OnceLock<PathBuf> = OnceLock::new();
    HOOK.get_or_init(|| {
        if std::env::var_os("CARGO_LLVM_COV").is_some() {
            let library = std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .join("libagora_sandbox.dylib");
            assert!(library.is_file(), "missing {}", library.display());
            return library;
        }
        let workdir = std::env::temp_dir().join(format!(
            "agora-sandbox-integration-hook-{}",
            std::process::id()
        ));
        agora_sandbox::hook_library::materialize(&workdir).unwrap()
    })
    .clone()
}

#[cfg(target_os = "macos")]
fn directory_contains(directory: &Path, needle: &[u8]) -> bool {
    std::fs::read_dir(directory).unwrap().any(|entry| {
        let path = entry.unwrap().path();
        if path.is_dir() {
            directory_contains(&path, needle)
        } else {
            std::fs::read(path)
                .map(|contents| {
                    contents
                        .windows(needle.len())
                        .any(|window| window == needle)
                })
                .unwrap_or(false)
        }
    })
}

#[cfg(target_os = "macos")]
fn sandbox_config() -> SandboxConfig {
    SandboxConfig::new(hook_library())
        .with_workdir(workspace_root().join(format!(
            "target/agora-sandbox-test-cache/runner-{}",
            uuid::Uuid::new_v4()
        )))
        .with_encrypted_workspace(FILESYSTEM_KEY)
}

#[cfg(target_os = "macos")]
fn sandbox_config_in(workdir: impl AsRef<Path>) -> SandboxConfig {
    SandboxConfig::new(hook_library())
        .with_workdir(workdir.as_ref())
        .with_encrypted_workspace(FILESYSTEM_KEY)
}

#[cfg(target_os = "macos")]
fn python3() -> PathBuf {
    let homebrew = PathBuf::from("/opt/homebrew/bin/python3");
    if homebrew.is_file() {
        homebrew
    } else {
        PathBuf::from("/usr/bin/python3")
    }
}

#[cfg(target_os = "macos")]
#[test]
fn unsupported_enforcement_fails_validation_and_default_tls_ca_is_allowed() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-default-ca-validation-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let hook = directory.join("hook.dylib");
    std::fs::write(&hook, b"hook").unwrap();
    let mut config = SandboxConfig::new(&hook).with_encrypted_workspace(FILESYSTEM_KEY);
    config.network.enforcement = NetworkEnforcement::Strict;
    let error = config.validate().unwrap_err();
    assert!(error.to_string().contains("strict network enforcement"));

    config.network.enforcement = NetworkEnforcement::Intercept;
    config.network.tls = TlsMode::Auto;
    assert!(config.validate().is_ok());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn runner_generates_default_tls_ca_in_the_configured_workdir() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-default-ca-test-{}",
        uuid::Uuid::new_v4()
    ));
    let command_workdir = directory.join("command");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&command_workdir).unwrap();
    let certificate = workdir.join("ca/ca.crt");
    let private_key = workdir.join("ca/ca.key");
    let mut config = sandbox_config_in(&workdir);
    config.network.tls = TlsMode::Auto;
    let command = SandboxCommand::new("/usr/bin/true").current_dir(&command_workdir);

    let outcome = Sandbox::new(config, NoopCallback)
        .run(command)
        .await
        .unwrap();

    assert!(outcome.status().success());
    assert!(
        std::fs::read_to_string(&certificate)
            .unwrap()
            .starts_with("-----BEGIN CERTIFICATE-----")
    );
    assert!(
        std::fs::read_to_string(&private_key)
            .unwrap()
            .starts_with("-----BEGIN PRIVATE KEY-----")
    );
    let trust_bundles = std::fs::read_dir(workdir.join("ca"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("trust-bundle-")
        })
        .collect::<Vec<_>>();
    assert!(trust_bundles.is_empty());
    assert!(!command_workdir.join("ca").exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn sandbox_allows_host_keychain_mach_lookup() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-keychain-boundary-test-{}",
        uuid::Uuid::new_v4()
    ));
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&directory).unwrap();
    let command = SandboxCommand::new(std::env::current_exe().unwrap())
        .arg("keychain_lookup_probe_child_process")
        .arg("--exact")
        .arg("--nocapture")
        .env("AGORA_SANDBOX_TEST_KEYCHAIN_LOOKUP", "1");

    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(command)
        .await
        .unwrap();

    assert!(outcome.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn runner_persists_an_encrypted_workspace_without_modifying_the_source() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-encrypted-workspace-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("input.txt"), b"original\n").unwrap();
    std::fs::write(
        source.join("verify.sh"),
        b"#!/bin/sh\ntest \"$(cat input.txt)\" = original && test \"$(cat output.txt)\" = 'encrypted workspace marker'\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(source.join("verify.sh"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(source.join("verify.sh"), permissions).unwrap();
    let config = SandboxConfig::new(hook_library())
        .with_workdir(&workdir)
        .with_encrypted_workspace("correct horse battery staple");
    let create = SandboxCommand::new("/bin/sh")
        .args([
            "-c",
            "test \"$(cat input.txt)\" = original && printf 'encrypted workspace marker\\n' > output.txt",
        ])
        .current_dir(&source);
    let created = Sandbox::new(config.clone(), NoopCallback)
        .run(create)
        .await
        .unwrap();

    assert!(
        created.status().success(),
        "sandbox child exited with {}",
        created.status()
    );
    assert_eq!(
        std::fs::read(source.join("input.txt")).unwrap(),
        b"original\n"
    );
    assert!(!source.join("output.txt").exists());
    assert!(!workdir.join("filesystem").exists());
    assert!(workdir.join("fs").is_dir());
    assert!(!directory_contains(
        &workdir.join("fs"),
        b"encrypted workspace marker"
    ));

    let verify = SandboxCommand::new("./verify.sh").current_dir(&source);
    let verified = Sandbox::new(config, NoopCallback)
        .run(verify)
        .await
        .unwrap();

    assert!(verified.status().success());
    assert!(!source.join("output.txt").exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn python_descriptor_writes_are_persisted_on_normal_exit() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-python-writeback-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    let script = "import os; fd=os.open('python.txt', os.O_WRONLY|os.O_CREAT|os.O_TRUNC, 0o640); assert os.write(fd,b'python-writeback') == 16";

    let written = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new(python3())
                .args(["-c", script])
                .current_dir(&source),
        )
        .await
        .unwrap();
    assert!(written.status().success());

    let verified = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new("/bin/sh")
                .args(["-c", "test \"$(cat python.txt)\" = python-writeback"])
                .current_dir(&source),
        )
        .await
        .unwrap();
    assert!(verified.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn exec_persists_open_encrypted_descriptors_before_replacing_the_process() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-exec-writeback-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    let script = "import os; fd=os.open('exec.txt', os.O_WRONLY|os.O_CREAT|os.O_TRUNC, 0o640); assert os.write(fd,b'exec-writeback') == 14; os.execv('/bin/sh', ['sh', '-c', 'test \"$(cat exec.txt)\" = exec-writeback'])";

    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new(python3())
                .args(["-c", script])
                .current_dir(&source),
        )
        .await
        .unwrap();

    assert!(outcome.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn descendant_exec_cannot_prepare_a_physical_workdir_executable() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-private-exec-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    let private_executable = workdir.join("private-echo");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::copy("/bin/echo", &private_executable).unwrap();
    let script = "import errno, os\ntry:\n os.execv(os.environ['PRIVATE_EXECUTABLE'], ['private-echo'])\nexcept OSError as error:\n assert error.errno == errno.EACCES\nelse:\n raise AssertionError('private executable unexpectedly ran')";

    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new(python3())
                .args(["-c", script])
                .env("PRIVATE_EXECUTABLE", &private_executable)
                .current_dir(&source),
        )
        .await
        .unwrap();

    assert!(outcome.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn encrypted_creates_apply_the_child_umask_to_logical_modes() {
    let directory =
        std::env::temp_dir().join(format!("agora-sandbox-umask-test-{}", uuid::Uuid::new_v4()));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    let command = "umask 077; : > private-file; mkdir private-dir; test \"$(/usr/bin/stat -f %Lp private-file)\" = 600; test \"$(/usr/bin/stat -f %Lp private-dir)\" = 700";

    let created = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new("/bin/sh")
                .args(["-c", command])
                .current_dir(&source),
        )
        .await
        .unwrap();
    assert!(created.status().success());

    let reopened = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new("/bin/sh")
                .args([
                    "-c",
                    "test \"$(/usr/bin/stat -f %Lp private-file)\" = 600; test \"$(/usr/bin/stat -f %Lp private-dir)\" = 700",
                ])
                .current_dir(&source),
        )
        .await
        .unwrap();
    assert!(reopened.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn encrypted_close_refreshes_the_logical_modification_time() {
    let directory =
        std::env::temp_dir().join(format!("agora-sandbox-mtime-test-{}", uuid::Uuid::new_v4()));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    let command = "printf first > mtime.txt; first=$(/usr/bin/stat -f %m mtime.txt); /bin/sleep 1; printf second >> mtime.txt; second=$(/usr/bin/stat -f %m mtime.txt); test \"$second\" -gt \"$first\"";

    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new("/bin/sh")
                .args(["-c", command])
                .current_dir(&source),
        )
        .await
        .unwrap();
    assert!(outcome.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn encrypted_reopen_after_shell_write_does_not_deadlock() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-file-lease-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    let run = Sandbox::new(sandbox_config_in(&workdir), NoopCallback).run(
        SandboxCommand::new("/bin/bash")
            .args([
                "-c",
                "printf lease-reopen > lease.txt; test \"$(cat lease.txt)\" = lease-reopen",
            ])
            .current_dir(&source),
    );

    let timeout = sandbox_lifecycle_timeout(15);
    let outcome = tokio::time::timeout(timeout, run)
        .await
        .expect("sandbox child deadlocked while reopening an encrypted file")
        .unwrap();
    assert!(outcome.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn lower_directory_reads_do_not_materialize_upper_directories() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-read-only-directory-test-{}",
        uuid::Uuid::new_v4()
    ));
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&directory).unwrap();
    let command = SandboxCommand::new(std::env::current_exe().unwrap())
        .arg("read_only_directory_child_process")
        .arg("--exact")
        .arg("--nocapture")
        .env("AGORA_SANDBOX_TEST_READ_ONLY_DIRECTORY", "1");

    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(command)
        .await
        .unwrap();
    assert!(outcome.status().success());
    assert!(!workdir.join("fs/usr").exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn system_ls_traverses_lower_directories_without_fts_errors() {
    let directory =
        std::env::temp_dir().join(format!("agora-sandbox-fts-test-{}", uuid::Uuid::new_v4()));
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&directory).unwrap();

    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(SandboxCommand::new("/bin/sh").args(["-c", "/bin/ls -la /usr/bin >/dev/null"]))
        .await
        .unwrap();
    assert!(outcome.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn device_paths_are_native_and_absent_from_filesystem_audit() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-device-passthrough-test-{}",
        uuid::Uuid::new_v4()
    ));
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&directory).unwrap();
    let paths = Arc::new(Mutex::new(Vec::new()));
    let callback_paths = Arc::clone(&paths);
    let callback = move |event| {
        if let Event::File(event) = event {
            callback_paths.lock().unwrap().push(event.file.path);
        }
        std::future::ready(Decision::Allow)
    };

    let outcome = Sandbox::new(sandbox_config_in(&workdir), callback)
        .run(
            SandboxCommand::new("/bin/sh")
                .args(["-c", "printf device-output >/dev/null && test -c /dev/null"]),
        )
        .await
        .unwrap();

    assert!(outcome.status().success());
    let paths = paths.lock().unwrap();
    assert!(
        paths
            .iter()
            .all(|path| path != "/dev" && !path.starts_with("/dev/")),
        "device paths unexpectedly reached filesystem audit: {paths:?}"
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn upper_only_entries_are_visible_to_system_ls() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-upper-ls-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("lower.txt"), b"lower").unwrap();
    std::fs::write(source.join("hidden.txt"), b"hidden").unwrap();

    let run = Sandbox::new(sandbox_config_in(&workdir), NoopCallback).run(
        SandboxCommand::new("/bin/bash")
            .args([
                "-c",
                "printf upper > upper.txt && test \"$(/bin/cat upper.txt)\" = upper && /bin/mkdir upper-dir && /bin/rm hidden.txt && listing=$(/bin/ls -1) && printf '%s\n' \"$listing\" | /usr/bin/grep -qx upper.txt && printf '%s\n' \"$listing\" | /usr/bin/grep -qx upper-dir && printf '%s\n' \"$listing\" | /usr/bin/grep -qx lower.txt && ! printf '%s\n' \"$listing\" | /usr/bin/grep -qx hidden.txt && long_listing=$(/bin/ls -l) && printf '%s\n' \"$long_listing\" | /usr/bin/grep -q ' upper.txt$' && printf '%s\n' \"$long_listing\" | /usr/bin/grep -q ' upper-dir$' && test \"$(/bin/ls -ln upper.txt | /usr/bin/awk '{print $5}')\" = 5 && test \"$(/bin/ls -d upper-dir)\" = upper-dir && /bin/ls -la upper-dir >/dev/null",
            ])
            .current_dir(&source),
    );
    let timeout = if std::env::var_os("CARGO_LLVM_COV").is_some() {
        Duration::from_secs(180)
    } else {
        Duration::from_secs(60)
    };
    let outcome = tokio::time::timeout(timeout, run)
        .await
        .expect("system ls deadlocked while traversing an upper-only directory")
        .unwrap();

    assert!(outcome.status().success());
    assert!(source.join("lower.txt").is_file());
    assert!(source.join("hidden.txt").is_file());
    assert!(!source.join("upper.txt").exists());
    assert!(!source.join("upper-dir").exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn upper_only_paths_can_be_canonicalized() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-realpath-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    let command = SandboxCommand::new(std::env::current_exe().unwrap())
        .arg("upper_only_canonicalize_child_process")
        .arg("--exact")
        .arg("--nocapture")
        .env("AGORA_SANDBOX_TEST_CANONICALIZE_UPPER", "1")
        .current_dir(&source);

    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(command)
        .await
        .unwrap();

    assert!(outcome.status().success());
    assert!(!source.join("upper-directory").exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn mkdir_p_accepts_existing_read_only_prefixes() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-mkdir-p-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    let created = source.join("upper/deep");
    std::fs::create_dir_all(&source).unwrap();

    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new("/bin/sh")
                .args(["-c", "/bin/mkdir -p \"$1\" && test -d \"$1\"", "mkdir-p"])
                .arg(&created),
        )
        .await
        .unwrap();

    assert!(outcome.status().success());
    assert!(!created.exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn path_helper_opens_fts_entries_relative_to_the_traversed_directory() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-path-helper-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();

    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new("/bin/sh")
                .args([
                    "-c",
                    "test -z \"$(/usr/libexec/path_helper -s 2>&1 >/dev/null)\"",
                ])
                .current_dir(&source),
        )
        .await
        .unwrap();

    assert!(outcome.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_second_open_sees_writes_from_a_live_encrypted_descriptor() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-live-writer-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();

    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new(python3())
                .args([
                    "-c",
                    "writer = open('session.jsonl', 'a'); writer.write('metadata'); writer.flush(); assert open('session.jsonl').read() == 'metadata'",
                ])
                .current_dir(&source),
        )
        .await
        .unwrap();

    assert!(outcome.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn sqlite_wal_transactions_survive_encrypted_reopen() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-sqlite-wal-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    let initialize = Sandbox::new(sandbox_config_in(&workdir), NoopCallback).run(
        SandboxCommand::new("/usr/bin/sqlite3")
            .args([
                "-batch",
                "-bail",
                "state.db",
                "PRAGMA journal_mode=WAL; CREATE TABLE entries(value TEXT NOT NULL); BEGIN IMMEDIATE; INSERT INTO entries VALUES('persisted'); COMMIT; PRAGMA wal_checkpoint(FULL);",
            ])
            .current_dir(&source),
    );

    let initialized = tokio::time::timeout(sandbox_lifecycle_timeout(20), initialize)
        .await
        .expect("SQLite WAL initialization timed out")
        .unwrap();
    assert!(initialized.status().success());

    let verified = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new("/bin/sh")
                .args([
                    "-c",
                    "test \"$(/usr/bin/sqlite3 -batch -bail state.db 'SELECT value FROM entries;')\" = persisted",
                ])
                .current_dir(&source),
        )
        .await
        .unwrap();

    assert!(verified.status().success());
    assert!(!source.join("state.db").exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn sqlite_wal_processes_share_locks_and_mapped_state() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-sqlite-lock-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    let script = r#"
import sqlite3
import subprocess
import sys

database = sqlite3.connect("state.db")
assert database.execute("PRAGMA journal_mode=WAL").fetchone()[0] == "wal"
database.execute("CREATE TABLE entries(value TEXT NOT NULL)")
database.commit()
database.close()

writer_script = r'''
import sqlite3
import sys

writer = sqlite3.connect("state.db", timeout=10)
writer.execute("BEGIN IMMEDIATE")
writer.execute("INSERT INTO entries VALUES('committed')")
print("READY", flush=True)
assert sys.stdin.buffer.read(1) == b"0"
print("LOCKED", flush=True)
assert sys.stdin.buffer.read(1) == b"1"
writer.commit()
writer.close()
'''
writer = subprocess.Popen(
    [sys.executable, "-c", writer_script],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
)
ready = writer.stdout.readline()
assert ready == b"READY\n", (ready, writer.stderr.read())
writer.stdin.write(b"0")
writer.stdin.flush()
locked = writer.stdout.readline()
assert locked == b"LOCKED\n", (locked, writer.stderr.read())
contender = sqlite3.connect("state.db", timeout=0)
try:
    contender.execute("BEGIN IMMEDIATE")
except sqlite3.OperationalError as error:
    assert "locked" in str(error).lower()
else:
    raise AssertionError("a second process acquired SQLite's exclusive writer lock")
assert contender.execute("SELECT count(*) FROM entries").fetchone()[0] == 0
contender.close()
writer.stdin.write(b"1")
writer.stdin.close()
assert writer.wait(timeout=15) == 0, writer.stderr.read()

verified = sqlite3.connect("state.db")
assert verified.execute("SELECT value FROM entries").fetchone()[0] == "committed"
assert verified.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
verified.execute("PRAGMA wal_checkpoint(TRUNCATE)")
verified.close()
"#;

    let run = Sandbox::new(sandbox_config_in(&workdir), NoopCallback).run(
        SandboxCommand::new(python3())
            .args(["-c", script])
            .current_dir(&source),
    );
    let outcome = tokio::time::timeout(sandbox_lifecycle_timeout(30), run)
        .await
        .expect("concurrent SQLite WAL test timed out")
        .unwrap();

    assert!(outcome.status().success());
    assert!(!source.join("state.db").exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn encrypted_opens_share_file_identity_locks_and_mappings() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-shared-inode-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    let script = r#"
import fcntl
import ctypes
import mmap
import os

path = "shared.bin"
first = os.open(path, os.O_CREAT | os.O_TRUNC | os.O_RDWR, 0o600)
assert os.write(first, b"abcdef") == 6
second = os.open(path, os.O_RDWR)
logical = os.stat(path)
for opened in (os.fstat(first), os.fstat(second)):
    assert (opened.st_dev, opened.st_ino) == (logical.st_dev, logical.st_ino)

os.ftruncate(first, 4096)
first_mapping = mmap.mmap(first, 4096, access=mmap.ACCESS_WRITE)
second_mapping = mmap.mmap(second, 4096, access=mmap.ACCESS_WRITE)
first_mapping[:6] = b"shared"
first_mapping.flush()
assert second_mapping[:6] == b"shared"

fcntl.lockf(first, fcntl.LOCK_EX | fcntl.LOCK_NB, 1, 0)
child = os.fork()
if child == 0:
    try:
        fcntl.lockf(second, fcntl.LOCK_EX | fcntl.LOCK_NB, 1, 0)
    except BlockingIOError:
        os._exit(0)
    os._exit(1)
_, status = os.waitpid(child, 0)
assert os.waitstatus_to_exitcode(status) == 0
fcntl.lockf(first, fcntl.LOCK_UN, 1, 0)

duplicate = os.dup(first)
fcntl.lockf(first, fcntl.LOCK_EX | fcntl.LOCK_NB, 1, 0)
os.close(duplicate)
child = os.fork()
if child == 0:
    try:
        fcntl.lockf(second, fcntl.LOCK_EX | fcntl.LOCK_NB, 1, 0)
    except BlockingIOError:
        os._exit(1)
    os._exit(0)
_, status = os.waitpid(child, 0)
assert os.waitstatus_to_exitcode(status) == 0

duplicate = os.dup(first)
fcntl.flock(first, fcntl.LOCK_EX | fcntl.LOCK_NB)
os.close(duplicate)
child = os.fork()
if child == 0:
    try:
        fcntl.flock(second, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os._exit(0)
    os._exit(1)
_, status = os.waitpid(child, 0)
assert os.waitstatus_to_exitcode(status) == 0

fcntl.flock(first, fcntl.LOCK_UN)
first_mapping.close()
second_mapping.close()
os.close(first)
os.close(second)

mapped = os.open(path, os.O_RDWR)
contender = os.open(path, os.O_RDWR)
libc = ctypes.CDLL(None, use_errno=True)
libc.mmap.argtypes = [
    ctypes.c_void_p,
    ctypes.c_size_t,
    ctypes.c_int,
    ctypes.c_int,
    ctypes.c_int,
    ctypes.c_longlong,
]
libc.mmap.restype = ctypes.c_void_p
address = libc.mmap(
    None,
    4096,
    mmap.PROT_READ | mmap.PROT_WRITE,
    mmap.MAP_SHARED,
    mapped,
    0,
)
assert address != ctypes.c_void_p(-1).value
fcntl.flock(mapped, fcntl.LOCK_EX | fcntl.LOCK_NB)
os.close(mapped)
child = os.fork()
if child == 0:
    try:
        fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os._exit(1)
    os._exit(0)
_, status = os.waitpid(child, 0)
assert os.waitstatus_to_exitcode(status) == 0
ctypes.memmove(address, b"S", 1)
assert libc.msync(ctypes.c_void_p(address), 4096, 0x10) == 0
assert os.pread(contender, 1, 0) == b"S"
assert libc.munmap(ctypes.c_void_p(address), 4096) == 0
os.close(contender)
"#;

    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new(python3())
                .args(["-c", script])
                .current_dir(&source),
        )
        .await
        .unwrap();

    assert!(outcome.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn system_rm_removes_lower_entries_from_an_encrypted_workspace() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-system-rm-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(source.join("examples/sandbox-go")).unwrap();
    std::fs::write(source.join("removed.txt"), b"lower file").unwrap();
    std::fs::write(source.join("examples/sandbox-go/main.go"), b"package main").unwrap();

    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new("/bin/bash")
                .args([
                    "-c",
                    "cd \"$1\" && /bin/rm -f removed.txt && test ! -e removed.txt && test -z \"$(/bin/ls -1 . | /usr/bin/grep '^removed\\.txt$')\" && /bin/rm -rf examples/ && test ! -e examples && test -z \"$(/bin/ls -1 . | /usr/bin/grep '^examples$')\"",
                    "rm-regression",
                ])
                .arg(&source),
        )
        .await
        .unwrap();

    assert!(outcome.status().success());
    assert!(source.join("removed.txt").is_file());
    assert!(source.join("examples/sandbox-go/main.go").is_file());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn system_ls_hides_encrypted_whiteouts() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-system-ls-whiteout-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("removed.txt"), b"lower file").unwrap();

    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new("/bin/sh")
                .args([
                    "-c",
                    "/bin/rm -f removed.txt && test -z \"$(/bin/ls -1 . | /usr/bin/grep '^removed\\.txt$')\"",
                ])
                .current_dir(&source),
        )
        .await
        .unwrap();

    assert!(outcome.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn external_upper_removal_reveals_lower_in_a_running_child() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-external-upper-file-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    let logical = source.join("value.txt");
    let release = source.join("release");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(&logical, b"lower").unwrap();

    let created = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new("/bin/sh")
                .args(["-c", "printf upper > value.txt"])
                .current_dir(&source),
        )
        .await
        .unwrap();
    assert!(created.status().success());

    let backing_directory = workdir
        .join("fs")
        .join(source.canonicalize().unwrap().strip_prefix("/").unwrap());
    let upper = std::fs::read_dir(&backing_directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b"enc_"))
        })
        .unwrap();
    let first_read = Arc::new(tokio::sync::Notify::new());
    let callback = {
        let first_read = Arc::clone(&first_read);
        move |event| {
            if matches!(
                event,
                Event::File(ref event)
                    if event.event_type == EventType::FilesystemClose
                        && Path::new(&event.file.path).ends_with("value.txt")
                        && event.file.mode.access == FileAccessMode::Read
            ) {
                first_read.notify_one();
            }
            std::future::ready(Decision::Allow)
        }
    };
    let child = tokio::spawn(
        Sandbox::new(sandbox_config_in(&workdir), callback).run(
            SandboxCommand::new("/bin/sh")
                .args([
                    "-c",
                    "test \"$(cat value.txt)\" = upper && until test -e release; do /bin/sleep 0.01; done && test \"$(cat value.txt)\" = lower",
                ])
                .current_dir(&source),
        ),
    );

    tokio::time::timeout(sandbox_lifecycle_timeout(30), first_read.notified())
        .await
        .expect("sandbox child did not finish its first upper read");
    std::fs::remove_file(&upper).unwrap();
    std::fs::write(&release, b"continue").unwrap();

    let outcome = child.await.unwrap().unwrap();
    assert!(outcome.status().success());
    assert_eq!(std::fs::read(&logical).unwrap(), b"lower");
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn system_ls_observes_external_upper_directory_removal() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-external-upper-directory-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    let logical_directory = source.join("view");
    let release = source.join("release");
    std::fs::create_dir_all(&logical_directory).unwrap();
    std::fs::write(logical_directory.join("lower.txt"), b"lower").unwrap();

    let created = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new("/bin/sh")
                .args([
                    "-c",
                    "/bin/rm -rf view && /bin/mkdir view && printf upper > view/upper.txt",
                ])
                .current_dir(&source),
        )
        .await
        .unwrap();
    assert!(created.status().success());

    let upper_directory = workdir.join("fs").join(
        logical_directory
            .canonicalize()
            .unwrap()
            .strip_prefix("/")
            .unwrap(),
    );
    let ready_for_removal = Arc::new(tokio::sync::Notify::new());
    let callback = {
        let ready_for_removal = Arc::clone(&ready_for_removal);
        move |event| {
            if matches!(
                event,
                Event::Process(ref event)
                    if event.event_type == EventType::ProcessExecAttempt
                        && event.command.executable == "/bin/sleep"
            ) {
                ready_for_removal.notify_one();
            }
            std::future::ready(Decision::Allow)
        }
    };
    let child = tokio::spawn(
        Sandbox::new(sandbox_config_in(&workdir), callback).run(
            SandboxCommand::new("/bin/sh")
                .args([
                    "-c",
                    "test \"$(cat view/upper.txt)\" = upper && until test -e release; do /bin/sleep 0.01; done && /bin/ls -1 view | /usr/bin/grep -qx lower.txt && test -z \"$(/bin/ls -1 view | /usr/bin/grep '^upper\\.txt$')\"",
                ])
                .current_dir(&source),
        ),
    );

    tokio::time::timeout(sandbox_lifecycle_timeout(30), ready_for_removal.notified())
        .await
        .expect("sandbox child did not finish its first upper directory read and close");
    std::fs::remove_dir_all(&upper_directory).unwrap();
    std::fs::write(&release, b"continue").unwrap();

    let outcome = child.await.unwrap().unwrap();
    assert!(outcome.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn symlinks_created_in_an_encrypted_workspace_are_visible() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-symlink-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("target.txt"), b"symlink target").unwrap();

    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new("/bin/sh")
                .args([
                    "-c",
                    "/bin/ln -s target.txt alias.txt && test \"$(cat alias.txt)\" = 'symlink target' && test \"$(readlink alias.txt)\" = target.txt && /bin/ls -1 | /usr/bin/grep -qx alias.txt && /bin/mkdir upper-only && /bin/ln -s ../target.txt upper-only/nested-alias.txt && /bin/ls -1 upper-only | /usr/bin/grep -qx nested-alias.txt",
                ])
                .current_dir(&source),
        )
        .await
        .unwrap();

    assert!(outcome.status().success());
    assert!(!source.join("alias.txt").exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn encrypted_file_leaf_names_are_not_stored_as_plaintext() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-encrypted-name-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new("/bin/sh")
                .args(["-c", "printf hidden-name > '安全方案.docx'"])
                .current_dir(&source),
        )
        .await
        .unwrap();
    assert!(outcome.status().success());

    let backing_directory = workdir
        .join("fs")
        .join(source.canonicalize().unwrap().strip_prefix("/").unwrap());
    assert!(!backing_directory.join("安全方案.docx").exists());
    let physical_names = std::fs::read_dir(&backing_directory)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .filter(|name| name.as_encoded_bytes().starts_with(b"enc_"))
        .collect::<Vec<_>>();
    assert_eq!(physical_names.len(), 1);
    let contents = std::fs::read(backing_directory.join(".metadata")).unwrap();
    assert!(
        !contents
            .windows("安全方案.docx".len())
            .any(|part| { part == "安全方案.docx".as_bytes() })
    );
    let metadata: serde_json::Value = serde_json::from_slice(&contents).unwrap();
    assert_eq!(metadata["version"], 3);
    assert!(metadata.get("backing_names").is_none());
    let record_key = metadata["entries"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap();
    assert_eq!(physical_names[0], std::ffi::OsStr::new(record_key));
    assert!(metadata["entries"][record_key].get("name").is_none());

    let reopened = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new("/bin/sh")
                .args(["-c", "test \"$(cat '安全方案.docx')\" = hidden-name"])
                .current_dir(&source),
        )
        .await
        .unwrap();
    assert!(reopened.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn sip_executable_copies_are_persistent_under_the_filesystem_root() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-persistent-executable-test-{}",
        uuid::Uuid::new_v4()
    ));
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&directory).unwrap();
    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(SandboxCommand::new("/usr/bin/true"))
        .await
        .unwrap();
    assert!(outcome.status().success());
    assert!(workdir.join("fs/usr/bin/true").is_file());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn encrypted_upper_executables_never_fall_back_to_stale_lower_contents() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-upper-executable-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    let executable = source.join("tool.sh");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(&executable, b"#!/bin/sh\nexit 41\n").unwrap();
    let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&executable, permissions).unwrap();

    let replaced = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(
            SandboxCommand::new("/bin/sh")
                .args([
                    "-c",
                    "printf '#!/bin/sh\\nexit 42\\n' > tool.sh; chmod 755 tool.sh",
                ])
                .current_dir(&source),
        )
        .await
        .unwrap();
    assert!(replaced.status().success());

    let executed = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(SandboxCommand::new(&executable).current_dir(&source))
        .await;
    if let Ok(outcome) = executed {
        assert_eq!(outcome.status().code(), Some(42));
    }
    assert_eq!(std::fs::read(&executable).unwrap(), b"#!/bin/sh\nexit 41\n");
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn encrypted_workspace_remains_ciphertext_while_the_child_is_running() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-runtime-ciphertext-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    let output = source.join("runtime-secret.txt");
    let marker = format!("runtime-secret-{}", uuid::Uuid::new_v4());
    std::fs::create_dir_all(&source).unwrap();

    let closed = Arc::new(tokio::sync::Notify::new());
    let callback = {
        let closed = Arc::clone(&closed);
        let output = output.to_string_lossy().into_owned();
        move |event| {
            if matches!(
                event,
                Event::File(ref event)
                    if event.event_type == EventType::FilesystemClose
                        && event.file.path == output
            ) {
                closed.notify_one();
            }
            std::future::ready(Decision::Allow)
        }
    };
    let config = SandboxConfig::new(hook_library())
        .with_workdir(&workdir)
        .with_encrypted_workspace(FILESYSTEM_KEY);
    let script = format!(
        "printf '%s' '{marker}' > '{}'; sleep 2; test \"$(cat '{}')\" = '{marker}'",
        output.display(),
        output.display()
    );
    let run = tokio::spawn(
        Sandbox::new(config, callback).run(
            SandboxCommand::new("/bin/bash")
                .args(["-c", script.as_str()])
                .current_dir(&source),
        ),
    );

    tokio::time::timeout(sandbox_lifecycle_timeout(30), closed.notified())
        .await
        .expect("sandbox child did not close the encrypted output file");
    assert!(
        !directory_contains(&workdir, marker.as_bytes()),
        "sandbox backing storage exposed plaintext while the child was running"
    );

    let outcome = run.await.unwrap().unwrap();
    assert!(outcome.status().success());
    assert!(!output.exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn runner_persists_a_plain_workspace_without_modifying_the_source() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-plain-workspace-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("input.txt"), b"original\n").unwrap();
    let config = SandboxConfig::new(hook_library())
        .with_workdir(&workdir)
        .with_plain_workspace();
    let create = SandboxCommand::new("/bin/sh")
        .args([
            "-c",
            "test \"$(cat input.txt)\" = original && printf 'plain workspace marker\\n' > output.txt",
        ])
        .current_dir(&source);
    let created = Sandbox::new(config.clone(), NoopCallback)
        .run(create)
        .await
        .unwrap();

    assert!(created.status().success());
    assert_eq!(
        std::fs::read(source.join("input.txt")).unwrap(),
        b"original\n"
    );
    assert!(!source.join("output.txt").exists());
    let persisted = workdir
        .join("fs")
        .join(source.canonicalize().unwrap().strip_prefix("/").unwrap())
        .join("output.txt");
    assert_eq!(
        std::fs::read(&persisted).unwrap(),
        b"plain workspace marker\n"
    );
    let verify = SandboxCommand::new("/bin/sh")
        .args([
            "-c",
            "test \"$(cat input.txt)\" = original && test \"$(cat output.txt)\" = 'plain workspace marker'",
        ])
        .current_dir(&source);
    let verified = Sandbox::new(config, NoopCallback)
        .run(verify)
        .await
        .unwrap();

    assert!(verified.status().success());
    assert!(!source.join("output.txt").exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn logical_permissions_match_in_plain_and_encrypted_workspaces() {
    for encrypted in [false, true] {
        let mode = if encrypted { "encrypted" } else { "plain" };
        let directory = std::env::temp_dir().join(format!(
            "agora-sandbox-{mode}-permission-test-{}",
            uuid::Uuid::new_v4()
        ));
        let source = directory.join("source");
        let workdir = directory.join("sandbox");
        std::fs::create_dir_all(&source).unwrap();
        for name in ["denied.txt", "writable.txt", "removable.txt"] {
            let path = source.join(name);
            std::fs::write(&path, format!("host-{name}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        }
        let config = SandboxConfig::new(hook_library()).with_workdir(&workdir);
        let config = if encrypted {
            config.with_encrypted_workspace(FILESYSTEM_KEY)
        } else {
            config.with_plain_workspace()
        };
        let exercise = SandboxCommand::new("/bin/sh")
            .args([
                "-c",
                "set -e
                 if (printf bad > denied.txt) 2>/dev/null; then exit 10; fi
                 test \"$(cat denied.txt)\" = host-denied.txt
                 chmod 644 writable.txt
                 printf sandbox-upper > writable.txt
                 rm removable.txt
                 mkdir locked
                 printf child > locked/child
                 chmod 000 locked
                 if cat locked/child >/dev/null 2>&1; then exit 11; fi
                 if /usr/bin/stat -f %z locked/child >/dev/null 2>&1; then exit 12; fi
                 if (printf new > locked/new) 2>/dev/null; then exit 13; fi
                 chmod 700 locked
                 printf agora-dev-passthrough-7f1a9c2e >/dev/null",
            ])
            .current_dir(&source);

        let exercised = Sandbox::new(config.clone(), NoopCallback)
            .run(exercise)
            .await
            .unwrap();
        assert!(
            exercised.status().success(),
            "{mode} permission exercise failed: {}",
            exercised.status()
        );
        assert_eq!(
            std::fs::read(source.join("denied.txt")).unwrap(),
            b"host-denied.txt\n"
        );
        assert_eq!(
            std::fs::read(source.join("writable.txt")).unwrap(),
            b"host-writable.txt\n"
        );
        assert_eq!(
            std::fs::metadata(source.join("writable.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o444
        );
        assert!(source.join("removable.txt").is_file());
        assert!(!workdir.join("fs/dev").exists());
        assert!(!directory_contains(
            &workdir,
            b"agora-dev-passthrough-7f1a9c2e"
        ));

        let verify = SandboxCommand::new("/bin/sh")
            .args([
                "-c",
                "set -e
                 test \"$(cat denied.txt)\" = host-denied.txt
                 test \"$(cat writable.txt)\" = sandbox-upper
                 test ! -e removable.txt
                 test \"$(cat locked/child)\" = child",
            ])
            .current_dir(&source);
        let verified = Sandbox::new(config, NoopCallback)
            .run(verify)
            .await
            .unwrap();
        assert!(
            verified.status().success(),
            "{mode} permission verification failed: {}",
            verified.status()
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn runner_rejects_concurrent_plain_sandboxes_in_the_same_workdir() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-plain-lock-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    let config = SandboxConfig::new(hook_library())
        .with_workdir(&workdir)
        .with_plain_workspace();
    let first = tokio::spawn(
        Sandbox::new(config.clone(), NoopCallback).run(
            SandboxCommand::new("/bin/sleep")
                .arg("1")
                .current_dir(&source),
        ),
    );
    let lock = workdir.join("fs/.fs.lock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !lock.exists() {
        assert!(
            Instant::now() < deadline,
            "plain filesystem lock was not created"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;

    let error = Sandbox::new(config, NoopCallback)
        .run(SandboxCommand::new("/usr/bin/true").current_dir(&source))
        .await
        .unwrap_err();

    assert!(error.to_string().contains("filesystem is already in use"));
    assert!(first.await.unwrap().unwrap().status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn runner_interposes_the_complete_filesystem_operation_set() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-filesystem-hook-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("source.txt"), b"host").unwrap();
    std::fs::set_permissions(
        source.join("source.txt"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    std::os::unix::fs::symlink("source.txt", source.join("source-link")).unwrap();
    let source_modified = source
        .join("source.txt")
        .metadata()
        .unwrap()
        .modified()
        .unwrap();
    let command = SandboxCommand::new(std::env::current_exe().unwrap())
        .arg("filesystem_interposed_child_process")
        .arg("--exact")
        .arg("--nocapture")
        .current_dir(&source)
        .env("AGORA_SANDBOX_TEST_FILESYSTEM_CHILD", &source);

    let events = Arc::new(Mutex::new(Vec::<FileEvent>::new()));
    let callback = {
        let events = Arc::clone(&events);
        move |event: Event| {
            if let Event::File(event) = event {
                events.lock().unwrap().push(event);
            }
            std::future::ready(Decision::Allow)
        }
    };
    let outcome = Sandbox::new(sandbox_config_in(&workdir), callback)
        .run(command)
        .await
        .unwrap();

    assert!(outcome.status().success());
    let source_path = source.join("source.txt").to_string_lossy().into_owned();
    let source_events = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event.file.path == source_path)
        .cloned()
        .collect::<Vec<_>>();
    assert!(source_events.iter().any(|event| {
        event.event_type == EventType::FilesystemOpen
            && event.file.mode.access == FileAccessMode::Read
            && !event.trace_id.is_empty()
    }));
    assert!(source_events.iter().any(|event| {
        event.event_type == EventType::FilesystemClose
            && event.file.mode.access == FileAccessMode::Read
            && !event.trace_id.is_empty()
    }));
    assert_eq!(std::fs::read(source.join("source.txt")).unwrap(), b"host");
    assert_eq!(
        source
            .join("source.txt")
            .metadata()
            .unwrap()
            .modified()
            .unwrap(),
        source_modified
    );
    assert_eq!(
        source
            .join("source.txt")
            .metadata()
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
    assert_eq!(
        std::fs::read_link(source.join("source-link")).unwrap(),
        Path::new("source.txt")
    );
    assert!(!source.join("renamed-link").exists());
    for path in [
        "created.txt",
        "creat.txt",
        "spawn.txt",
        "renamed-at.txt",
        "created-at",
        "created",
        "vectored.txt",
        "mapped.bin",
    ] {
        assert!(!source.join(path).exists());
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn runner_rejects_a_different_encrypted_workspace_key() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-encrypted-workspace-key-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    std::fs::create_dir_all(&source).unwrap();

    let config = SandboxConfig::new(hook_library())
        .with_workdir(&workdir)
        .with_encrypted_workspace("original passphrase");
    let command = SandboxCommand::new("/usr/bin/true").current_dir(&source);
    assert!(
        Sandbox::new(config, NoopCallback)
            .run(command)
            .await
            .unwrap()
            .status()
            .success()
    );

    let wrong_config = SandboxConfig::new(hook_library())
        .with_workdir(&workdir)
        .with_encrypted_workspace("different passphrase");
    let error = Sandbox::new(wrong_config, NoopCallback)
        .run(SandboxCommand::new("/usr/bin/true").current_dir(&source))
        .await
        .unwrap_err();

    assert!(
        error.to_string().contains("filesystem key is incorrect"),
        "unexpected error: {error:#}"
    );
    assert!(workdir.join("fs/.key.json").is_file());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn runner_rejects_a_malformed_tls_ca_before_starting_the_child() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-malformed-ca-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let certificate = directory.join("ca.pem");
    let private_key = directory.join("ca-key.pem");
    let marker = directory.join("child-started");
    std::fs::write(&certificate, b"not a certificate").unwrap();
    std::fs::write(&private_key, b"not a private key").unwrap();
    let mut config = sandbox_config().with_tls_ca(&certificate, &private_key);
    config.network.tls = TlsMode::Auto;
    let command = SandboxCommand::new("/bin/sh")
        .arg("-c")
        .arg(format!("touch {}", marker.display()));

    let error = Sandbox::new(config, NoopCallback)
        .run(command)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("TLS CA certificate"));
    assert!(!marker.exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn runner_propagates_child_exit_status() {
    let sandbox = Sandbox::new(sandbox_config(), NoopCallback);
    let command = SandboxCommand::new(std::env::current_exe().unwrap())
        .arg("exits_with_seven")
        .arg("--exact")
        .arg("--nocapture")
        .env("AGORA_SANDBOX_TEST_EXIT_SEVEN", "1");
    let outcome = sandbox.run(command).await.unwrap();

    assert_eq!(outcome.status().code(), Some(7));
    assert!(!outcome.sandbox_id().is_empty());
    assert!(!outcome.run_id().is_empty());
}

#[test]
fn exits_with_seven() {
    if std::env::var_os("AGORA_SANDBOX_TEST_EXIT_SEVEN").is_some() {
        std::process::exit(7);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn records_current_executable() {
    let Some(expected) = std::env::var_os("AGORA_SANDBOX_TEST_CURRENT_EXE") else {
        return;
    };
    assert_eq!(
        std::env::current_exe().unwrap().canonicalize().unwrap(),
        PathBuf::from(expected).canonicalize().unwrap()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn relocated_executable_can_inspect_its_current_path() {
    if std::env::var_os("AGORA_SANDBOX_TEST_CURRENT_EXE_ACCESS").is_none() {
        return;
    }
    let executable = std::env::current_exe().unwrap();
    let mut file = std::fs::File::open(&executable).unwrap();
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic).unwrap();
    assert!(matches!(
        u32::from_be_bytes(magic),
        0xcafebabe | 0xcafebabf | 0xcefaedfe | 0xcffaedfe
    ));
    assert!(
        std::fs::read_dir(executable.parent().unwrap())
            .unwrap()
            .any(|entry| entry.unwrap().file_name() == executable.file_name().unwrap())
    );
}

#[cfg(target_os = "macos")]
#[test]
fn relocated_executable_can_read_and_load_sibling_resources() {
    if std::env::var_os("AGORA_SANDBOX_TEST_RELOCATED_PACKAGE").is_none() {
        return;
    }
    let filesystem_root =
        PathBuf::from(std::env::var_os("AGORA_SANDBOX_TEST_FILESYSTEM_ROOT").unwrap())
            .canonicalize()
            .unwrap();
    let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
    assert!(executable.starts_with(&filesystem_root));
    let contents = executable.parent().unwrap().parent().unwrap();
    let package = contents.parent().unwrap();

    assert_eq!(
        std::fs::read_to_string(contents.join("Info.plist")).unwrap(),
        "agora relocated resource\n"
    );

    let library = std::ffi::CString::new(
        contents
            .join("PlugIns/libfixture.dylib")
            .as_os_str()
            .as_encoded_bytes(),
    )
    .unwrap();
    let handle = unsafe { libc::dlopen(library.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        let error = unsafe { libc::dlerror() };
        let message = if error.is_null() {
            "unknown loader error".into()
        } else {
            unsafe { std::ffi::CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        };
        panic!("failed to load relocated sibling: {message}");
    }
    let symbol = unsafe { libc::dlsym(handle, c"agora_fixture_value".as_ptr()) };
    assert!(!symbol.is_null());
    let fixture_value = unsafe {
        std::mem::transmute::<*mut libc::c_void, unsafe extern "C" fn() -> libc::c_int>(symbol)
    };
    assert_eq!(unsafe { fixture_value() }, 42);
    assert_eq!(unsafe { libc::dlclose(handle) }, 0);
    assert_eq!(package.file_name().unwrap(), "Relocated.app");
}

#[cfg(target_os = "macos")]
#[test]
fn relocated_executable_spawns_its_sibling_and_preserves_missing_errno() {
    let Some(role) = std::env::var_os("AGORA_SANDBOX_TEST_RELOCATED_SIBLING") else {
        return;
    };
    if role == "sibling" {
        return;
    }
    assert_eq!(role, "primary");

    let missing = Command::new("/missing/agora-executable")
        .status()
        .unwrap_err();
    assert_eq!(missing.raw_os_error(), Some(libc::ENOENT));

    let sibling = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("sibling");
    let status = Command::new(sibling)
        .arg("relocated_executable_spawns_its_sibling_and_preserves_missing_errno")
        .arg("--exact")
        .arg("--nocapture")
        .env("AGORA_SANDBOX_TEST_RELOCATED_SIBLING", "sibling")
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(target_os = "macos")]
#[test]
fn process_audit_does_not_reject_a_large_argument() {
    if std::env::var_os("AGORA_SANDBOX_TEST_LARGE_ARGUMENT").is_none() {
        return;
    }
    let status = Command::new("/usr/bin/true")
        .arg("x".repeat(70 * 1024))
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(target_os = "macos")]
#[test]
fn cloexec_default_spawn_child_process() {
    if std::env::var_os("AGORA_SANDBOX_TEST_CLOEXEC_SPAWN").is_none() {
        return;
    }

    unsafe extern "C" {
        fn posix_spawn_file_actions_addinherit_np(
            actions: *mut libc::posix_spawn_file_actions_t,
            descriptor: libc::c_int,
        ) -> libc::c_int;
    }

    const POSIX_SPAWN_CLOEXEC_DEFAULT: libc::c_short = 0x4000;
    let mut attributes: libc::posix_spawnattr_t = std::ptr::null_mut();
    assert_eq!(unsafe { libc::posix_spawnattr_init(&mut attributes) }, 0);
    assert_eq!(
        unsafe { libc::posix_spawnattr_setflags(&mut attributes, POSIX_SPAWN_CLOEXEC_DEFAULT) },
        0
    );
    let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
    assert_eq!(
        unsafe { libc::posix_spawn_file_actions_init(&mut actions) },
        0
    );
    for descriptor in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        if unsafe { libc::fcntl(descriptor, libc::F_GETFD) } >= 0 {
            assert_eq!(
                unsafe { posix_spawn_file_actions_addinherit_np(&mut actions, descriptor) },
                0
            );
        }
    }
    let arguments = [
        c"/bin/bash".as_ptr(),
        c"-lc".as_ptr(),
        c"printf cloexec-control-ok > \"$AGORA_SANDBOX_TEST_CLOEXEC_PATH\" && test \"$(cat \"$AGORA_SANDBOX_TEST_CLOEXEC_PATH\")\" = cloexec-control-ok".as_ptr(),
        std::ptr::null(),
    ];
    let environment = unsafe { *libc::_NSGetEnviron() };
    let mut pid = 0;
    let result = unsafe {
        libc::posix_spawn(
            &mut pid,
            arguments[0],
            &actions,
            &attributes,
            arguments.as_ptr().cast_mut().cast(),
            environment,
        )
    };
    assert_eq!(
        unsafe { libc::posix_spawn_file_actions_destroy(&mut actions) },
        0
    );
    assert_eq!(unsafe { libc::posix_spawnattr_destroy(&mut attributes) }, 0);
    assert_eq!(result, 0);

    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status));
    assert_eq!(libc::WEXITSTATUS(status), 0);
}

#[cfg(target_os = "macos")]
#[test]
fn records_tls_trust_environment() {
    let Some(private_workdir) = std::env::var_os("AGORA_SANDBOX_TEST_TLS_TRUST_ENV") else {
        return;
    };
    let values = TLS_TRUST_ENVIRONMENT
        .iter()
        .map(|key| PathBuf::from(std::env::var_os(key).unwrap()))
        .collect::<Vec<_>>();
    assert!(values.iter().all(|path| path == &values[0]));
    assert!(values[0].is_file());
    assert!(!values[0].starts_with(private_workdir));
    assert!(
        values[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("trust-bundle-")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn filesystem_interposed_child_process() {
    let Some(root) = std::env::var_os("AGORA_SANDBOX_TEST_FILESYSTEM_CHILD") else {
        return;
    };
    let root = PathBuf::from(root);
    let source =
        std::ffi::CString::new(root.join("source.txt").as_os_str().as_encoded_bytes()).unwrap();
    let created =
        std::ffi::CString::new(root.join("created").as_os_str().as_encoded_bytes()).unwrap();
    let renamed = std::ffi::CString::new(
        root.join("created/renamed.txt")
            .as_os_str()
            .as_encoded_bytes(),
    )
    .unwrap();
    let creat =
        std::ffi::CString::new(root.join("creat.txt").as_os_str().as_encoded_bytes()).unwrap();
    let spawn =
        std::ffi::CString::new(root.join("spawn.txt").as_os_str().as_encoded_bytes()).unwrap();
    let vectored =
        std::ffi::CString::new(root.join("vectored.txt").as_os_str().as_encoded_bytes()).unwrap();
    let mapped =
        std::ffi::CString::new(root.join("mapped.bin").as_os_str().as_encoded_bytes()).unwrap();
    let source_link =
        std::ffi::CString::new(root.join("source-link").as_os_str().as_encoded_bytes()).unwrap();
    let renamed_link =
        std::ffi::CString::new(root.join("renamed-link").as_os_str().as_encoded_bytes()).unwrap();

    unsafe {
        assert_eq!(libc::access(std::ptr::null(), libc::R_OK), -1);
        assert_eq!(libc::open(std::ptr::null(), libc::O_RDONLY), -1);

        assert_eq!(libc::access(source.as_ptr(), libc::R_OK), 0);
        let mut status = std::mem::MaybeUninit::<libc::stat>::zeroed();
        assert_eq!(libc::stat(std::ptr::null(), status.as_mut_ptr()), -1);
        assert_eq!(libc::lstat(std::ptr::null(), status.as_mut_ptr()), -1);
        assert_eq!(libc::stat(source.as_ptr(), status.as_mut_ptr()), 0);
        assert_eq!(libc::lstat(source.as_ptr(), status.as_mut_ptr()), 0);

        let descriptor = libc::open(source.as_ptr(), libc::O_RDONLY);
        assert!(descriptor >= 0);

        let mut link_target = [0_u8; 64];
        let link_length = libc::readlink(
            source_link.as_ptr(),
            link_target.as_mut_ptr().cast(),
            link_target.len(),
        );
        assert_eq!(link_length, 10);
        assert_eq!(&link_target[..link_length as usize], b"source.txt");
        assert_eq!(
            libc::renamex_np(source_link.as_ptr(), renamed_link.as_ptr(), 0),
            0
        );
        assert_eq!(
            libc::readlink(
                source_link.as_ptr(),
                link_target.as_mut_ptr().cast(),
                link_target.len(),
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOENT);

        let timeval = [libc::timeval {
            tv_sec: 1,
            tv_usec: 0,
        }; 2];
        let timespec = [libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        }; 2];
        assert_eq!(libc::utimes(source.as_ptr(), timeval.as_ptr()), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(libc::lutimes(source.as_ptr(), timeval.as_ptr()), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(libc::futimes(descriptor, timeval.as_ptr()), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(libc::futimens(descriptor, timespec.as_ptr()), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            libc::utimensat(libc::AT_FDCWD, source.as_ptr(), timespec.as_ptr(), 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(libc::chflags(source.as_ptr(), 0), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(libc::fchflags(descriptor, 0), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            libc::setxattr(
                source.as_ptr(),
                c"com.agora.test".as_ptr(),
                b"value".as_ptr().cast(),
                5,
                0,
                0,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            libc::fsetxattr(
                descriptor,
                c"com.agora.test".as_ptr(),
                b"value".as_ptr().cast(),
                5,
                0,
                0,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            libc::removexattr(source.as_ptr(), c"com.agora.test".as_ptr(), 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            libc::fremovexattr(descriptor, c"com.agora.test".as_ptr(), 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(libc::close(descriptor), 0);

        let root_path = std::ffi::CString::new(root.as_os_str().as_encoded_bytes()).unwrap();
        let directory = libc::open(root_path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
        assert!(directory >= 0);
        assert_eq!(libc::fchdir(directory), 0);
        assert_eq!(libc::fchdir(-1), -1);
        assert_eq!(
            libc::fstatat(directory, c"source.txt".as_ptr(), status.as_mut_ptr(), 0),
            0
        );
        let link_length = libc::readlinkat(
            directory,
            c"renamed-link".as_ptr(),
            link_target.as_mut_ptr().cast(),
            link_target.len(),
        );
        assert_eq!(link_length, 10);
        assert_eq!(&link_target[..link_length as usize], b"source.txt");
        assert_eq!(
            libc::renameatx_np(
                directory,
                c"renamed-link".as_ptr(),
                directory,
                c"source-link".as_ptr(),
                0,
            ),
            0
        );
        assert_eq!(
            libc::renamex_np(source_link.as_ptr(), renamed_link.as_ptr(), 1),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            libc::renamex_np(c"missing".as_ptr(), c"also-missing".as_ptr(), 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOENT);
        assert_eq!(
            libc::renameatx_np(
                directory,
                c"missing".as_ptr(),
                directory,
                c"also-missing".as_ptr(),
                0,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOENT);
        let created_file = libc::openat(
            directory,
            c"created.txt".as_ptr(),
            libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
            0o600,
        );
        assert!(created_file >= 0);
        assert_eq!(libc::write(created_file, b"created".as_ptr().cast(), 7), 7);
        assert_eq!(libc::ftruncate(created_file, 4), 0);
        assert_eq!(libc::fchmod(created_file, 0o640), 0);
        assert_eq!(libc::fsync(created_file), 0);
        assert_eq!(
            call_interposed_descriptor(c"agora_sandbox_commit_synced_descriptor", created_file,),
            0
        );
        assert_eq!(libc::fcntl(created_file, libc::F_SETFD, 0), 0);
        assert_eq!(
            libc::fcntl(created_file, libc::F_GETFD) & libc::FD_CLOEXEC,
            0
        );
        let duplicate = libc::dup(created_file);
        assert!(duplicate >= 0);
        assert_eq!(libc::fcntl(duplicate, libc::F_GETFD) & libc::FD_CLOEXEC, 0);
        let fcntl_duplicate = libc::fcntl(created_file, libc::F_DUPFD_CLOEXEC, 0);
        assert!(fcntl_duplicate >= 0);
        assert_ne!(
            libc::fcntl(fcntl_duplicate, libc::F_GETFD) & libc::FD_CLOEXEC,
            0
        );
        let replacement = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
        assert!(replacement >= 0);
        assert_eq!(libc::dup2(created_file, replacement), replacement);
        assert_eq!(
            libc::fcntl(replacement, libc::F_GETFD) & libc::FD_CLOEXEC,
            0
        );
        assert_eq!(libc::dup2(created_file, created_file), created_file);
        for duplicate in [duplicate, fcntl_duplicate, replacement] {
            assert_eq!(libc::close(duplicate), 0);
        }
        assert_eq!(libc::fchown(created_file, !0, !0), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(libc::close(created_file), 0);
        assert_eq!(libc::fsync(-1), -1);
        assert_eq!(libc::dup(-1), -1);
        assert_eq!(libc::fchmod(-1, 0o600), -1);
        assert_eq!(*libc::__error(), libc::EPERM);
        let creat_file = libc::creat(creat.as_ptr(), 0o600);
        assert!(creat_file >= 0);
        assert_eq!(libc::write(creat_file, b"creat".as_ptr().cast(), 5), 5);
        assert_eq!(libc::close(creat_file), 0);
        assert_eq!(libc::truncate(creat.as_ptr(), -1), -1);
        assert_eq!(*libc::__error(), libc::EINVAL);
        assert_eq!(libc::truncate(creat.as_ptr(), 2), 0);
        let creat_reader = libc::open(creat.as_ptr(), libc::O_RDONLY);
        assert!(creat_reader >= 0);
        let mut truncated = [0_u8; 2];
        assert_eq!(
            libc::read(creat_reader, truncated.as_mut_ptr().cast(), 2),
            2
        );
        assert_eq!(&truncated, b"cr");
        assert_eq!(libc::close(creat_reader), 0);

        let vectored_file = libc::open(
            vectored.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(vectored_file >= 0);
        let initial = [b"ab".as_slice(), b"cd".as_slice()].map(|part| libc::iovec {
            iov_base: part.as_ptr().cast_mut().cast(),
            iov_len: part.len(),
        });
        assert_eq!(
            libc::writev(
                vectored_file,
                initial.as_ptr(),
                initial.len() as libc::c_int
            ),
            4
        );
        assert_eq!(libc::pwrite(vectored_file, b"XY".as_ptr().cast(), 2, 1), 2);
        let positioned = [b"EF".as_slice(), b"GH".as_slice()].map(|part| libc::iovec {
            iov_base: part.as_ptr().cast_mut().cast(),
            iov_len: part.len(),
        });
        assert_eq!(
            libc::pwritev(
                vectored_file,
                positioned.as_ptr(),
                positioned.len() as libc::c_int,
                4,
            ),
            4
        );
        assert_eq!(libc::fsync(vectored_file), 0);
        let reader = libc::open(vectored.as_ptr(), libc::O_RDONLY);
        assert!(reader >= 0);
        let mut content = [0_u8; 8];
        assert_eq!(
            libc::read(reader, content.as_mut_ptr().cast(), content.len()),
            8
        );
        assert_eq!(&content, b"aXYdEFGH");
        assert_eq!(libc::close(reader), 0);
        assert_eq!(libc::close(vectored_file), 0);

        let mapped_file = libc::open(
            mapped.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        );
        assert!(mapped_file >= 0);
        let page_size = libc::sysconf(libc::_SC_PAGESIZE) as usize;
        assert!(page_size > 0);
        let mapping_length = 3 * page_size;
        assert_eq!(
            libc::ftruncate(mapped_file, mapping_length as libc::off_t),
            0
        );
        let mapping = libc::mmap(
            std::ptr::null_mut(),
            mapping_length,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            mapped_file,
            0,
        );
        assert_ne!(mapping, libc::MAP_FAILED);
        std::ptr::copy_nonoverlapping(b"mapped".as_ptr(), mapping.cast::<u8>(), 6);
        assert_eq!(libc::msync(mapping, mapping_length, libc::MS_SYNC), 0);
        assert_eq!(libc::close(mapped_file), 0);
        let middle = mapping.cast::<u8>().add(page_size).cast();
        assert_eq!(
            libc::mprotect(middle, page_size, libc::PROT_READ),
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        assert_eq!(
            libc::mprotect(middle, page_size, libc::PROT_READ | libc::PROT_WRITE,),
            0
        );
        std::ptr::copy_nonoverlapping(b"-data".as_ptr(), mapping.cast::<u8>().add(6), 5);
        assert_eq!(libc::msync(mapping, mapping_length, libc::MS_ASYNC), 0);
        let reader = libc::open(mapped.as_ptr(), libc::O_RDONLY);
        assert!(reader >= 0);
        let mut content = [0_u8; 11];
        assert_eq!(
            libc::read(reader, content.as_mut_ptr().cast(), content.len()),
            11
        );
        assert_eq!(&content, b"mapped-data");
        assert_eq!(libc::close(reader), 0);

        assert_eq!(libc::chmod(source.as_ptr(), 0o600), 0);
        assert_eq!(libc::stat(source.as_ptr(), status.as_mut_ptr()), 0);
        assert_eq!(u32::from((*status.as_ptr()).st_mode) & 0o777, 0o600);
        assert_eq!(libc::chown(source.as_ptr(), !0, !0), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(libc::lchown(source.as_ptr(), !0, !0), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            libc::fchmodat(directory, c"source.txt".as_ptr(), 0o640, 0),
            0
        );
        assert_eq!(
            libc::fchmodat(directory, c"source.txt".as_ptr(), 0o640, 1 << 20,),
            -1
        );
        assert_eq!(*libc::__error(), libc::EINVAL);
        assert_eq!(libc::stat(source.as_ptr(), status.as_mut_ptr()), 0);
        assert_eq!(u32::from((*status.as_ptr()).st_mode) & 0o777, 0o640);
        assert_eq!(libc::chmod(source.as_ptr(), 0), 0);
        assert_eq!(
            libc::faccessat(
                directory,
                c"source.txt".as_ptr(),
                libc::R_OK,
                libc::AT_EACCESS,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::EACCES);
        assert_eq!(libc::chmod(source.as_ptr(), 0o640), 0);
        assert_eq!(
            libc::faccessat(libc::AT_FDCWD, c"/dev/null".as_ptr(), libc::R_OK, 0,),
            0
        );
        assert_eq!(
            libc::faccessat(
                directory,
                c"source.txt".as_ptr(),
                libc::R_OK,
                libc::AT_EACCESS,
            ),
            0
        );
        assert_eq!(
            libc::faccessat(directory, c"source.txt".as_ptr(), libc::R_OK, 1 << 20,),
            -1
        );
        assert_eq!(*libc::__error(), libc::EINVAL);
        assert_eq!(
            libc::fchownat(directory, c"source.txt".as_ptr(), !0, !0, 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);

        assert_eq!(libc::mkdirat(directory, c"created-at".as_ptr(), 0o700), 0);
        assert_eq!(
            libc::renameat(
                directory,
                c"created.txt".as_ptr(),
                directory,
                c"renamed-at.txt".as_ptr(),
            ),
            0
        );
        assert_eq!(libc::unlinkat(directory, c"renamed-at.txt".as_ptr(), 0), 0);
        assert_eq!(
            libc::unlinkat(directory, c"created-at".as_ptr(), libc::AT_REMOVEDIR),
            0
        );

        assert_eq!(libc::link(source.as_ptr(), c"hard-link".as_ptr()), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            libc::linkat(
                directory,
                c"source.txt".as_ptr(),
                directory,
                c"hard-link-at".as_ptr(),
                0,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            libc::symlink(c"source.txt".as_ptr(), c"symlink".as_ptr()),
            0
        );
        let link_length = libc::readlink(
            c"symlink".as_ptr(),
            link_target.as_mut_ptr().cast(),
            link_target.len(),
        );
        assert_eq!(link_length, 10);
        assert_eq!(&link_target[..link_length as usize], b"source.txt");
        assert_eq!(libc::unlink(c"symlink".as_ptr()), 0);
        assert_eq!(
            libc::symlinkat(c"source.txt".as_ptr(), directory, c"symlink-at".as_ptr()),
            0
        );
        let link_length = libc::readlinkat(
            directory,
            c"symlink-at".as_ptr(),
            link_target.as_mut_ptr().cast(),
            link_target.len(),
        );
        assert_eq!(link_length, 10);
        assert_eq!(&link_target[..link_length as usize], b"source.txt");
        assert_eq!(libc::unlinkat(directory, c"symlink-at".as_ptr(), 0), 0);
        assert_eq!(libc::clonefile(source.as_ptr(), c"clone".as_ptr(), 0), -1);
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            libc::clonefileat(
                directory,
                c"source.txt".as_ptr(),
                directory,
                c"clone-at".as_ptr(),
                0,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(
            libc::copyfile(
                source.as_ptr(),
                c"copy".as_ptr(),
                std::ptr::null_mut(),
                libc::COPYFILE_DATA,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOTSUP);

        let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        assert_eq!(libc::posix_spawn_file_actions_init(&mut actions), 0);
        assert_eq!(
            libc::posix_spawn_file_actions_addopen(
                &mut actions,
                8,
                source.as_ptr(),
                libc::O_RDONLY,
                0,
            ),
            0
        );
        assert_eq!(
            libc::posix_spawn_file_actions_addopen(
                &mut actions,
                9,
                spawn.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                0o600,
            ),
            libc::ENOTSUP
        );
        let mut child = 0;
        let arguments = [c"/usr/bin/true".as_ptr().cast_mut(), std::ptr::null_mut()];
        assert_eq!(
            libc::posix_spawn(
                &mut child,
                c"/usr/bin/true".as_ptr(),
                &actions,
                std::ptr::null(),
                arguments.as_ptr(),
                std::ptr::null(),
            ),
            0
        );
        let mut child_status = 0;
        assert_eq!(libc::waitpid(child, &mut child_status, 0), child);
        assert_eq!(child_status, 0);
        assert_eq!(libc::posix_spawn_file_actions_destroy(&mut actions), 0);

        assert_eq!(libc::munmap(middle, page_size), 0);
        assert_eq!(libc::munmap(mapping, page_size), 0);
        assert_eq!(
            libc::munmap(mapping.cast::<u8>().add(2 * page_size).cast(), page_size,),
            0
        );
        let mapped_file = libc::open(mapped.as_ptr(), libc::O_RDWR);
        assert!(mapped_file >= 0);
        let reservation = libc::mmap(
            std::ptr::null_mut(),
            page_size,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        );
        assert_ne!(reservation, libc::MAP_FAILED);
        let fixed = libc::mmap(
            reservation,
            page_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_FIXED,
            mapped_file,
            0,
        );
        assert_eq!(fixed, reservation);
        assert_eq!(
            &std::slice::from_raw_parts(fixed.cast::<u8>(), 11),
            b"mapped-data"
        );
        assert_eq!(libc::munmap(fixed, page_size), 0);
        assert_eq!(libc::close(mapped_file), 0);

        assert_eq!(
            libc::openat(-1, c"missing.txt".as_ptr(), libc::O_RDONLY),
            -1
        );
        assert_eq!(
            libc::fstatat(-1, c"missing.txt".as_ptr(), status.as_mut_ptr(), 0),
            -1
        );
        assert_eq!(libc::close(directory), 0);

        assert!(libc::fopen(source.as_ptr(), std::ptr::null()).is_null());
        let stream = libc::fopen(source.as_ptr(), c"r".as_ptr());
        assert!(!stream.is_null());
        assert!(libc::freopen(spawn.as_ptr(), c"w".as_ptr(), stream).is_null());
        assert_eq!(*libc::__error(), libc::ENOTSUP);
        assert_eq!(libc::fclose(stream), 0);

        let stream = libc::fopen(source.as_ptr(), c"r".as_ptr());
        assert!(!stream.is_null());
        let stream = libc::freopen(c"/dev/null".as_ptr(), c"r".as_ptr(), stream);
        assert!(!stream.is_null());
        assert_eq!(libc::fclose(stream), 0);

        let appended =
            std::ffi::CString::new(root.join("appended.txt").as_os_str().as_encoded_bytes())
                .unwrap();
        let stream = libc::fopen(appended.as_ptr(), c"a+".as_ptr());
        assert!(!stream.is_null());
        assert_eq!(libc::fclose(stream), 0);
        assert_eq!(libc::unlink(appended.as_ptr()), 0);

        let original_directory = libc::open(c".".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
        let device_directory = libc::open(c"/dev".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
        assert!(original_directory >= 0);
        assert!(device_directory >= 0);
        *libc::__error() = libc::E2BIG;
        assert_eq!(libc::fchdir(device_directory), 0);
        assert_eq!(*libc::__error(), libc::E2BIG);
        assert_eq!(libc::fchdir(original_directory), 0);
        assert_eq!(libc::close(device_directory), 0);
        assert_eq!(libc::close(original_directory), 0);

        assert_eq!(libc::mkdir(created.as_ptr(), 0o700), 0);
        let created_directory = libc::open(created.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
        assert!(created_directory >= 0);
        assert_eq!(libc::chmod(created.as_ptr(), 0o400), 0);
        assert_eq!(libc::fchdir(created_directory), -1);
        assert_eq!(*libc::__error(), libc::EACCES);
        assert_eq!(libc::chmod(created.as_ptr(), 0o500), 0);
        let denied = std::ffi::CString::new(
            root.join("created/denied.txt")
                .as_os_str()
                .as_encoded_bytes(),
        )
        .unwrap();
        assert_eq!(
            libc::open(
                denied.as_ptr(),
                libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
                0o600,
            ),
            -1
        );
        assert_eq!(*libc::__error(), libc::EACCES);
        assert_eq!(libc::chmod(created.as_ptr(), 0o700), 0);
        assert_eq!(libc::close(created_directory), 0);
        assert_eq!(libc::rename(creat.as_ptr(), renamed.as_ptr()), 0);
        assert_eq!(libc::unlink(renamed.as_ptr()), 0);

        assert!(fts_open(std::ptr::null(), 0x010, None).is_null());
        assert_eq!(*libc::__error(), libc::EFAULT);
        let mut fts_paths = [root_path.as_ptr().cast_mut(), std::ptr::null_mut()];
        let fts = fts_open(fts_paths.as_mut_ptr(), 0x004 | 0x010, None);
        assert!(!fts.is_null());
        assert!(!fts_read(fts).is_null());
        assert!(!fts_children(fts, 0).is_null());
        let mut fts_entries = 1;
        while !fts_read(fts).is_null() {
            fts_entries += 1;
        }
        assert!(fts_entries >= 3);
        assert_eq!(fts_close(fts), 0);

        let directory = libc::opendir(root_path.as_ptr());
        assert!(!directory.is_null());
        let mut names = Vec::new();
        loop {
            let entry = libc::readdir(directory);
            if entry.is_null() {
                break;
            }
            names.push(
                std::ffi::CStr::from_ptr((*entry).d_name.as_ptr())
                    .to_bytes()
                    .to_vec(),
            );
        }
        assert!(names.iter().any(|name| name == b"source.txt"));
        assert!(names.iter().any(|name| name == b"created"));
        libc::rewinddir(directory);
        let mut rewound_names = Vec::new();
        loop {
            let entry = libc::readdir(directory);
            if entry.is_null() {
                break;
            }
            rewound_names.push(
                std::ffi::CStr::from_ptr((*entry).d_name.as_ptr())
                    .to_bytes()
                    .to_vec(),
            );
        }
        assert_eq!(rewound_names, names);
        assert_eq!(libc::closedir(directory), 0);

        let descriptor = libc::open(root_path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
        assert!(descriptor >= 0);
        let directory = libc::fdopendir(descriptor);
        assert!(!directory.is_null());
        let mut entry = std::mem::zeroed::<libc::dirent>();
        let mut result = std::ptr::null_mut();
        assert_eq!(
            libc::readdir_r(std::ptr::null_mut(), &mut entry, &mut result),
            libc::EINVAL
        );
        loop {
            assert_eq!(libc::readdir_r(directory, &mut entry, &mut result), 0);
            if result.is_null() {
                break;
            }
        }
        libc::rewinddir(directory);
        assert_eq!(libc::readdir_r(directory, &mut entry, &mut result), 0);
        assert!(!result.is_null());
        assert_eq!(libc::closedir(directory), 0);
        assert!(libc::fdopendir(-1).is_null());

        let external_directory = libc::open(c"/usr/bin".as_ptr(), libc::O_RDONLY);
        assert!(external_directory >= 0);
        let external_directory = libc::fdopendir(external_directory);
        assert!(!external_directory.is_null());
        assert!(!libc::readdir(external_directory).is_null());
        assert_eq!(libc::closedir(external_directory), 0);

        assert_eq!(libc::unlink(source.as_ptr()), 0);
        assert_eq!(
            libc::faccessat(libc::AT_FDCWD, source.as_ptr(), libc::F_OK, 0),
            -1
        );
        assert_eq!(*libc::__error(), libc::ENOENT);

        assert!(libc::opendir(std::ptr::null()).is_null());
        assert_eq!(libc::mkdir(std::ptr::null(), 0o700), -1);
        assert_eq!(libc::rename(std::ptr::null(), renamed.as_ptr()), -1);
        assert_eq!(libc::unlink(std::ptr::null()), -1);
        assert_eq!(libc::rmdir(std::ptr::null()), -1);
        assert_eq!(libc::chdir(std::ptr::null()), -1);

        assert_eq!(libc::chdir(created.as_ptr()), 0);
        assert!(libc::getcwd(std::ptr::null_mut(), 1).is_null());
        let mut small = [0_i8; 1];
        assert!(libc::getcwd(small.as_mut_ptr(), small.len()).is_null());
        let current = libc::getcwd(std::ptr::null_mut(), 0);
        assert!(!current.is_null());
        assert_eq!(
            std::ffi::CStr::from_ptr(current).to_bytes(),
            created.as_bytes()
        );
        libc::free(current.cast());
        assert_eq!(libc::chdir(root_path.as_ptr()), 0);
        assert_eq!(libc::rmdir(created.as_ptr()), 0);
    }

    exercise_interposed_process_symbols();
}

#[cfg(target_os = "macos")]
#[test]
fn read_only_directory_child_process() {
    if std::env::var_os("AGORA_SANDBOX_TEST_READ_ONLY_DIRECTORY").is_none() {
        return;
    }

    unsafe {
        let directory = libc::opendir(c"/usr/bin".as_ptr());
        assert!(!directory.is_null());
        while !libc::readdir(directory).is_null() {}
        assert_eq!(*libc::__error(), 0);
        assert_eq!(libc::closedir(directory), 0);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn upper_only_canonicalize_child_process() {
    if std::env::var_os("AGORA_SANDBOX_TEST_CANONICALIZE_UPPER").is_none() {
        return;
    }
    let logical = std::env::current_dir().unwrap().join("upper-directory");
    std::fs::create_dir(&logical).unwrap();
    let canonical = std::fs::canonicalize(&logical).unwrap();
    assert_eq!(canonical, logical);
    assert!(!canonical.to_string_lossy().contains("/.agora-sandbox/fs/"));
}

#[cfg(target_os = "macos")]
fn exercise_interposed_process_symbols() {
    type PosixSpawnFn = unsafe extern "C" fn(
        *mut libc::pid_t,
        *const libc::c_char,
        *const libc::posix_spawn_file_actions_t,
        *const libc::posix_spawnattr_t,
        *const *mut libc::c_char,
        *const *mut libc::c_char,
    ) -> libc::c_int;
    type ExecveFn = unsafe extern "C" fn(
        *const libc::c_char,
        *const *const libc::c_char,
        *const *const libc::c_char,
    ) -> libc::c_int;
    type ExecvFn =
        unsafe extern "C" fn(*const libc::c_char, *const *const libc::c_char) -> libc::c_int;

    unsafe fn symbol(name: &std::ffi::CStr) -> *mut libc::c_void {
        let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
        assert!(!symbol.is_null(), "missing interposed symbol {name:?}");
        symbol
    }

    let true_path = c"/usr/bin/true";
    let true_name = c"true";
    let mut direct_arguments = [true_path.as_ptr().cast_mut(), std::ptr::null_mut()];
    let mut searched_arguments = [true_name.as_ptr().cast_mut(), std::ptr::null_mut()];

    for (name, executable, arguments) in [
        (
            c"agora_sandbox_posix_spawn",
            true_path,
            direct_arguments.as_mut_ptr(),
        ),
        (
            c"agora_sandbox_posix_spawnp",
            true_name,
            searched_arguments.as_mut_ptr(),
        ),
    ] {
        let spawn = unsafe { std::mem::transmute::<*mut libc::c_void, PosixSpawnFn>(symbol(name)) };
        let mut pid = 0;
        assert_eq!(
            unsafe {
                spawn(
                    &mut pid,
                    executable.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    arguments,
                    std::ptr::null(),
                )
            },
            0
        );
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
    }

    for operation in ["execve", "execv", "execvp"] {
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            let result = match operation {
                "execve" => {
                    let exec = unsafe {
                        std::mem::transmute::<*mut libc::c_void, ExecveFn>(symbol(
                            c"agora_sandbox_execve",
                        ))
                    };
                    let arguments = [true_path.as_ptr(), std::ptr::null()];
                    let environment = unsafe { *libc::_NSGetEnviron() };
                    unsafe { exec(true_path.as_ptr(), arguments.as_ptr(), environment.cast()) }
                }
                "execv" => {
                    let exec = unsafe {
                        std::mem::transmute::<*mut libc::c_void, ExecvFn>(symbol(
                            c"agora_sandbox_execv",
                        ))
                    };
                    let arguments = [true_path.as_ptr(), std::ptr::null()];
                    unsafe { exec(true_path.as_ptr(), arguments.as_ptr()) }
                }
                "execvp" => {
                    let exec = unsafe {
                        std::mem::transmute::<*mut libc::c_void, ExecvFn>(symbol(
                            c"agora_sandbox_execvp",
                        ))
                    };
                    let arguments = [true_name.as_ptr(), std::ptr::null()];
                    unsafe { exec(true_name.as_ptr(), arguments.as_ptr()) }
                }
                _ => unreachable!(),
            };
            unsafe { libc::_exit(if result == -1 { 126 } else { 127 }) };
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
    }

    let execve = unsafe {
        std::mem::transmute::<*mut libc::c_void, ExecveFn>(symbol(c"agora_sandbox_execve"))
    };
    assert_eq!(
        unsafe { execve(std::ptr::null(), std::ptr::null(), std::ptr::null()) },
        -1
    );
    assert_eq!(unsafe { *libc::__error() }, libc::EFAULT);

    let execvp = unsafe {
        std::mem::transmute::<*mut libc::c_void, ExecvFn>(symbol(c"agora_sandbox_execvp"))
    };
    let missing = c"agora-command-that-does-not-exist";
    let arguments = [missing.as_ptr(), std::ptr::null()];
    assert_eq!(
        unsafe { libc::setenv(c"PATH".as_ptr(), c":relative".as_ptr(), 1) },
        0
    );
    assert_eq!(unsafe { execvp(missing.as_ptr(), arguments.as_ptr()) }, -1);
    assert_eq!(unsafe { *libc::__error() }, libc::ENOENT);
}

#[test]
fn intercepted_child_process() {
    if std::env::var_os("AGORA_SANDBOX_TEST_CHILD").is_none() {
        return;
    }

    let destination = std::env::var("AGORA_SANDBOX_TEST_DESTINATION").unwrap();
    let destination = destination.parse().unwrap();
    let mut stream = TcpStream::connect(destination).unwrap();
    let peer = stream.peer_addr().unwrap();
    assert!(peer.ip().is_loopback());
    assert_ne!(peer, destination);
    stream.write_all(b"hooked").unwrap();
    let mut echoed = [0_u8; 6];
    stream.read_exact(&mut echoed).unwrap();
    assert_eq!(&echoed, b"hooked");

    let destination = destination.to_string().parse::<SocketAddrV4>().unwrap();
    let datagram = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    datagram.connect(destination).unwrap();

    let address = libc::sockaddr_in {
        sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
        sin_family: libc::AF_INET as u8,
        sin_port: destination.port().to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(destination.ip().octets()),
        },
        sin_zero: [0; 8],
    };
    assert_eq!(
        unsafe {
            libc::connect(
                -1,
                std::ptr::addr_of!(address).cast(),
                std::mem::size_of_val(&address) as libc::socklen_t,
            )
        },
        -1
    );

    let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    assert!(socket >= 0);
    let endpoints = TestSocketEndpoints {
        source_interface: 0,
        source_address: std::ptr::null(),
        source_address_length: 0,
        destination_address: std::ptr::addr_of!(address).cast(),
        destination_address_length: std::mem::size_of_val(&address) as libc::socklen_t,
    };
    assert_eq!(
        unsafe {
            connectx(
                socket,
                std::ptr::addr_of!(endpoints),
                0,
                0,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    unsafe { libc::close(socket) };
    assert_eq!(
        unsafe {
            connectx(
                -1,
                std::ptr::addr_of!(endpoints),
                0,
                0,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        -1
    );
}

#[cfg(target_os = "macos")]
#[test]
fn forked_intercepted_child_process() {
    if std::env::var_os("AGORA_SANDBOX_TEST_FORK_CHILD").is_none() {
        return;
    }

    let destination = std::env::var("AGORA_SANDBOX_TEST_DESTINATION")
        .unwrap()
        .parse()
        .unwrap();
    let child = unsafe { libc::fork() };
    assert!(child >= 0);
    if child == 0 {
        let succeeded = exchange_payload(destination, b"child!").is_ok();
        unsafe { libc::_exit(if succeeded { 0 } else { 1 }) };
    }

    exchange_payload(destination, b"parent").unwrap();
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert!(libc::WIFEXITED(status));
    assert_eq!(libc::WEXITSTATUS(status), 0);
}

#[cfg(target_os = "macos")]
#[test]
fn unlinked_current_directory_child_process() {
    if std::env::var_os("AGORA_SANDBOX_TEST_UNLINKED_CWD_CHILD").is_none() {
        return;
    }

    let original = std::fs::File::open(".").unwrap();
    let directory = PathBuf::from("managed-current-directory");
    std::fs::create_dir(&directory).unwrap();
    let descriptor = std::fs::File::open(&directory).unwrap();
    assert_eq!(unsafe { libc::fchdir(descriptor.as_raw_fd()) }, 0);
    std::fs::remove_dir(std::env::current_dir().unwrap()).unwrap();

    let target = std::env::var_os("AGORA_SANDBOX_TEST_HOST_TARGET").unwrap();
    let child = Command::new("/bin/sh")
        .args(["-c", "printf escaped > \"$AGORA_SANDBOX_TEST_HOST_TARGET\""])
        .env("AGORA_SANDBOX_TEST_HOST_TARGET", target)
        .status();

    assert_eq!(unsafe { libc::fchdir(original.as_raw_fd()) }, 0);
    assert!(child.is_err() || child.is_ok_and(|status| !status.success()));
}

#[cfg(target_os = "macos")]
#[test]
fn unlinked_open_file_exec_child_process() {
    let Some(path) = std::env::var_os("AGORA_SANDBOX_TEST_UNLINKED_OPEN_FILE") else {
        return;
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    std::fs::remove_file(&path).unwrap();

    let child = unsafe { libc::fork() };
    assert!(child >= 0);
    if child == 0 {
        let arguments = [c"/usr/bin/true".as_ptr(), std::ptr::null()];
        unsafe {
            libc::execv(arguments[0], arguments.as_ptr());
            libc::_exit(*libc::__error());
        }
    }

    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert!(libc::WIFEXITED(status));
    assert_eq!(libc::WEXITSTATUS(status), 0);
    drop(file);
}

#[cfg(target_os = "macos")]
fn exchange_payload(destination: std::net::SocketAddr, payload: &[u8; 6]) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(destination)?;
    stream.write_all(payload)?;
    let mut echoed = [0_u8; 6];
    stream.read_exact(&mut echoed)?;
    if &echoed != payload {
        return Err(std::io::Error::other("unexpected echoed payload"));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn missing_hook_configuration_child_process() {
    if std::env::var_os("AGORA_SANDBOX_TEST_MISSING_CONFIG_CHILD").is_none() {
        return;
    }

    let directory = std::env::temp_dir().join(format!(
        "agora-missing-hook-filesystem-{}",
        uuid::Uuid::new_v4()
    ));
    let original_directory = std::env::current_dir().unwrap();
    std::fs::create_dir_all(&directory).unwrap();
    let source_path = directory.join("source");
    let target_path = directory.join("target");
    std::fs::write(&source_path, b"source").unwrap();
    std::fs::write(&target_path, b"target").unwrap();
    std::os::unix::fs::symlink("source", directory.join("link")).unwrap();
    let path = |path: &Path| std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let root = path(&directory);
    let original = path(&original_directory);
    let source = path(&source_path);
    let target = path(&target_path);
    let link = path(&directory.join("link"));

    unsafe {
        let descriptor = libc::open(source.as_ptr(), libc::O_RDWR);
        assert!(descriptor >= 0);
        assert_eq!(libc::fcntl(descriptor, libc::F_SETFD, 0), 0);
        assert_eq!(libc::ftruncate(descriptor, 4), 0);
        assert_eq!(libc::fchmod(descriptor, 0o600), 0);
        assert_eq!(libc::fsync(descriptor), 0);
        assert_eq!(
            call_interposed_descriptor(c"agora_sandbox_commit_synced_descriptor", descriptor),
            0
        );
        let duplicate = libc::dup(descriptor);
        assert!(duplicate >= 0);
        assert_eq!(libc::dup2(descriptor, duplicate), duplicate);
        assert_eq!(libc::close(duplicate), 0);

        let mut status = std::mem::zeroed::<libc::stat>();
        assert_eq!(libc::fstat(descriptor, &mut status), 0);
        assert_eq!(libc::stat(source.as_ptr(), &mut status), 0);
        assert_eq!(libc::lstat(source.as_ptr(), &mut status), 0);
        assert_eq!(libc::access(source.as_ptr(), libc::R_OK), 0);

        let directory_descriptor = libc::open(root.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY);
        assert!(directory_descriptor >= 0);
        assert_eq!(
            libc::fstatat(
                directory_descriptor,
                c"source".as_ptr(),
                &mut status,
                libc::AT_SYMLINK_NOFOLLOW,
            ),
            0
        );
        assert_eq!(
            libc::faccessat(directory_descriptor, c"source".as_ptr(), libc::R_OK, 0),
            0
        );
        assert_eq!(
            libc::fchmodat(directory_descriptor, c"source".as_ptr(), 0o640, 0),
            0
        );

        let mut link_target = [0_u8; 16];
        assert_eq!(
            libc::readlink(
                link.as_ptr(),
                link_target.as_mut_ptr().cast(),
                link_target.len(),
            ),
            6
        );
        assert_eq!(
            libc::readlinkat(
                directory_descriptor,
                c"link".as_ptr(),
                link_target.as_mut_ptr().cast(),
                link_target.len(),
            ),
            6
        );

        let stream = libc::fopen(source.as_ptr(), c"r".as_ptr());
        assert!(!stream.is_null());
        let stream = libc::freopen(target.as_ptr(), c"r".as_ptr(), stream);
        assert!(!stream.is_null());
        assert_eq!(libc::fclose(stream), 0);

        let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        assert_eq!(libc::posix_spawn_file_actions_init(&mut actions), 0);
        assert_eq!(
            libc::posix_spawn_file_actions_addopen(
                &mut actions,
                8,
                source.as_ptr(),
                libc::O_RDONLY,
                0,
            ),
            0
        );
        assert_eq!(libc::posix_spawn_file_actions_destroy(&mut actions), 0);

        assert_eq!(libc::chdir(root.as_ptr()), 0);
        assert_eq!(libc::fchdir(directory_descriptor), 0);
        let mut cwd = [0_i8; libc::PATH_MAX as usize];
        assert_eq!(libc::getcwd(cwd.as_mut_ptr(), cwd.len()), cwd.as_mut_ptr());

        let directory_stream = libc::fdopendir(libc::dup(directory_descriptor));
        assert!(!directory_stream.is_null());
        let mut entry = std::mem::zeroed::<libc::dirent>();
        let mut result = std::ptr::null_mut();
        assert_eq!(
            libc::readdir_r(directory_stream, &mut entry, &mut result),
            0
        );
        libc::rewinddir(directory_stream);
        assert_eq!(libc::closedir(directory_stream), 0);

        assert_eq!(libc::chdir(original.as_ptr()), 0);
        assert_eq!(libc::close(directory_descriptor), 0);
        assert_eq!(libc::close(descriptor), 0);
    }

    std::fs::remove_dir_all(&directory).unwrap();

    let destination = std::env::var("AGORA_SANDBOX_TEST_DESTINATION").unwrap();
    let error = TcpStream::connect(destination).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EACCES));
}

#[cfg(target_os = "macos")]
#[test]
fn nonblocking_intercepted_child_process() {
    if std::env::var_os("AGORA_SANDBOX_TEST_NONBLOCKING_CHILD").is_none() {
        return;
    }

    run_nonblocking_intercepted_child(false);
}

#[cfg(target_os = "macos")]
#[test]
fn nonblocking_connectx_intercepted_child_process() {
    if std::env::var_os("AGORA_SANDBOX_TEST_NONBLOCKING_CONNECTX_CHILD").is_none() {
        return;
    }

    run_nonblocking_intercepted_child(true);
}

#[cfg(target_os = "macos")]
#[test]
fn unsupported_connectx_intercepted_child_process() {
    if std::env::var_os("AGORA_SANDBOX_TEST_UNSUPPORTED_CONNECTX_CHILD").is_none() {
        return;
    }

    let destination = std::env::var("AGORA_SANDBOX_TEST_DESTINATION")
        .unwrap()
        .parse::<SocketAddrV4>()
        .unwrap();
    let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    assert!(socket >= 0);
    let address = libc::sockaddr_in {
        sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
        sin_family: libc::AF_INET as u8,
        sin_port: destination.port().to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(destination.ip().octets()),
        },
        sin_zero: [0; 8],
    };
    let endpoints = TestSocketEndpoints {
        source_interface: 0,
        source_address: std::ptr::null(),
        source_address_length: 0,
        destination_address: std::ptr::addr_of!(address).cast(),
        destination_address_length: std::mem::size_of_val(&address) as libc::socklen_t,
    };
    let payload = b"must-not-bypass";
    let vector = libc::iovec {
        iov_base: payload.as_ptr().cast_mut().cast(),
        iov_len: payload.len(),
    };
    let mut bytes_written = 0;
    let result = unsafe {
        connectx(
            socket,
            std::ptr::addr_of!(endpoints),
            0,
            0,
            std::ptr::addr_of!(vector),
            1,
            std::ptr::addr_of_mut!(bytes_written),
            std::ptr::null_mut(),
        )
    };

    assert_eq!(result, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EACCES)
    );
    unsafe { libc::close(socket) };
}

#[cfg(target_os = "macos")]
fn run_nonblocking_intercepted_child(use_connectx: bool) {
    let destination = std::env::var("AGORA_SANDBOX_TEST_DESTINATION")
        .unwrap()
        .parse::<SocketAddrV4>()
        .unwrap();
    let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    assert!(socket >= 0);
    let flags = unsafe { libc::fcntl(socket, libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(socket, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    let address = libc::sockaddr_in {
        sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
        sin_family: libc::AF_INET as u8,
        sin_port: destination.port().to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(destination.ip().octets()),
        },
        sin_zero: [0; 8],
    };

    let started = Instant::now();
    let mut bytes_written = usize::MAX;
    let mut connection_id = u32::MAX;
    let result = if use_connectx {
        let endpoints = TestSocketEndpoints {
            source_interface: 0,
            source_address: std::ptr::null(),
            source_address_length: 0,
            destination_address: std::ptr::addr_of!(address).cast(),
            destination_address_length: std::mem::size_of_val(&address) as libc::socklen_t,
        };
        unsafe {
            connectx(
                socket,
                std::ptr::addr_of!(endpoints),
                0,
                0,
                std::ptr::null(),
                0,
                std::ptr::addr_of_mut!(bytes_written),
                std::ptr::addr_of_mut!(connection_id),
            )
        }
    } else {
        unsafe {
            libc::connect(
                socket,
                std::ptr::addr_of!(address).cast(),
                std::mem::size_of_val(&address) as libc::socklen_t,
            )
        }
    };
    let elapsed = started.elapsed();
    assert_eq!(result, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EINPROGRESS)
    );
    assert!(
        elapsed < Duration::from_millis(250),
        "nonblocking connect took {elapsed:?}",
    );
    if use_connectx {
        assert_eq!(bytes_written, 0);
        assert_eq!(connection_id, 0);
    }
    assert_ne!(
        unsafe { libc::fcntl(socket, libc::F_GETFL) } & libc::O_NONBLOCK,
        0
    );

    let mut descriptor = libc::pollfd {
        fd: socket,
        events: libc::POLLOUT,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 3_000) }, 1);
    assert_ne!(descriptor.revents & libc::POLLOUT, 0);
    let mut socket_error = 0;
    let mut error_length = std::mem::size_of_val(&socket_error) as libc::socklen_t;
    assert_eq!(
        unsafe {
            libc::getsockopt(
                socket,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                std::ptr::addr_of_mut!(socket_error).cast(),
                &mut error_length,
            )
        },
        0
    );
    assert_eq!(socket_error, 0);

    let mut stream = unsafe { TcpStream::from_raw_fd(socket) };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream.write_all(b"hooked").unwrap();
    let mut echoed = [0_u8; 6];
    stream.read_exact(&mut echoed).unwrap();
    assert_eq!(&echoed, b"hooked");
}

#[tokio::test]
async fn injected_hook_routes_a_real_child_connection_through_the_proxy() {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let destination = listener.local_addr().unwrap();
    let echo = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = [0_u8; 6];
        tokio::io::AsyncReadExt::read_exact(&mut stream, &mut bytes)
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut stream, &bytes)
            .await
            .unwrap();
    });
    let events = Arc::new(Mutex::new(Vec::<NetworkEvent>::new()));
    let callback = {
        let events = Arc::clone(&events);
        move |event: Event| {
            if let Some(event) = event.into_network() {
                events.lock().unwrap().push(event);
            }
            std::future::ready(Decision::Allow)
        }
    };
    let command = SandboxCommand::new(std::env::current_exe().unwrap())
        .arg("intercepted_child_process")
        .arg("--exact")
        .arg("--nocapture")
        .env("AGORA_SANDBOX_TEST_CHILD", "1")
        .env("AGORA_SANDBOX_TEST_DESTINATION", destination.to_string());
    let outcome = Sandbox::new(sandbox_config(), callback)
        .run(command)
        .await
        .unwrap();
    assert!(
        outcome.status().success(),
        "child status: {:?}",
        outcome.status()
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), echo)
        .await
        .unwrap()
        .unwrap();
    let event_types = events
        .lock()
        .unwrap()
        .iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert_eq!(
        event_types,
        vec![
            EventType::NetworkConnectAttempt,
            EventType::NetworkConnectEstablished,
            EventType::NetworkConnectionClosed,
        ]
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn runner_keeps_an_unrestricted_executable_at_its_original_path() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-executable-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let workdir = directory.join("cache");
    let source = std::env::current_exe().unwrap().canonicalize().unwrap();
    let command = SandboxCommand::new(std::env::current_exe().unwrap())
        .arg("records_current_executable")
        .arg("--exact")
        .arg("--nocapture")
        .env("AGORA_SANDBOX_TEST_CURRENT_EXE", &source);
    let config = sandbox_config_in(&workdir);

    let outcome = Sandbox::new(config.clone(), NoopCallback)
        .run(command)
        .await
        .unwrap();

    assert!(outcome.status().success());
    let cached = workdir
        .join("fs")
        .join(source.strip_prefix(Path::new("/")).unwrap());
    assert!(!cached.exists());

    let second = SandboxCommand::new(std::env::current_exe().unwrap())
        .arg("records_current_executable")
        .arg("--exact")
        .arg("--nocapture")
        .env("AGORA_SANDBOX_TEST_CURRENT_EXE", &source);
    let outcome = Sandbox::new(config, NoopCallback)
        .run(second)
        .await
        .unwrap();
    assert!(outcome.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn runner_prepares_a_relocated_executable_sibling_on_demand() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-sibling-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source_directory = directory.join("source");
    let workdir = directory.join("cache");
    std::fs::create_dir_all(&source_directory).unwrap();
    let primary = source_directory.join("primary");
    let sibling = source_directory.join("sibling");
    for executable in [&primary, &sibling] {
        std::fs::copy(std::env::current_exe().unwrap(), executable).unwrap();
        let output = Command::new("/usr/bin/codesign")
            .args([
                "--force",
                "--sign",
                "-",
                "--options",
                "runtime",
                "--timestamp=none",
            ])
            .arg(executable)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "codesign failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let command = SandboxCommand::new(&primary)
        .arg("relocated_executable_spawns_its_sibling_and_preserves_missing_errno")
        .arg("--exact")
        .arg("--nocapture")
        .env("AGORA_SANDBOX_TEST_RELOCATED_SIBLING", "primary");
    let config = sandbox_config_in(&workdir);

    let outcome = Sandbox::new(config, NoopCallback)
        .run(command)
        .await
        .unwrap();

    assert!(outcome.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn prepared_executable_can_inspect_its_current_path() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-current-executable-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source_directory = directory.join("source");
    let workdir = directory.join("cache");
    std::fs::create_dir_all(&source_directory).unwrap();
    let executable = source_directory.join("current-executable");
    std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    let output = Command::new("/usr/bin/codesign")
        .args([
            "--force",
            "--sign",
            "-",
            "--options",
            "runtime",
            "--timestamp=none",
        ])
        .arg(&executable)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "codesign failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let command = SandboxCommand::new(&executable)
        .arg("relocated_executable_can_inspect_its_current_path")
        .arg("--exact")
        .arg("--nocapture")
        .env("AGORA_SANDBOX_TEST_CURRENT_EXE_ACCESS", "1");

    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(command)
        .await
        .unwrap();

    assert!(outcome.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn prepared_executable_can_read_and_load_sibling_resources() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-relocated-package-test-{}",
        uuid::Uuid::new_v4()
    ));
    let package = directory.join("source/Relocated.app");
    let contents = package.join("Contents");
    let executable_directory = contents.join("MacOS");
    let plugin_directory = contents.join("PlugIns");
    let workdir = directory.join("cache");
    std::fs::create_dir_all(&executable_directory).unwrap();
    std::fs::create_dir_all(&plugin_directory).unwrap();

    let executable = executable_directory.join("relocated-fixture");
    std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    std::fs::write(contents.join("Info.plist"), b"agora relocated resource\n").unwrap();
    let source = directory.join("libfixture.c");
    let library = plugin_directory.join("libfixture.dylib");
    std::fs::write(&source, b"int agora_fixture_value(void) { return 42; }\n").unwrap();
    let architecture = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "arm64"
    };
    let output = Command::new("/usr/bin/xcrun")
        .args(["clang", "-dynamiclib", "-arch", architecture, "-o"])
        .arg(&library)
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "clang failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-", "--timestamp=none"])
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "dylib codesign failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let entitlements = directory.join("entitlements.plist");
    std::fs::write(
        &entitlements,
        br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>com.apple.security.cs.disable-library-validation</key><true/></dict></plist>
"#,
    )
    .unwrap();
    let output = Command::new("/usr/bin/codesign")
        .args([
            "--force",
            "--sign",
            "-",
            "--options",
            "runtime",
            "--timestamp=none",
            "--entitlements",
        ])
        .arg(&entitlements)
        .arg(&executable)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "executable codesign failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let command = SandboxCommand::new(&executable)
        .arg("relocated_executable_can_read_and_load_sibling_resources")
        .arg("--exact")
        .arg("--nocapture")
        .env("AGORA_SANDBOX_TEST_RELOCATED_PACKAGE", "1")
        .env("AGORA_SANDBOX_TEST_FILESYSTEM_ROOT", workdir.join("fs"));
    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(command)
        .await
        .unwrap();

    assert!(
        outcome.status().success(),
        "child status: {:?}",
        outcome.status()
    );
    let cached_package = workdir
        .join("fs")
        .join(package.strip_prefix(Path::new("/")).unwrap());
    assert!(!cached_package.join("Contents/Info.plist").exists());
    assert!(
        !cached_package
            .join("Contents/PlugIns/libfixture.dylib")
            .exists()
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn runner_truncates_large_process_audit_without_rejecting_the_command() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-large-argument-test-{}",
        uuid::Uuid::new_v4()
    ));
    let command = SandboxCommand::new(std::env::current_exe().unwrap())
        .arg("process_audit_does_not_reject_a_large_argument")
        .arg("--exact")
        .arg("--nocapture")
        .env("AGORA_SANDBOX_TEST_LARGE_ARGUMENT", "1");
    let config = sandbox_config_in(directory.join("cache"));

    let outcome = Sandbox::new(config, NoopCallback)
        .run(command)
        .await
        .unwrap();

    assert!(outcome.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn runner_control_channels_survive_a_nested_network_sandbox() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-nested-policy-test-{}",
        uuid::Uuid::new_v4()
    ));
    let logical = workspace_root().join(format!(
        "target/agora-sandbox-nested-policy-{}",
        uuid::Uuid::new_v4()
    ));
    let command = SandboxCommand::new("/usr/bin/sandbox-exec")
        .args([
            "-p",
            "(version 1) (allow default) (deny network*)",
            "/bin/bash",
            "-lc",
            "printf nested-control-ok > \"$AGORA_SANDBOX_TEST_NESTED_PATH\" && test \"$(cat \"$AGORA_SANDBOX_TEST_NESTED_PATH\")\" = nested-control-ok",
        ])
        .current_dir(workspace_root())
        .env("AGORA_SANDBOX_TEST_NESTED_PATH", &logical);

    let outcome = Sandbox::new(sandbox_config_in(directory.join("cache")), NoopCallback)
        .run(command)
        .await
        .unwrap();

    assert!(outcome.status().success());
    assert!(!logical.exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn runner_control_channels_reconnect_after_cloexec_default_spawn() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-cloexec-control-test-{}",
        uuid::Uuid::new_v4()
    ));
    let logical = workspace_root().join(format!(
        "target/agora-sandbox-cloexec-control-{}",
        uuid::Uuid::new_v4()
    ));
    let command = SandboxCommand::new(std::env::current_exe().unwrap())
        .arg("cloexec_default_spawn_child_process")
        .arg("--exact")
        .arg("--nocapture")
        .current_dir(workspace_root())
        .env("AGORA_SANDBOX_TEST_CLOEXEC_SPAWN", "1")
        .env("AGORA_SANDBOX_TEST_CLOEXEC_PATH", &logical);

    let outcome = Sandbox::new(sandbox_config_in(directory.join("cache")), NoopCallback)
        .run(command)
        .await
        .unwrap();

    assert!(outcome.status().success());
    assert!(!logical.exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn runner_control_channels_survive_nested_sandbox_and_cloexec_default() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-nested-cloexec-control-test-{}",
        uuid::Uuid::new_v4()
    ));
    let logical = workspace_root().join(format!(
        "target/agora-sandbox-nested-cloexec-control-{}",
        uuid::Uuid::new_v4()
    ));
    let executable = std::env::current_exe().unwrap();
    let command = SandboxCommand::new("/usr/bin/sandbox-exec")
        .args([
            std::ffi::OsStr::new("-p"),
            std::ffi::OsStr::new("(version 1) (allow default) (deny network*)"),
            executable.as_os_str(),
            std::ffi::OsStr::new("cloexec_default_spawn_child_process"),
            std::ffi::OsStr::new("--exact"),
            std::ffi::OsStr::new("--nocapture"),
        ])
        .current_dir(workspace_root())
        .env("AGORA_SANDBOX_TEST_CLOEXEC_SPAWN", "1")
        .env("AGORA_SANDBOX_TEST_CLOEXEC_PATH", &logical);

    let outcome = Sandbox::new(sandbox_config_in(directory.join("cache")), NoopCallback)
        .run(command)
        .await
        .unwrap();

    assert!(outcome.status().success());
    assert!(!logical.exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn runner_executes_shebang_scripts_through_a_prepared_restricted_interpreter() {
    use std::os::unix::fs::PermissionsExt;

    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-shebang-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let workdir = directory.join("cache");
    let script = directory.join("client");
    std::fs::write(
        &script,
        b"#!/usr/bin/env sh\ncase \"$1:$DYLD_INSERT_LIBRARIES\" in direct:*libagora_sandbox.dylib*|nested:*libagora_sandbox.dylib*) exit 0 ;; *) exit 9 ;; esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = sandbox_config_in(&workdir);

    let direct = Sandbox::new(config.clone(), NoopCallback)
        .run(SandboxCommand::new(&script).arg("direct"))
        .await
        .unwrap();

    assert!(direct.status().success());

    let command = format!("{} nested", script.display());
    let nested = Sandbox::new(config, NoopCallback)
        .run(SandboxCommand::new("/bin/bash").args(["-c", &command]))
        .await
        .unwrap();

    assert!(nested.status().success());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn runner_injects_the_configured_tls_ca_path() {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};

    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-tls-environment-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let certificate = directory.join("ca.pem");
    let private_key = directory.join("ca-key.pem");
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca = params.self_signed(&key).unwrap();
    std::fs::write(&certificate, ca.pem()).unwrap();
    std::fs::write(&private_key, key.serialize_pem()).unwrap();
    let mut config = sandbox_config().with_tls_ca(&certificate, &private_key);
    config.network.tls = TlsMode::Auto;
    let private_workdir = config.workdir().to_path_buf();
    let script = format!(
        "AGORA_SANDBOX_TEST_TLS_TRUST_ENV='{}' '{}' \
         records_tls_trust_environment --exact --nocapture",
        private_workdir.display(),
        std::env::current_exe().unwrap().display()
    );
    let command = SandboxCommand::new("/bin/bash").args(["-c", &script]);

    let outcome = Sandbox::new(config, NoopCallback)
        .run(command)
        .await
        .unwrap();

    assert!(outcome.status().success());
    assert!(certificate.exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn copied_bash_routes_system_curl_through_the_proxy() {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let destination = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut bytes = [0_u8; 1024];
            let read = tokio::io::AsyncReadExt::read(&mut stream, &mut bytes)
                .await
                .unwrap();
            assert_ne!(read, 0);
            request.extend_from_slice(&bytes[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        tokio::io::AsyncWriteExt::write_all(
            &mut stream,
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    });
    let events = Arc::new(Mutex::new(Vec::<NetworkEvent>::new()));
    let callback = {
        let events = Arc::clone(&events);
        move |event: Event| {
            if let Some(event) = event.into_network() {
                events.lock().unwrap().push(event);
            }
            std::future::ready(Decision::Allow)
        }
    };
    let script = format!(
        "/usr/bin/curl \
         --silent --show-error --output /dev/null http://{destination}/"
    );

    let run = Sandbox::new(sandbox_config(), callback)
        .run(SandboxCommand::new("/bin/bash").args(["-c", &script]));
    let timeout = sandbox_lifecycle_timeout(15);
    let outcome = tokio::time::timeout(timeout, run)
        .await
        .expect("sandbox shutdown hung after curl exited")
        .unwrap();

    assert!(outcome.status().success());
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.event_type == EventType::NetworkConnectAttempt)
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn runner_terminates_background_descendants_before_returning() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-process-group-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let marker = format!("agora-background-{}", uuid::Uuid::new_v4());
    let script = format!("/bin/sh -c '/bin/sleep 30' {marker} &");

    let outcome = Sandbox::new(sandbox_config(), NoopCallback)
        .run(SandboxCommand::new("/bin/bash").args(["-c", &script]))
        .await
        .unwrap();

    assert!(outcome.status().success());
    assert!(
        !Command::new("/usr/bin/pgrep")
            .args(["-f", marker.as_str()])
            .status()
            .unwrap()
            .success(),
        "sandbox background process still exists"
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn injected_hook_refreshes_process_identity_after_fork() {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let destination = listener.local_addr().unwrap();
    let echo = tokio::spawn(async move {
        let mut connections = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            connections.push(tokio::spawn(async move {
                let mut bytes = [0_u8; 6];
                tokio::io::AsyncReadExt::read_exact(&mut stream, &mut bytes)
                    .await
                    .unwrap();
                tokio::io::AsyncWriteExt::write_all(&mut stream, &bytes)
                    .await
                    .unwrap();
            }));
        }
        for connection in connections {
            connection.await.unwrap();
        }
    });
    let events = Arc::new(Mutex::new(Vec::<NetworkEvent>::new()));
    let callback = {
        let events = Arc::clone(&events);
        move |event: Event| {
            if let Some(event) = event.into_network() {
                events.lock().unwrap().push(event);
            }
            std::future::ready(Decision::Allow)
        }
    };
    let command = SandboxCommand::new(std::env::current_exe().unwrap())
        .arg("forked_intercepted_child_process")
        .arg("--exact")
        .arg("--nocapture")
        .env("AGORA_SANDBOX_TEST_FORK_CHILD", "1")
        .env("AGORA_SANDBOX_TEST_DESTINATION", destination.to_string());

    let outcome = Sandbox::new(sandbox_config(), callback)
        .run(command)
        .await
        .unwrap();
    assert!(outcome.status().success());
    tokio::time::timeout(Duration::from_secs(2), echo)
        .await
        .unwrap()
        .unwrap();

    let events = events.lock().unwrap();
    let attempts = events
        .iter()
        .filter(|event| event.event_type == EventType::NetworkConnectAttempt)
        .collect::<Vec<_>>();
    assert_eq!(attempts.len(), 2);
    assert_ne!(attempts[0].process.pid, attempts[1].process.pid);
    assert!(
        attempts[0].process.ppid == attempts[1].process.pid
            || attempts[1].process.ppid == attempts[0].process.pid
    );
    for event in &attempts {
        assert!(
            event
                .connection_id
                .as_deref()
                .unwrap()
                .starts_with(&format!("{}-", event.process.pid))
        );
    }
    assert_ne!(attempts[0].connection_id, attempts[1].connection_id);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn child_creation_fails_closed_from_an_unlinked_managed_current_directory() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-unlinked-current-directory-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    let host_target = source.join("host.txt");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(&host_target, b"host").unwrap();
    let command = SandboxCommand::new(std::env::current_exe().unwrap())
        .arg("unlinked_current_directory_child_process")
        .arg("--exact")
        .arg("--nocapture")
        .current_dir(&source)
        .env("AGORA_SANDBOX_TEST_UNLINKED_CWD_CHILD", "1")
        .env("AGORA_SANDBOX_TEST_HOST_TARGET", &host_target);

    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(command)
        .await
        .unwrap();

    assert!(outcome.status().success());
    assert_eq!(std::fs::read(&host_target).unwrap(), b"host");
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn forked_exec_ignores_path_refresh_for_an_unlinked_open_file() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-unlinked-open-file-test-{}",
        uuid::Uuid::new_v4()
    ));
    let source = directory.join("source");
    let workdir = directory.join("sandbox");
    let unlinked = source.join("unlinked.lock");
    std::fs::create_dir_all(&source).unwrap();
    let command = SandboxCommand::new(std::env::current_exe().unwrap())
        .arg("unlinked_open_file_exec_child_process")
        .arg("--exact")
        .arg("--nocapture")
        .current_dir(&source)
        .env("AGORA_SANDBOX_TEST_UNLINKED_OPEN_FILE", &unlinked);

    let outcome = Sandbox::new(sandbox_config_in(&workdir), NoopCallback)
        .run(command)
        .await
        .unwrap();

    assert!(outcome.status().success());
    assert!(!unlinked.exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn injected_hook_preserves_nonblocking_connect_and_poll_semantics() {
    assert_injected_nonblocking_connection(
        "nonblocking_intercepted_child_process",
        "AGORA_SANDBOX_TEST_NONBLOCKING_CHILD",
    )
    .await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn injected_hook_preserves_nonblocking_connectx_and_poll_semantics() {
    assert_injected_nonblocking_connection(
        "nonblocking_connectx_intercepted_child_process",
        "AGORA_SANDBOX_TEST_NONBLOCKING_CONNECTX_CHILD",
    )
    .await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn injected_hook_blocks_unsupported_connectx_without_direct_fallback() {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let destination = listener.local_addr().unwrap();
    let command = SandboxCommand::new(std::env::current_exe().unwrap())
        .arg("unsupported_connectx_intercepted_child_process")
        .arg("--exact")
        .arg("--nocapture")
        .env("AGORA_SANDBOX_TEST_UNSUPPORTED_CONNECTX_CHILD", "1")
        .env("AGORA_SANDBOX_TEST_DESTINATION", destination.to_string());
    let outcome = Sandbox::new(sandbox_config(), NoopCallback)
        .run(command)
        .await
        .unwrap();

    assert!(outcome.status().success());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn injected_hook_blocks_tcp_when_runtime_configuration_is_missing() {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let destination = listener.local_addr().unwrap();
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .arg("missing_hook_configuration_child_process")
        .arg("--exact")
        .arg("--nocapture")
        .env("AGORA_SANDBOX_TEST_MISSING_CONFIG_CHILD", "1")
        .env("AGORA_SANDBOX_TEST_DESTINATION", destination.to_string())
        .env("DYLD_INSERT_LIBRARIES", hook_library())
        .env_remove("AGORA_SANDBOX_TOKEN")
        .env_remove("AGORA_SANDBOX_PROXY_IPV4")
        .env_remove("AGORA_SANDBOX_PROXY_IPV6");

    let status = command.status().await.unwrap();

    assert!(status.success(), "child status: {status:?}");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

#[cfg(target_os = "macos")]
async fn assert_injected_nonblocking_connection(child_test: &str, child_environment: &str) {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let destination = listener.local_addr().unwrap();
    let echo = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = [0_u8; 6];
        tokio::io::AsyncReadExt::read_exact(&mut stream, &mut bytes)
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut stream, &bytes)
            .await
            .unwrap();
    });
    let callback = |event: Event| async move {
        if event
            .as_network()
            .is_some_and(|event| event.event_type == EventType::NetworkConnectAttempt)
        {
            tokio::time::sleep(Duration::from_millis(750)).await;
        }
        Decision::Allow
    };
    let command = SandboxCommand::new(std::env::current_exe().unwrap())
        .arg(child_test)
        .arg("--exact")
        .arg("--nocapture")
        .env(child_environment, "1")
        .env("AGORA_SANDBOX_TEST_DESTINATION", destination.to_string());
    let outcome = Sandbox::new(sandbox_config(), callback)
        .run(command)
        .await
        .unwrap();

    assert!(
        outcome.status().success(),
        "child status: {:?}",
        outcome.status()
    );
    tokio::time::timeout(Duration::from_secs(2), echo)
        .await
        .unwrap()
        .unwrap();
}
