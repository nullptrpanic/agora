#[test]
fn server_is_binary_only_and_prints_hello() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(!manifest_dir.join("src/lib.rs").exists());

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agora-server"))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "hello from agora-server\n"
    );
}
