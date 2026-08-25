use super::{FilesystemKeyMigrationProgress, ProgressDisplay, collect_keys, section};

#[test]
fn key_prompts_are_visible_and_preserve_non_newline_characters() {
    let mut input = &b" old key \nnew key\n"[..];
    let mut output = Vec::new();

    let keys = collect_keys(&mut input, &mut output).unwrap();

    assert_eq!(keys.current, " old key ");
    assert_eq!(keys.new, "new key");
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Current filesystem key"));
    assert!(output.contains("New filesystem key"));
}

#[test]
fn non_interactive_progress_lists_stage_percentages() {
    let mut output = Vec::new();
    let mut display = ProgressDisplay::new(&mut output, false, false);

    display
        .render(FilesystemKeyMigrationProgress::ReencryptingFiles)
        .unwrap();
    display
        .render(FilesystemKeyMigrationProgress::Completed)
        .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("40%"));
    assert!(output.contains("Re-encrypting filesystem files"));
    assert!(output.contains("100%"));
    assert!(output.contains("Migration complete"));
}

#[test]
fn key_prompts_retry_empty_values_and_report_closed_input() {
    let mut input = &b"\nold\n\nnew\n"[..];
    let mut output = Vec::new();

    let keys = collect_keys(&mut input, &mut output).unwrap();

    assert_eq!(keys.current, "old");
    assert_eq!(keys.new, "new");
    assert_eq!(
        String::from_utf8(output)
            .unwrap()
            .matches("A value is required.")
            .count(),
        2
    );

    let error = collect_keys(&mut &b""[..], &mut Vec::new())
        .err()
        .expect("closed input must fail");
    assert!(error.to_string().contains("input closed"));
}

#[test]
fn interactive_progress_uses_color_and_refreshes_one_line() {
    let mut output = Vec::new();
    section(&mut output, "Migration", true).unwrap();
    let mut display = ProgressDisplay::new(&mut output, true, true);

    display
        .render(FilesystemKeyMigrationProgress::ReencryptingFiles)
        .unwrap();
    display
        .render(FilesystemKeyMigrationProgress::Completed)
        .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("\u{1b}[1;36m"));
    assert!(output.contains("\r"));
    assert!(output.contains("40%"));
    assert!(output.contains("100%"));
    assert!(output.ends_with('\n'));
}
