use super::*;
use std::io::Cursor;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[test]
fn text_prompts_cover_defaults_retries_optional_values_and_eof() {
    let mut output = Vec::new();
    section(&mut output, "Plain", false).unwrap();
    section(&mut output, "Colored", true).unwrap();

    let mut choices = Cursor::new(b"invalid\n3\n2\n".as_slice());
    assert_eq!(
        choose(&mut output, &mut choices, "Pick", &["a", "b"], 0, false).unwrap(),
        1
    );
    assert!(String::from_utf8_lossy(&output).contains("Please enter a number from 1 to 2"));

    let mut default_choice = Cursor::new(b"\n".as_slice());
    assert_eq!(
        choose(
            &mut output,
            &mut default_choice,
            "Default",
            &["a", "b"],
            1,
            false,
        )
        .unwrap(),
        1
    );

    let mut required = Cursor::new(b"\nvalue\n".as_slice());
    assert_eq!(
        prompt(&mut output, &mut required, "Required", None, true, false).unwrap(),
        "value"
    );
    let mut default = Cursor::new(b"\n".as_slice());
    assert_eq!(
        prompt(
            &mut output,
            &mut default,
            "Defaulted",
            Some("fallback"),
            true,
            false,
        )
        .unwrap(),
        "fallback"
    );
    let mut optional = Cursor::new(b"\n".as_slice());
    assert_eq!(
        prompt(&mut output, &mut optional, "Optional", None, false, false).unwrap(),
        ""
    );
    assert!(read_line(&mut Cursor::new(Vec::<u8>::new()), "closed").is_err());
}

#[test]
fn generated_configs_cover_both_channels_and_secure_file_output() {
    let workspace = tempfile::tempdir().unwrap();

    let mut lark_input = Cursor::new(b"\napp-id\nsecret\n\n\nmodel\n\n".as_slice());
    let mut output = Vec::new();
    let lark = collect_config(
        &mut lark_input,
        &mut output,
        workspace.path(),
        Some(PathBuf::from("/usr/local/bin/codex")),
        false,
        false,
    )
    .unwrap();
    let lark_json = serde_json::to_value(&lark).unwrap();
    assert_eq!(lark_json["channels"][0]["type"], "lark");
    assert_eq!(lark_json["agents"][0]["path"], "/usr/local/bin/codex");
    assert_eq!(lark_json["agents"][0]["effort"], "high");

    let mut telegram_input = Cursor::new(b"2\ntoken\n\n/path/to/codex\nmodel\nlow\n".as_slice());
    let telegram = collect_config(
        &mut telegram_input,
        &mut output,
        workspace.path(),
        None,
        false,
        true,
    )
    .unwrap();
    let telegram_json = serde_json::to_value(&telegram).unwrap();
    assert_eq!(telegram_json["channels"][0]["type"], "telegram");
    assert_eq!(telegram_json["channels"][0]["token"], "token");
    assert_eq!(telegram_json["agents"][0]["model"], "model");

    let config_path = workspace.path().join("generated.json");
    write_config(&config_path, &telegram).unwrap();
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    assert_eq!(stored, telegram_json);
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(config_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn executable_detection_rejects_directories_and_non_executable_files() {
    let directory = tempfile::tempdir().unwrap();
    let regular = directory.path().join("regular");
    let executable = directory.path().join("executable");
    std::fs::write(&regular, b"regular").unwrap();
    std::fs::write(&executable, b"executable").unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();

    assert!(!is_executable(directory.path()));
    assert!(!is_executable(&regular));
    assert!(is_executable(&executable));
    assert_eq!(
        executable_names("codex").collect::<Vec<_>>(),
        [OsString::from("codex")]
    );
}
