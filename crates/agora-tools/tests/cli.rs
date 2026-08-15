use std::process::Command;

fn agora_tools() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agora-tools"))
}

#[test]
fn help_exposes_trace_viewer() {
    let output = agora_tools().arg("--help").output().unwrap();

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("trace-viewer"));
}

#[test]
fn trace_viewer_requires_config() {
    let output = agora_tools().arg("trace-viewer").output().unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--config <CONFIG>"));
}

#[test]
fn trace_viewer_documents_only_supported_options() {
    let output = agora_tools()
        .args(["trace-viewer", "--help"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--config <CONFIG>"));
    assert!(stdout.contains("--sandbox-bin <SANDBOX_BIN>"));
    assert!(stdout.contains("--no-open"));
    assert!(!stdout.contains("--command"));
    assert!(!stdout.contains("--shell"));
    assert!(!stdout.contains("--listen"));
}

#[test]
fn trace_viewer_rejects_browser_selected_execution_options() {
    let output = agora_tools()
        .args([
            "trace-viewer",
            "--config",
            "sandbox.json",
            "--shell",
            "/bin/zsh",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument '--shell'"));
}
