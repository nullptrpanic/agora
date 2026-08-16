#[cfg(target_os = "macos")]
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "macos")]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
fn byte_occurrences(bytes: &[u8], needle: &[u8]) -> usize {
    bytes
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

#[cfg(target_os = "macos")]
fn wait_for_pty_output(
    output: &mut std::process::ChildStdout,
    transcript: &mut Vec<u8>,
    needle: &[u8],
    occurrences: usize,
    deadline: Instant,
) -> std::io::Result<bool> {
    let mut chunk = [0_u8; 4096];
    loop {
        match output.read(&mut chunk) {
            Ok(0) => return Ok(byte_occurrences(transcript, needle) >= occurrences),
            Ok(length) => transcript.extend_from_slice(&chunk[..length]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        if byte_occurrences(transcript, needle) >= occurrences {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn cli_workdir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "agora-sandbox-cli-workdir-{}",
        uuid::Uuid::new_v4()
    ))
}

#[cfg(target_os = "macos")]
fn write_cli_config(
    directory: &Path,
    workdir: &Path,
    tls: &str,
    encryption: &str,
    key: Option<&str>,
    log_file: Option<&Path>,
) -> PathBuf {
    std::fs::create_dir_all(directory).unwrap();
    let mut local = serde_json::json!({ "encrypt": encryption });
    if let Some(key) = key {
        local["key"] = serde_json::Value::String(key.to_string());
    }
    let log = log_file
        .map(|path| serde_json::json!({ "file": path }))
        .unwrap_or_else(|| serde_json::json!({}));
    let config = serde_json::json!({
        "workdir": workdir,
        "tls": tls,
        "filesystem": {
            "local": local,
            "nfs": []
        },
        "log": log
    });
    let path = directory.join("sandbox.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn configured_command(config: &Path, executable: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agora-sandbox"));
    command
        .arg("run")
        .arg("-c")
        .arg(config)
        .arg("-e")
        .arg(executable);
    command
}

#[test]
fn sandbox_cli_documents_only_available_options() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agora-sandbox"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("run"));
    assert!(stdout.contains("migrate-key"));
    assert!(!stdout.contains("__session-daemon"));
    assert!(!stdout.contains("--smb-config"));
    assert!(!stdout.contains("--filesystem-key"));

    let run = std::process::Command::new(env!("CARGO_BIN_EXE_agora-sandbox"))
        .args(["run", "--help"])
        .output()
        .unwrap();
    assert!(run.status.success());
    let run = String::from_utf8_lossy(&run.stdout);
    assert!(run.contains("-c, --config <CONFIG>"));
    assert!(run.contains("-e, --executable <EXECUTABLE>"));
    assert!(!run.contains("--workdir"));
    assert!(!run.contains("--tls"));
    assert!(!run.contains("--audit-file"));

    let removed = std::process::Command::new(env!("CARGO_BIN_EXE_agora-sandbox"))
        .args(["tls", "generate"])
        .output()
        .unwrap();
    assert!(!removed.status.success());
}

#[test]
fn sandbox_cli_exposes_the_web_viewer() {
    let output = Command::new(env!("CARGO_BIN_EXE_agora-sandbox"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("web"));
}

#[test]
fn sandbox_web_requires_a_config_file() {
    let output = Command::new(env!("CARGO_BIN_EXE_agora-sandbox"))
        .arg("web")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--config <CONFIG>"));
}

#[test]
fn sandbox_web_documents_only_viewer_options() {
    let output = Command::new(env!("CARGO_BIN_EXE_agora-sandbox"))
        .args(["web", "--help"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("-c, --config <CONFIG>"));
    assert!(stdout.contains("--no-open"));
    assert!(!stdout.contains("--sandbox-bin"));
    assert!(!stdout.contains("--command"));
    assert!(!stdout.contains("--shell"));
    assert!(!stdout.contains("--listen"));
}

#[test]
fn sandbox_web_rejects_browser_selected_execution_options() {
    let output = Command::new(env!("CARGO_BIN_EXE_agora-sandbox"))
        .args(["web", "--config", "sandbox.json", "--shell", "/bin/zsh"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument '--shell'"));
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_cli_runs_from_one_strict_config_file() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("sandbox.json");
    std::fs::write(
        &config,
        r#"{
          "workdir": "workdir",
          "tls": "off",
          "filesystem": {
            "local": { "encrypt": "plain" },
            "nfs": []
          },
          "log": {}
        }"#,
    )
    .unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_agora-sandbox"))
        .arg("run")
        .arg("-c")
        .arg("sandbox.json")
        .args(["-e", "/usr/bin/true"])
        .current_dir(root.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(root.path().join("workdir/fs").is_dir());
    assert!(
        root.path()
            .join("workdir/runtime/logs/sandbox.log")
            .is_file()
    );
}

#[test]
fn sandbox_cli_documents_interactive_key_migration() {
    let output = Command::new(env!("CARGO_BIN_EXE_agora-sandbox"))
        .args(["migrate-key", "--help"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--workdir <WORKDIR>"));
    assert!(stdout.contains("Interactively"));
    assert!(!stdout.contains("--filesystem-key"));
    assert!(!stdout.contains("--new-filesystem-key"));
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_cli_runs_with_a_plain_local_filesystem() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workdir");
    let config = write_cli_config(root.path(), &workdir, "off", "plain", None, None);
    let output = configured_command(&config, "/usr/bin/true")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(workdir.join("fs").is_dir());
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_cli_preserves_the_child_exit_code() {
    let root = tempfile::tempdir().unwrap();
    let config = write_cli_config(
        root.path(),
        &root.path().join("workdir"),
        "off",
        "plain",
        None,
        None,
    );

    let output = configured_command(&config, "/bin/bash -c 'exit 7'")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
}

#[cfg(target_os = "macos")]
#[test]
fn shared_session_allows_a_second_command_in_the_same_workdir() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workdir");
    let data = root.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let config = write_cli_config(root.path(), &workdir, "off", "plain", None, None);
    let data = data.to_string_lossy();
    let wait = format!(
        "import os,time; os.chdir({data:?}); print('READY',flush=True); exec(\"while not os.path.exists('second-finished'): time.sleep(0.05)\")"
    );
    let wait = format!("/usr/bin/python3 -c {}", shell_words::quote(&wait));
    let mut first = configured_command(&config, wait)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = String::new();
    let mut stdout = std::io::BufReader::new(first.stdout.take().unwrap());
    std::io::BufRead::read_line(&mut stdout, &mut output).unwrap();
    assert_eq!(output, "READY\n");

    let finish = format!("cd {} && : > second-finished", shell_words::quote(&data));
    let finish = format!("/bin/bash -c {}", shell_words::quote(&finish));
    let second = configured_command(&config, finish).output().unwrap();

    assert!(
        second.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(first.wait().unwrap().success());
}

#[cfg(target_os = "macos")]
#[test]
fn shared_session_accepts_a_different_direct_command() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workdir");
    let data = root.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let config = write_cli_config(root.path(), &workdir, "off", "plain", None, None);
    let wait = format!(
        "import os,time; os.chdir({:?}); print('READY',flush=True); exec(\"while not os.path.exists('done'): time.sleep(0.05)\")",
        data.to_string_lossy()
    );
    let wait = format!("/usr/bin/python3 -c {}", shell_words::quote(&wait));
    let mut first = configured_command(&config, wait)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(first.stdout.take().unwrap()),
        &mut ready,
    )
    .unwrap();
    assert_eq!(ready, "READY\n");

    let second = configured_command(&config, format!("/bin/ls -d {}", data.display()))
        .output()
        .unwrap();

    assert!(second.status.success());
    assert_eq!(
        String::from_utf8_lossy(&second.stdout).trim(),
        data.to_string_lossy()
    );
    std::fs::write(data.join("done"), b"").unwrap();
    assert!(first.wait().unwrap().success());
}

#[cfg(target_os = "macos")]
#[test]
fn shared_session_makes_encrypted_writes_visible_between_commands() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workdir");
    let data = root.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let config = write_cli_config(
        root.path(),
        &workdir,
        "off",
        "encrypted",
        Some("shared-session-filesystem-key"),
        None,
    );
    let data = data.to_string_lossy();
    let data = shell_words::quote(&data);
    let wait_script = format!(
        "cd {data} && echo READY && while [ ! -f .session-done ]; do /bin/sleep 0.05; done && /bin/cat shared-session.txt"
    );
    let wait_command = format!("/bin/bash -c {}", shell_words::quote(&wait_script));
    let mut first = configured_command(&config, wait_command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = std::io::BufReader::new(first.stdout.take().unwrap());
    let mut ready = String::new();
    std::io::BufRead::read_line(&mut stdout, &mut ready).unwrap();
    assert_eq!(ready, "READY\n");

    let write_script =
        format!("cd {data} && printf shared-value > shared-session.txt && : > .session-done");
    let write_command = format!("/bin/bash -c {}", shell_words::quote(&write_script));
    let second = configured_command(&config, write_command).output().unwrap();
    assert!(
        second.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    let first_status = loop {
        if let Some(status) = first.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = first.kill();
            panic!("first shared-session command did not observe the peer write");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut remaining = String::new();
    stdout.read_to_string(&mut remaining).unwrap();
    assert!(first_status.success());
    assert_eq!(remaining, "shared-value");
    assert!(!root.path().join("data/shared-session.txt").exists());
    assert!(!root.path().join("data/.session-done").exists());

    let read_script = format!("cd {data} && /bin/cat shared-session.txt");
    let read_command = format!("/bin/bash -c {}", shell_words::quote(&read_script));
    let persisted = configured_command(&config, read_command).output().unwrap();
    assert!(persisted.status.success());
    assert_eq!(persisted.stdout, b"shared-value");
}

#[cfg(target_os = "macos")]
#[test]
fn shared_session_preserves_file_lock_exclusion_between_commands() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workdir");
    let data = root.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let config = write_cli_config(
        root.path(),
        &workdir,
        "off",
        "encrypted",
        Some("shared-session-lock-key"),
        None,
    );
    let data = data.to_string_lossy();
    let holder = format!(
        "import fcntl, os, time; os.chdir({data:?}); f=open('locked.txt','a+'); fcntl.flock(f,fcntl.LOCK_EX); print('LOCKED',flush=True); exec(\"while not os.path.exists('lock-done'): time.sleep(0.05)\")"
    );
    let holder = format!("/usr/bin/python3 -c {}", shell_words::quote(&holder));
    let mut first = configured_command(&config, holder)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = std::io::BufReader::new(first.stdout.take().unwrap());
    let mut ready = String::new();
    std::io::BufRead::read_line(&mut stdout, &mut ready).unwrap();
    assert_eq!(ready, "LOCKED\n");

    let contender = format!(
        "import fcntl, os, sys; os.chdir({data:?}); f=open('locked.txt','a+');\ntry:\n fcntl.flock(f,fcntl.LOCK_EX|fcntl.LOCK_NB); sys.exit(9)\nexcept BlockingIOError:\n open('lock-done','w').close()"
    );
    let contender = format!("/usr/bin/python3 -c {}", shell_words::quote(&contender));
    let second = configured_command(&config, contender).output().unwrap();
    assert!(
        second.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    let first_status = loop {
        if let Some(status) = first.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = first.kill();
            panic!("file-lock holder did not observe the peer completion file");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(first_status.success());
    assert!(!root.path().join("data/locked.txt").exists());
    assert!(!root.path().join("data/lock-done").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn shared_session_preserves_sqlite_wal_locking_and_commits() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workdir");
    let data = root.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let config = write_cli_config(
        root.path(),
        &workdir,
        "off",
        "encrypted",
        Some("shared-session-sqlite-key"),
        None,
    );
    let data = data.to_string_lossy();
    let writer = format!(
        "import os, sqlite3, time; os.chdir({data:?}); c=sqlite3.connect('session.db',timeout=0); c.execute('PRAGMA journal_mode=WAL'); c.execute('CREATE TABLE IF NOT EXISTS records(value TEXT)'); c.commit(); c.execute('BEGIN IMMEDIATE'); c.execute(\"INSERT INTO records VALUES ('committed')\"); print('READY',flush=True); exec(\"while not os.path.exists('sqlite-done'): time.sleep(0.05)\"); c.commit()"
    );
    let writer = format!("/usr/bin/python3 -c {}", shell_words::quote(&writer));
    let mut first = configured_command(&config, writer)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = std::io::BufReader::new(first.stdout.take().unwrap());
    let mut ready = String::new();
    std::io::BufRead::read_line(&mut stdout, &mut ready).unwrap();
    assert_eq!(ready, "READY\n");

    let contender = format!(
        "import os, sqlite3, sys; os.chdir({data:?}); c=sqlite3.connect('session.db',timeout=0);\ntry:\n c.execute('BEGIN IMMEDIATE'); sys.exit(9)\nexcept sqlite3.OperationalError as e:\n assert 'locked' in str(e).lower(), str(e)\nopen('sqlite-done','w').close()"
    );
    let contender = format!("/usr/bin/python3 -c {}", shell_words::quote(&contender));
    let second = configured_command(&config, contender).output().unwrap();
    assert!(
        second.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    let first_status = loop {
        if let Some(status) = first.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = first.kill();
            panic!("SQLite writer did not finish after the peer released it");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(first_status.success());

    let reader = format!(
        "import os, sqlite3; os.chdir({data:?}); print(sqlite3.connect('session.db').execute('SELECT value FROM records').fetchone()[0],end='')"
    );
    let reader = format!("/usr/bin/python3 -c {}", shell_words::quote(&reader));
    let committed = configured_command(&config, reader).output().unwrap();
    assert!(
        committed.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&committed.stdout),
        String::from_utf8_lossy(&committed.stderr)
    );
    assert_eq!(committed.stdout, b"committed");
    assert!(!root.path().join("data/session.db").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn shared_session_rejects_a_different_effective_configuration() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workdir");
    let first_config = write_cli_config(
        &root.path().join("first"),
        &workdir,
        "off",
        "plain",
        None,
        None,
    );
    let second_config = write_cli_config(
        &root.path().join("second"),
        &workdir,
        "off",
        "plain",
        None,
        Some(&workdir.join("runtime/logs/other.log")),
    );
    let mut first = configured_command(&first_config, "/bin/bash -c 'echo READY; /bin/sleep 3'")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = std::io::BufReader::new(first.stdout.take().unwrap());
    let mut ready = String::new();
    std::io::BufRead::read_line(&mut stdout, &mut ready).unwrap();
    assert_eq!(ready, "READY\n");

    let rejected = configured_command(&second_config, "/usr/bin/true")
        .output()
        .unwrap();

    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("sandbox session configuration mismatch"),
        "stderr={}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(first.wait().unwrap().success());
}

#[cfg(target_os = "macos")]
#[test]
fn concurrent_first_entries_elect_one_workspace_session() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workdir");
    let data = root.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let config = write_cli_config(
        root.path(),
        &workdir,
        "off",
        "encrypted",
        Some("concurrent-session-election-key"),
        None,
    );
    let data = data.to_string_lossy();
    let first_script = format!(
        "import os,time; os.chdir({data:?}); open('first-ready','w').close(); exec(\"while not os.path.exists('second-ready'): time.sleep(0.05)\")"
    );
    let second_script = format!(
        "import os,time; os.chdir({data:?}); open('second-ready','w').close(); exec(\"while not os.path.exists('first-ready'): time.sleep(0.05)\")"
    );
    let first_command = format!("/usr/bin/python3 -c {}", shell_words::quote(&first_script));
    let second_command = format!("/usr/bin/python3 -c {}", shell_words::quote(&second_script));
    let mut first = configured_command(&config, first_command).spawn().unwrap();
    let mut second = configured_command(&config, second_command).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let (first_status, second_status) = loop {
        let first_status = first.try_wait().unwrap();
        let second_status = second.try_wait().unwrap();
        if let (Some(first_status), Some(second_status)) = (first_status, second_status) {
            break (first_status, second_status);
        }
        if Instant::now() >= deadline {
            let _ = first.kill();
            let _ = second.kill();
            let _ = first.wait();
            let _ = second.wait();
            panic!("concurrent first entries did not join one workspace session");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(first_status.success());
    assert!(second_status.success());
    assert!(!root.path().join("data/first-ready").exists());
    assert!(!root.path().join("data/second-ready").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn final_session_client_returns_after_the_workspace_lock_is_released() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workdir");
    let config = write_cli_config(root.path(), &workdir, "off", "plain", None, None);

    let output = configured_command(&config, "/usr/bin/true")
        .output()
        .unwrap();

    assert!(output.status.success());
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(workdir.join("fs/.fs.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "CLI returned before the session daemon released .fs.lock: {}",
        std::io::Error::last_os_error()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn copied_cli_materializes_its_hook_without_a_sidecar() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    let workdir = root.path().join("workdir");
    std::fs::create_dir(&bin).unwrap();
    let executable = bin.join("agora-sandbox");
    std::fs::copy(env!("CARGO_BIN_EXE_agora-sandbox"), &executable).unwrap();
    assert!(!bin.join("libagora_sandbox.dylib").exists());
    let config = write_cli_config(root.path(), &workdir, "off", "plain", None, None);

    let output = Command::new(&executable)
        .arg("run")
        .arg("-c")
        .arg(&config)
        .args(["-e", "/usr/bin/true"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let versions = std::fs::read_dir(workdir.join("runtime/hook"))
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.path().is_dir())
        .collect::<Vec<_>>();
    assert_eq!(versions.len(), 1);
    let checksum = versions[0].file_name();
    let checksum = checksum.to_str().unwrap();
    assert_eq!(checksum.len(), 32);
    assert!(
        checksum
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert!(versions[0].path().join("libagora_sandbox.dylib").is_file());
}

#[test]
fn sandbox_cli_rejects_a_key_in_plain_filesystem_mode() {
    let root = tempfile::tempdir().unwrap();
    let config = write_cli_config(
        root.path(),
        &root.path().join("workdir"),
        "off",
        "plain",
        Some("unused"),
        None,
    );
    let output = configured_command(&config, "/usr/bin/true")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("filesystem.local.key is not allowed when encrypt is plain")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_cli_migrates_the_encrypted_filesystem_key_in_place() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workdir");
    let run = |key: &str| {
        let config = write_cli_config(root.path(), &workdir, "off", "encrypted", Some(key), None);
        configured_command(&config, "/usr/bin/true")
            .output()
            .unwrap()
    };

    let initialized = run("old-filesystem-key");
    assert!(
        initialized.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&initialized.stdout),
        String::from_utf8_lossy(&initialized.stderr)
    );

    let mut migrated = Command::new(env!("CARGO_BIN_EXE_agora-sandbox"));
    migrated
        .arg("migrate-key")
        .arg("--workdir")
        .arg(&workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut migrated = migrated.spawn().unwrap();
    migrated
        .stdin
        .take()
        .unwrap()
        .write_all(b"old-filesystem-key\nnew-filesystem-key\n")
        .unwrap();
    let migrated = migrated.wait_with_output().unwrap();
    assert!(
        migrated.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&migrated.stdout),
        String::from_utf8_lossy(&migrated.stderr)
    );
    let stdout = String::from_utf8_lossy(&migrated.stdout);
    assert!(stdout.contains("Current filesystem key"), "{stdout}");
    assert!(stdout.contains("New filesystem key"), "{stdout}");
    assert!(stdout.contains("100%"), "{stdout}");

    let old_key = run("old-filesystem-key");
    assert!(!old_key.status.success());
    assert!(String::from_utf8_lossy(&old_key.stderr).contains("key is incorrect"));

    let new_key = run("new-filesystem-key");
    assert!(
        new_key.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&new_key.stdout),
        String::from_utf8_lossy(&new_key.stderr)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_cli_prompts_for_migration_keys_in_a_terminal() {
    let workdir = cli_workdir();
    let mut process = Command::new("/usr/bin/script");
    process
        .arg("-q")
        .arg("/dev/null")
        .arg(env!("CARGO_BIN_EXE_agora-sandbox"))
        .arg("migrate-key")
        .arg("--workdir")
        .arg(&workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"old-filesystem-key\nnew-filesystem-key\n")
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("interactive key migration did not exit before the deadline");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = child.wait_with_output().unwrap();

    assert!(!status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Current filesystem key"), "{stdout}");
    assert!(stdout.contains("New filesystem key"), "{stdout}");
    assert!(stdout.contains("Migration progress"), "{stdout}");
    assert!(stdout.contains("5%"), "{stdout}");
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_cli_shell_can_execute_after_ctrl_c_interrupts_the_prompt() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().join("workdir");
    let data = directory.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let config = write_cli_config(directory.path(), &workdir, "off", "plain", None, None);
    let prompt = b"__AGORA_PROMPT__ ";
    let status_prefix = b"__AGORA_TRUE_STATUS__=";
    let logical = data.join("after-sigint.txt");

    let mut process = Command::new("/usr/bin/script");
    process
        .arg("-q")
        .arg("/dev/null")
        .arg(env!("CARGO_BIN_EXE_agora-sandbox"))
        .arg("run")
        .arg("-c")
        .arg(&config)
        .arg("-e")
        .arg("/bin/bash --noprofile --norc -i")
        .env("PS1", std::str::from_utf8(prompt).unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    let flags = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(output.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut transcript = Vec::new();
    let mut wait_for = |needle: &[u8], occurrences: usize| {
        let found =
            wait_for_pty_output(&mut output, &mut transcript, needle, occurrences, deadline)
                .unwrap();
        if !found {
            child.kill().ok();
            child.wait().ok();
            panic!(
                "PTY output did not contain {:?}; transcript={}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&transcript)
            );
        }
    };

    wait_for(prompt, 1);
    input.write_all(b"stty -echo\n").unwrap();
    wait_for(prompt, 2);
    input.write_all(b"\x03").unwrap();
    wait_for(prompt, 3);
    let logical_argument = logical.to_string_lossy().into_owned();
    let quoted_logical = shell_words::quote(&logical_argument);
    let command = format!(
        "printf sandboxed > {quoted_logical}; /bin/cat {quoted_logical}; /usr/bin/true; printf '__AGORA_TRUE_STATUS__=%s\\n' \"$?\"; exit\n"
    );
    input.write_all(command.as_bytes()).unwrap();
    wait_for(status_prefix, 1);

    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().ok();
            child.wait().ok();
            panic!(
                "interactive sandbox shell did not exit; transcript={}",
                String::from_utf8_lossy(&transcript)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    output.read_to_end(&mut transcript).unwrap();
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();

    assert!(
        status.success(),
        "status={status}; transcript={}; stderr={}",
        String::from_utf8_lossy(&transcript),
        String::from_utf8_lossy(&stderr)
    );
    assert!(
        transcript
            .windows(b"__AGORA_TRUE_STATUS__=0".len())
            .any(|window| window == b"__AGORA_TRUE_STATUS__=0"),
        "transcript={}; stderr={}",
        String::from_utf8_lossy(&transcript),
        String::from_utf8_lossy(&stderr)
    );
    assert!(
        transcript
            .windows(b"sandboxed".len())
            .any(|window| window == b"sandboxed"),
        "transcript={}; stderr={}",
        String::from_utf8_lossy(&transcript),
        String::from_utf8_lossy(&stderr)
    );
    assert!(!logical.exists(), "post-SIGINT write bypassed the overlay");
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_cli_auto_generates_reuses_and_replaces_its_workdir_tls_ca() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().join("workdir");
    let certificate = workdir.join("ca/ca.crt");
    let private_key = workdir.join("ca/ca.key");
    let config = write_cli_config(directory.path(), &workdir, "auto", "plain", None, None);

    let run = || {
        configured_command(&config, "/usr/bin/true")
            .output()
            .unwrap()
    };

    let generated = run();
    assert!(
        generated.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&generated.stdout),
        String::from_utf8_lossy(&generated.stderr)
    );
    let certificate_pem = std::fs::read_to_string(&certificate).unwrap();
    let private_key_pem = std::fs::read_to_string(&private_key).unwrap();
    assert!(certificate_pem.starts_with("-----BEGIN CERTIFICATE-----"));
    assert!(private_key_pem.starts_with("-----BEGIN PRIVATE KEY-----"));
    assert_eq!(
        certificate.metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        private_key.metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );

    let reused = run();
    assert!(
        reused.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&reused.stdout),
        String::from_utf8_lossy(&reused.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&certificate).unwrap(),
        certificate_pem
    );
    assert_eq!(
        std::fs::read_to_string(&private_key).unwrap(),
        private_key_pem
    );

    std::fs::remove_file(&private_key).unwrap();
    let regenerated = run();
    assert!(
        regenerated.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&regenerated.stdout),
        String::from_utf8_lossy(&regenerated.stderr)
    );
    assert_ne!(
        std::fs::read_to_string(&certificate).unwrap(),
        certificate_pem
    );
    assert_ne!(
        std::fs::read_to_string(&private_key).unwrap(),
        private_key_pem
    );
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_cli_encrypted_ls_lists_upper_file() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workdir");
    let config = write_cli_config(
        root.path(),
        &workdir,
        "off",
        "encrypted",
        Some("interactive-filesystem-key"),
        None,
    );
    let directory = root.path().to_string_lossy();
    let directory = shell_words::quote(&directory);
    let create_script = format!("cd {directory} && printf AGORA_UPPER_ONLY > interactive.txt");
    let create = format!("/bin/bash -c {}", shell_words::quote(&create_script));
    let created = configured_command(&config, create).output().unwrap();
    assert!(
        created.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&created.stdout),
        String::from_utf8_lossy(&created.stderr)
    );

    let list = format!("/bin/ls -1 {directory}");
    let output = configured_command(&config, list).output().unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.trim_end_matches('\r') == "interactive.txt"),
        "stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_cli_external_command_writes_to_encrypted_redirection() {
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workdir");
    let audit = root.path().join("audit.jsonl");
    let config = write_cli_config(
        root.path(),
        &workdir,
        "off",
        "encrypted",
        Some("redirection-filesystem-key"),
        Some(&audit),
    );
    let directory = root.path().to_string_lossy();
    let directory = shell_words::quote(&directory);
    let script = format!(
        "cd {directory} && /bin/echo inherited-output > redirected.txt && /bin/cat redirected.txt"
    );
    let command = format!("/bin/bash -c {}", shell_words::quote(&script));
    let output = configured_command(&config, command).output().unwrap();

    assert!(
        output.status.success(),
        "stdout={}\nstderr={}\naudit={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        std::fs::read_to_string(&audit).unwrap_or_default(),
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "inherited-output\n"
    );
}

#[test]
fn sandbox_cli_rejects_unknown_config_fields() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("sandbox.json");
    std::fs::write(
        &config,
        r#"{
          "workdir": "workdir",
          "tls": "off",
          "filesystem": { "local": { "encrypt": "plain" } },
          "tls_ca_cert": "/tmp/ca.pem"
        }"#,
    )
    .unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let output = configured_command(&config, "/bin/true").output().unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown field `tls_ca_cert`"));
}

#[test]
fn sandbox_cli_requires_a_command() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agora-sandbox"))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Usage: agora-sandbox <COMMAND>"));
}

#[test]
fn intercepted_cli_child() {
    if std::env::var_os("AGORA_SANDBOX_TEST_CLI_CHILD").is_none() {
        return;
    }

    let destination = std::env::var("AGORA_SANDBOX_TEST_DESTINATION").unwrap();
    let mut stream = TcpStream::connect(destination).unwrap();
    let request = b"GET / HTTP/1.1\r\nHost: audit.example\r\nConnection: close\r\n\r\n";
    stream.write_all(request).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut echoed = Vec::new();
    stream.read_to_end(&mut echoed).unwrap();
    assert_eq!(echoed, request);
}

#[test]
fn sandbox_cli_writes_audit_to_the_default_workspace_log() {
    let (output, destination, records) = run_audited_cli(None);

    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(audit_records(&output.stdout).is_empty());
    assert!(audit_records(&output.stderr).is_empty());
    let network_records = records
        .iter()
        .filter(|record| record["audit"]["type"] == "network")
        .collect::<Vec<_>>();
    assert_eq!(network_records.len(), 1, "records={records:?}");
    assert_audit_record(network_records[0], destination);
}

#[test]
fn sandbox_cli_appends_structured_logs_to_the_configured_file() {
    let temp = std::env::temp_dir().join(format!("agora-sandbox-log-{}", uuid::Uuid::new_v4()));
    let log_file = temp.join("nested/sandbox.log");

    let (first, first_destination, first_records) = run_audited_cli(Some(&log_file));
    let (second, second_destination, records) = run_audited_cli(Some(&log_file));

    assert!(
        first.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(audit_records(&first.stdout).is_empty());
    assert!(audit_records(&second.stdout).is_empty());
    assert_eq!(
        first_records
            .iter()
            .filter(|record| record["audit"]["type"] == "network")
            .count(),
        1
    );
    let network_records = records
        .iter()
        .filter(|record| record["audit"]["type"] == "network")
        .collect::<Vec<_>>();
    assert_eq!(network_records.len(), 2);
    assert_audit_record(network_records[0], first_destination);
    assert_audit_record(network_records[1], second_destination);
    std::fs::remove_dir_all(temp).unwrap();
}

fn run_audited_cli(log_file: Option<&Path>) -> (Output, SocketAddr, Vec<serde_json::Value>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let destination = listener.local_addr().unwrap();
    let echo = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        stream.write_all(&bytes).unwrap();
    });
    let test_binary = std::env::current_exe().unwrap();
    let command = format!(
        "'{}' intercepted_cli_child --exact --nocapture",
        test_binary.display()
    );
    let root = tempfile::tempdir().unwrap();
    let workdir = root.path().join("workdir");
    let expected_log = log_file
        .map(|path| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                workdir.join(path)
            }
        })
        .unwrap_or_else(|| workdir.join("runtime/logs/sandbox.log"));
    let config = write_cli_config(root.path(), &workdir, "off", "plain", None, log_file);
    let mut process = configured_command(&config, command);
    process
        .env("AGORA_SANDBOX_TEST_CLI_CHILD", "1")
        .env("AGORA_SANDBOX_TEST_DESTINATION", destination.to_string());
    let output = process.output().unwrap();

    if !output.status.success() {
        drop(TcpStream::connect(destination));
    }
    echo.join().unwrap();
    let records = audit_records(&std::fs::read(expected_log).unwrap());
    (output, destination, records)
}

fn audit_records(output: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| {
            let record = line.get(line.find('{')?..)?;
            serde_json::from_str(record).ok()
        })
        .collect()
}

fn assert_audit_record(record: &serde_json::Value, destination: SocketAddr) {
    assert_eq!(record["message"], "sandbox audit event");
    assert_eq!(record["level"], "INFO");
    assert!(record["time"].as_str().is_some());
    let record = &record["audit"];
    let object = record.as_object().unwrap();
    assert_eq!(object.len(), 7);
    assert_eq!(record["type"], "network");
    assert!(record["access_time"].as_str().is_some());
    assert!(
        record["trace_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert!(record["pid"].as_u64().is_some_and(|pid| pid > 0));
    assert_eq!(record["destination_ip"], destination.ip().to_string());
    assert_eq!(record["destination_port"], destination.port());
    assert_eq!(record["domain"], "audit.example");
}
