use agora_node::config::{AgentType, ChannelConfig, IsolateMode, NodeConfig};
use std::io::Write as _;

#[test]
fn node_has_library_and_binary_and_requires_a_subcommand() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(manifest_dir.join("src/lib.rs").exists());
    assert!(manifest_dir.join("src/main.rs").exists());

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agora-node"))
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("<COMMAND>"));
}

#[test]
fn node_rejects_removed_manual_task_flags() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agora-node"))
        .arg("--task")
        .arg("hello")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unexpected argument"));
}

#[test]
fn node_accepts_empty_config_without_starting_channel() {
    let temp = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("agents.json");
    std::fs::write(&config_path, r#"{"channels":[],"agents":[]}"#).unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agora-node"))
        .arg("daemon")
        .arg("--config")
        .arg(config_path)
        .env("HOME", temp.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"message\":\"loaded 0 channels and 0 agents\""));
    assert!(
        temp.path()
            .join(".agora")
            .join("db")
            .join("store.db")
            .exists()
    );
}

#[test]
fn node_config_generate_builds_a_telegram_config_without_detected_codex() {
    let temp = tempfile::tempdir().unwrap();
    let empty_path = tempfile::tempdir().unwrap();
    let output = run_config_generate(
        temp.path(),
        empty_path.path(),
        "-g",
        std::path::Path::new("config2.json"),
        "9\n2\nbot-token\n\n/custom/bin/codex\n\ngpt-5.6\n\n",
    );

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let generated = std::fs::read(temp.path().join("config2.json")).unwrap();
    let generated_text = String::from_utf8_lossy(&generated);
    assert!(generated_text.find("\"channels\"") < generated_text.find("\"agents\""));
    let config: NodeConfig = serde_json::from_slice(&generated).unwrap();
    let ChannelConfig::Telegram(channel) = &config.channels[0] else {
        panic!("generated channel should be Telegram");
    };
    assert_eq!(channel.name, "telegram");
    assert_eq!(channel.token, "bot-token");
    assert_generated_agent(
        &config,
        temp.path(),
        "telegram",
        "/custom/bin/codex",
        "gpt-5.6",
        "high",
    );
}

#[test]
fn node_config_generate_builds_a_lark_config_with_detected_codex_default() {
    let temp = tempfile::tempdir().unwrap();
    let output_dir = tempfile::tempdir().unwrap();
    let executable_dir = tempfile::tempdir().unwrap();
    let codex = executable_dir.path().join("codex");
    #[cfg(unix)]
    let executable = {
        let executable = executable_dir.path().join("codex.js");
        std::fs::write(&executable, "stub").unwrap();
        std::os::unix::fs::symlink(&executable, &codex).unwrap();
        executable
    };
    #[cfg(not(unix))]
    let executable = {
        std::fs::write(&codex, "stub").unwrap();
        codex.clone()
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();
    }

    let config_path = output_dir.path().join("lark-config.json");
    let output = run_config_generate(
        temp.path(),
        executable_dir.path(),
        "--generate",
        &config_path,
        "\napp-id\napp-secret\n\n\ngpt-5.6-codex\nxhigh\n",
    );

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(&format!("Codex path [{}]", codex.display())));
    let config: NodeConfig = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    let ChannelConfig::Lark(channel) = &config.channels[0] else {
        panic!("generated channel should be Lark");
    };
    assert_eq!(channel.name, "lark");
    assert_eq!(channel.app_id, "app-id");
    assert_eq!(channel.secret, "app-secret");
    assert_generated_agent(
        &config,
        temp.path(),
        "lark",
        &codex.to_string_lossy(),
        "gpt-5.6-codex",
        "xhigh",
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(config_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn node_config_generate_supports_the_interactive_terminal_flow() {
    use std::os::unix::fs::PermissionsExt as _;

    let workspace = tempfile::tempdir().unwrap();
    let output_dir = tempfile::tempdir().unwrap();
    let executable_dir = tempfile::tempdir().unwrap();
    let codex = executable_dir.path().join("codex");
    std::fs::write(&codex, "#!/bin/sh\n").unwrap();
    let mut permissions = std::fs::metadata(&codex).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&codex, permissions).unwrap();
    let config_path = output_dir.path().join("interactive.json");

    let mut child = std::process::Command::new("/usr/bin/script")
        .arg("-q")
        .arg("/dev/null")
        .arg(env!("CARGO_BIN_EXE_agora-node"))
        .arg("config")
        .arg("-g")
        .arg(&config_path)
        .current_dir(workspace.path())
        .env("PATH", executable_dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        for answer in ["\r", "app-id\r", "secret\r", "\r", "\r", "gpt-5\r", "\r"] {
            input.write_all(answer.as_bytes()).unwrap();
            input.flush().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("interactive config generation did not exit before the deadline");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    writer.join().unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(
        status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.windows(2).any(|window| window == b"\x1b["));
    let config: NodeConfig = serde_json::from_slice(&std::fs::read(config_path).unwrap()).unwrap();
    let ChannelConfig::Lark(channel) = &config.channels[0] else {
        panic!("generated channel should be Lark");
    };
    assert_eq!(channel.app_id, "app-id");
    assert_eq!(channel.secret, "secret");
    assert_generated_agent(
        &config,
        workspace.path(),
        "lark",
        &codex.to_string_lossy(),
        "gpt-5",
        "high",
    );
}

#[test]
fn node_config_generate_overwrites_an_existing_config() {
    let temp = tempfile::tempdir().unwrap();
    let empty_path = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("config.json");
    std::fs::write(&config_path, "original").unwrap();

    let output = run_config_generate(
        temp.path(),
        empty_path.path(),
        "-g",
        &config_path,
        "2\nreplacement-token\n\n/custom/bin/codex\ngpt-5.6\n\n",
    );

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let config: NodeConfig = serde_json::from_slice(&std::fs::read(config_path).unwrap()).unwrap();
    let ChannelConfig::Telegram(channel) = &config.channels[0] else {
        panic!("generated channel should be Telegram");
    };
    assert_eq!(channel.token, "replacement-token");
}

#[test]
fn node_config_generate_reports_the_underlying_write_error() {
    let temp = tempfile::tempdir().unwrap();
    let empty_path = tempfile::tempdir().unwrap();
    let output = run_config_generate(
        temp.path(),
        empty_path.path(),
        "-g",
        temp.path(),
        "2\nbot-token\n\n/custom/bin/codex\ngpt-5.6\n\n",
    );

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("open configuration file"));
    assert!(stdout.contains("os error"));
}

#[test]
fn node_config_generate_requires_an_output_path() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agora-node"))
        .args(["config", "-g"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("<PATH>"));
}

fn run_config_generate(
    current_dir: &std::path::Path,
    path: &std::path::Path,
    flag: &str,
    output_path: &std::path::Path,
    input: &str,
) -> std::process::Output {
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_agora-node"))
        .args(["config", flag])
        .arg(output_path)
        .current_dir(current_dir)
        .env("PATH", path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn assert_generated_agent(
    config: &NodeConfig,
    workspace: &std::path::Path,
    channel: &str,
    path: &str,
    model: &str,
    effort: &str,
) {
    assert_eq!(config.agents.len(), 1);
    let agent = &config.agents[0];
    assert_eq!(agent.name, "agent");
    assert_eq!(agent.isolate, IsolateMode::Session);
    assert_eq!(
        agent.workspace,
        std::fs::canonicalize(workspace).unwrap().to_string_lossy()
    );
    assert_eq!(agent.agent_type, AgentType::Codex);
    assert_eq!(agent.path, path);
    assert_eq!(agent.model.as_deref(), Some(model));
    assert_eq!(agent.effort.as_deref(), Some(effort));
    assert_eq!(agent.subscribe.len(), 1);
    assert_eq!(agent.subscribe[0].channel, channel);
}

#[test]
fn node_subcommand_help_describes_daemon_and_config_options() {
    let daemon = std::process::Command::new(env!("CARGO_BIN_EXE_agora-node"))
        .args(["daemon", "--help"])
        .output()
        .unwrap();
    assert!(daemon.status.success());
    let daemon_help = String::from_utf8_lossy(&daemon.stdout);
    assert!(daemon_help.contains("-c"));
    assert!(daemon_help.contains("--config"));
    assert!(daemon_help.contains("<CONFIG>"));

    let config = std::process::Command::new(env!("CARGO_BIN_EXE_agora-node"))
        .args(["config", "--help"])
        .output()
        .unwrap();
    assert!(config.status.success());
    let config_help = String::from_utf8_lossy(&config.stdout);
    assert!(config_help.contains("-g"));
    assert!(config_help.contains("--generate"));
    assert!(config_help.contains("<PATH>"));
}

#[test]
fn node_help_describes_the_config_fields() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agora-node"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("config"));
    assert!(stdout.contains("daemon"));
    for section in [
        "QUICK START",
        "MINIMAL CONFIGURATION",
        "CHANNEL PERMISSIONS",
        "FIELD REFERENCE",
    ] {
        assert!(stdout.contains(section), "help is missing {section:?}");
    }
    assert!(stdout.contains("agora-node daemon --config config.json"));
    assert!(stdout.contains("  \"channels\": ["));
    assert!(!stdout.contains(r#"{"proxy":"127.0.0.1:7890""#));
    for expected in [
        "CONFIGURATION FILE",
        "Existing files are overwritten",
        "runtime",
        "max_in_flight_tasks",
        "max_in_flight_runs",
        "max_concurrent_runs",
        "channels",
        "app_id",
        "secret",
        "telegram",
        "token",
        "permission.users",
        "permission.users[].id",
        "permission.groups",
        "require_mention",
        "deny all",
        "agents",
        "isolate",
        "workspace",
        "~/.agora/workspace",
        "model",
        "effort",
        "agent_sandbox",
        "timeout_seconds",
        "max_output_bytes",
        "HTTP_PROXY/HTTPS_PROXY",
        "subscribe",
        "filter",
    ] {
        assert!(stdout.contains(expected), "help is missing {expected:?}");
    }
    assert!(!stdout.contains("Agent card fields"));
    assert!(!stdout.contains("\"task\""));
}
