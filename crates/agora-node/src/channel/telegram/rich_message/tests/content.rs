use super::*;

#[test]
fn telegram_formats_web_actions_and_only_hides_typed_diagnostics() {
    let mut content = TelegramRichContent::new("agent".into());
    content.apply(RunEvent::Output(OutputEvent::ToolCall {
        id: "search".into(),
        text: "Search ``".into(),
        status: ProgressStatus::Running,
        web_action: Some(crate::task::WebAction::Search { queries: vec![] }),
    }));
    assert!(
        content
            .render(false)
            .contains("<summary>搜索资料</summary>")
    );
    content.apply(RunEvent::Output(OutputEvent::Diagnostic {
        id: "diag".into(),
        text: "warning text".into(),
    }));
    assert!(!content.render(false).contains("warning text"));
    content.apply(RunEvent::Output(OutputEvent::Progress {
        id: "error".into(),
        text: "warning text".into(),
        status: ProgressStatus::Failed,
    }));
    assert!(content.render(false).contains("× warning text"));
}

#[test]
fn telegram_preserves_commentary_and_tool_updates_as_progress_text() {
    let mut content = TelegramRichContent::new("agent".into());
    content.apply(RunEvent::Output(OutputEvent::Commentary {
        id: "comment".into(),
        text: "Checking the source".into(),
    }));
    for status in [ProgressStatus::Running, ProgressStatus::Completed] {
        content.apply(RunEvent::Output(OutputEvent::ToolCall {
            web_action: None,
            id: "tool".into(),
            text: "Search documentation".into(),
            status,
        }));
    }
    content.apply(RunEvent::Completed { exit_code: 0 });
    let rendered = content.render(false);
    assert!(rendered.contains("Checking the source"));
    assert_eq!(rendered.matches("Search documentation").count(), 1);
    assert!(rendered.contains("已完成"));
}

#[test]
fn split_budget_includes_prefix_and_wrapper() {
    for prefix in [
        "x".repeat(32_767),
        "line\n".repeat(399),
        "<b>x</b>\n".repeat(133),
    ] {
        for (text, escaped) in [
            ("y", "y"),
            ("&", "&amp;"),
            ("<", "&lt;"),
            ("你", "你"),
            ("\n", "\n"),
        ] {
            let parts =
                TelegramRichContent::split_sections(vec![prefix.clone(), text.repeat(32_769)]);

            assert!(
                parts[0] == prefix,
                "the full prefix must be sent on its own"
            );
            assert!(parts.iter().all(|part| part.chars().count() <= 32_768));
            assert!(
                parts
                    .iter()
                    .all(|part| part.lines().count() + part.matches('<').count() <= 400)
            );
            let content = parts[1..]
                .iter()
                .map(|part| {
                    part.strip_prefix("<pre>")
                        .unwrap()
                        .strip_suffix("</pre>")
                        .unwrap()
                })
                .collect::<String>();
            assert_eq!(content, escaped.repeat(32_769));
        }
    }
}

#[test]
fn split_budget_reserves_the_first_escaped_character() {
    let prefix = "x".repeat(32_754);
    let parts = TelegramRichContent::split_sections(vec![prefix.clone(), "&".repeat(32_769)]);

    assert!(
        parts[0] == prefix,
        "the escaped character does not fit beside the prefix"
    );
    assert!(parts.iter().all(|part| part.chars().count() <= 32_768));
    let content = parts[1..]
        .iter()
        .map(|part| {
            part.strip_prefix("<pre>")
                .unwrap()
                .strip_suffix("</pre>")
                .unwrap()
        })
        .collect::<String>();
    assert_eq!(content, "&amp;".repeat(32_769));
}

#[test]
fn telegram_empty_completion_has_one_native_disclosure_and_identifies_the_agent() {
    let mut content = TelegramRichContent::new("codex-dev".into());
    content.apply(RunEvent::Completed { exit_code: 0 });
    assert_eq!(
        content.render(false),
        "<details><summary>已完成</summary>\n\ncodex-dev\n\n</details>"
    );
}

#[test]
fn telegram_terminal_answer_is_visible_above_the_closed_process() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.apply(RunEvent::Output(OutputEvent::Answer {
        text: "**All checks passed.**".to_string(),
    }));
    content.apply(RunEvent::Completed { exit_code: 0 });

    assert_eq!(
        content.render(false),
        "**All checks passed.**\n\n<details><summary>已完成</summary>\n\ncodex-dev\n\n</details>"
    );
}

#[test]
fn telegram_answer_with_details_markup_uses_the_safe_unwrapped_fallback() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.apply(RunEvent::Output(OutputEvent::Answer {
        text: "Treat </DETAILS> as literal text.".to_string(),
    }));
    content.apply(RunEvent::Completed { exit_code: 0 });

    assert_eq!(
        content.render(false),
        "Treat </DETAILS> as literal text.\n\n<details><summary>已完成</summary>\n\ncodex-dev\n\n</details>"
    );
}

#[test]
fn telegram_terminal_usage_does_not_add_a_footer() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.apply(RunEvent::Output(OutputEvent::Usage(TokenUsage {
        input_tokens: 42_800,
        cached_input_tokens: 31_600,
        output_tokens: 3_200,
        reasoning_output_tokens: 1_900,
    })));
    content.apply(RunEvent::Completed { exit_code: 0 });

    assert_eq!(
        content.render(false),
        "<details><summary>已完成</summary>\n\ncodex-dev\n\n</details>"
    );
}

#[test]
fn telegram_rich_message_shows_shell_below_answer_and_skips_summary_and_usage() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.apply(RunEvent::Output(OutputEvent::Thinking {
        text: "Inspecting <the project>".to_string(),
    }));
    content.apply(RunEvent::Output(OutputEvent::Thinking {
        text: "Checking the tests".to_string(),
    }));
    content.apply(RunEvent::Output(OutputEvent::CommandExecution {
        id: "command-1".to_string(),
        command: "cargo test".to_string(),
        status: ProgressStatus::Completed,
        exit_code: Some(0),
    }));
    content.apply(RunEvent::Output(OutputEvent::Answer {
        text: "**All checks passed.**\n\n- tests\n- clippy".to_string(),
    }));
    content.apply(RunEvent::Output(OutputEvent::Usage(TokenUsage {
        input_tokens: 42_800,
        cached_input_tokens: 31_600,
        output_tokens: 3_200,
        reasoning_output_tokens: 1_900,
    })));
    content.apply(RunEvent::Completed { exit_code: 0 });

    let rendered = content.render(false);

    assert!(rendered.starts_with(
        "**All checks passed.**\n\n- tests\n- clippy\n\n<details><summary>已完成</summary>"
    ));
    assert!(rendered.contains("<details><summary>运行 cargo test</summary>"));
    assert!(
        rendered
            .contains("<pre><code class=\"language-bash\">$ cargo test\n\n✓ Success</code></pre>")
    );
    assert!(!rendered.contains("Inspecting"));
    assert!(!rendered.contains("Checking the tests"));
    assert!(rendered.ends_with("<footer>codex-dev</footer>\n\n</details>"));
    assert!(!rendered.contains("\n\ncodex-dev\n") && !rendered.contains("42.8K"));
}

#[test]
fn telegram_rich_message_expands_the_process_while_running() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.apply(RunEvent::Output(OutputEvent::Thinking {
        text: "Inspecting the project".to_string(),
    }));
    content.apply(RunEvent::Output(OutputEvent::Thinking {
        text: "Checking the tests".to_string(),
    }));
    content.apply(RunEvent::Output(OutputEvent::CommandExecution {
        id: "command-1".to_string(),
        command: "cargo test".to_string(),
        status: ProgressStatus::Running,
        exit_code: None,
    }));

    let rendered = content.render(false);
    assert!(rendered.starts_with("<details open><summary>运行 cargo test</summary>"));
    assert!(
        rendered
            .contains("<pre><code class=\"language-bash\">$ cargo test\n\n● Running</code></pre>")
    );
    assert_eq!(
        content.render(true),
        "<tg-thinking>运行 cargo test</tg-thinking>"
    );

    content.apply(RunEvent::Output(OutputEvent::CommandExecution {
        id: "command-1".into(),
        command: "cargo test".into(),
        status: ProgressStatus::Completed,
        exit_code: Some(0),
    }));
    assert!(
        content
            .render(false)
            .starts_with("<details open><summary>运行中</summary>")
    );
    assert_eq!(content.render(true), "<tg-thinking>运行中</tg-thinking>");
}

#[test]
fn telegram_shell_action_titles_only_name_literal_targets() {
    for (command, title) in [
        ("/bin/zsh -lc 'git diff --stat'", "运行 git diff"),
        (
            "/usr/bin/curl -fsSL 'https://user:password@example.org/path?token=abc'",
            "请求 example.org",
        ),
        ("curl --proxy https://proxy.example.org", "运行 curl"),
        ("curl https://one.example https://two.example", "运行 curl"),
        ("curl -H 'Location: https://hidden.example'", "运行 curl"),
        ("curl https://$HOST/path", "运行 curl"),
        ("curl https://one.example && echo done", "执行命令"),
        ("curl\nhttps://two.example", "执行命令"),
        ("curl 'https://unclosed.example", "运行 curl"),
        ("python3 -c 'print(42)'", "运行 python3"),
    ] {
        let mut content = TelegramRichContent::new("agent".into());
        content.apply(RunEvent::Output(OutputEvent::CommandExecution {
            id: "cmd".into(),
            command: command.into(),
            status: ProgressStatus::Running,
            exit_code: None,
        }));
        assert_eq!(
            content.render(true),
            format!("<tg-thinking>{title}</tg-thinking>"),
            "{command}"
        );
    }
}

#[test]
fn telegram_empty_process_uses_a_disclosure_without_an_agent_placeholder() {
    let mut content = TelegramRichContent::new("agent".into());
    content.apply(RunEvent::Completed { exit_code: 0 });
    assert_eq!(
        content.render(false),
        "<details><summary>已完成</summary>\n\n没有可展示的运行步骤。\n\n</details>"
    );
}

#[test]
fn telegram_running_answer_precedes_and_collapses_the_process() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.apply(RunEvent::Output(OutputEvent::Thinking {
        text: "Inspecting the project".to_string(),
    }));
    content.apply(RunEvent::Output(OutputEvent::Answer {
        text: "The answer has started.".to_string(),
    }));

    let rendered = content.render(false);
    let answer = rendered.find("The answer has started.").unwrap();
    let process = rendered.find("<details><summary>运行中").unwrap();

    assert!(answer < process);
    assert!(!rendered.contains("<details open><summary>运行中"));
}

#[test]
fn telegram_rich_message_preserves_the_complete_agent_command() {
    let command = format!(
        "/bin/bash -lc \"pwd && {} && echo end-of-command\"",
        "rg --files ".repeat(40)
    );
    assert!(command.chars().count() > 400);
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.apply(RunEvent::Output(OutputEvent::CommandExecution {
        id: "command-1".to_string(),
        command: command.clone(),
        status: ProgressStatus::Running,
        exit_code: None,
    }));

    let rendered = content.render(false);
    let escaped_command = TelegramRichContent::escape_structural_text(&command);

    assert!(rendered.contains("<pre><code class=\"language-bash\">$ "));
    assert!(rendered.contains(&escaped_command));
    assert!(rendered.contains("end-of-command"));
    assert!(!rendered.contains("..."));
}

#[test]
fn telegram_rich_message_updates_the_latest_progress_marker() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    for (status, marker) in [
        (ProgressStatus::Completed, "✓"),
        (ProgressStatus::Failed, "×"),
        (ProgressStatus::Stopped, "■"),
    ] {
        content.apply(RunEvent::Output(OutputEvent::Progress {
            id: "command-1".to_string(),
            text: "Run tests".to_string(),
            status,
        }));
        assert!(
            content
                .render(false)
                .contains(&format!("{marker} Run tests"))
        );
    }
}

#[test]
fn telegram_rich_message_updates_progress_in_its_original_position() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.apply(RunEvent::Output(OutputEvent::Thinking {
        text: "Plan the checks".to_string(),
    }));
    content.apply(RunEvent::Output(OutputEvent::Progress {
        id: "command-1".to_string(),
        text: "Run tests".to_string(),
        status: ProgressStatus::Running,
    }));
    content.apply(RunEvent::Output(OutputEvent::Thinking {
        text: "Review the result".to_string(),
    }));
    content.apply(RunEvent::Output(OutputEvent::Progress {
        id: "command-2".to_string(),
        text: "Check formatting".to_string(),
        status: ProgressStatus::Completed,
    }));
    content.apply(RunEvent::Output(OutputEvent::Progress {
        id: "command-1".to_string(),
        text: "Run tests".to_string(),
        status: ProgressStatus::Failed,
    }));
    content.apply(RunEvent::Completed { exit_code: 0 });

    let rendered = content.render(false);

    assert_eq!(rendered.matches("Run tests").count(), 1);
    assert_eq!(rendered.matches("Check formatting").count(), 1);
    assert!(rendered.contains("× Run tests"));
    assert!(rendered.contains("✓ Check formatting"));
    assert!(!rendered.contains("Plan the checks"));
    assert!(!rendered.contains("Review the result"));
    assert!(rendered.find("Run tests").unwrap() < rendered.find("Check formatting").unwrap());
}

#[test]
fn telegram_changed_commentary_snapshots_keep_order_and_exact_duplicates_collapse() {
    let mut content = TelegramRichContent::new("agent".into());
    for text in ["First step", "Second step", "First step"] {
        content.apply(RunEvent::Output(OutputEvent::Commentary {
            id: "step".into(),
            text: text.into(),
        }));
    }
    let rendered = content.render(false);
    assert_eq!(rendered.matches("First step").count(), 1);
    assert!(rendered.find("First step").unwrap() < rendered.find("Second step").unwrap());
}

#[test]
fn telegram_rich_message_closes_running_progress_on_system_interruption() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.apply(RunEvent::Output(OutputEvent::Progress {
        id: "command-1".to_string(),
        text: "Run tests".to_string(),
        status: ProgressStatus::Running,
    }));

    content.apply(RunEvent::Interrupted);

    assert!(content.render(false).contains("■ Run tests"));
}

#[test]
fn telegram_rich_message_omits_oldest_process_entries_before_latest_output() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    for index in 0..500 {
        content.apply(RunEvent::Output(OutputEvent::Commentary {
            id: format!("comment-{index}"),
            text: format!("phase-{index:03} {}", "detail ".repeat(20)),
        }));
    }
    content.apply(RunEvent::Output(OutputEvent::Answer {
        text: "Final answer remains visible".to_string(),
    }));
    content.apply(RunEvent::Completed { exit_code: 0 });

    let rendered = content.render(false);

    assert!(TelegramRichContent::within_limits(&rendered));
    assert!(rendered.contains("已省略"));
    assert!(!rendered.contains("phase-000"));
    assert!(rendered.contains("phase-499"));
    assert!(rendered.contains("Final answer remains visible"));
    assert!(content.process.len() <= 64);
}

#[test]
fn telegram_rich_message_splits_oversized_content_without_losing_output() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.apply(RunEvent::Output(OutputEvent::Answer {
        text: "</pre><h1>& oversized answer\n".repeat(2_000),
    }));
    content.apply(RunEvent::Output(OutputEvent::Usage(TokenUsage {
        input_tokens: 1_500,
        cached_input_tokens: 1_000,
        output_tokens: 500,
        reasoning_output_tokens: 250,
    })));
    content.apply(RunEvent::Completed { exit_code: 0 });

    let messages = content.render_messages(false, None);

    assert!(messages.len() > 2);
    assert!(messages.iter().all(|message| {
        message.chars().count() <= 32_768
            && message
                .lines()
                .count()
                .saturating_add(message.matches('<').count())
                <= 400
    }));
    let rendered = messages.join("\n");
    assert!(!rendered.contains(i18n::OUTPUT_TRUNCATED.trim()));
    assert_eq!(
        rendered
            .matches("&lt;/pre&gt;&lt;h1&gt;&amp; oversized answer")
            .count(),
        2_000
    );
    assert!(messages.first().unwrap().starts_with("<pre>"));
    assert!(
        messages
            .last()
            .unwrap()
            .contains("<summary>已完成</summary>")
    );
    assert!(!rendered.contains("<footer>"));
}

#[test]
fn telegram_rich_message_bounds_retained_answer_on_a_utf8_boundary() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.apply(RunEvent::Output(OutputEvent::Answer {
        text: format!("prefix{}tail", "界".repeat(100_000)),
    }));

    assert!(content.answer.starts_with(i18n::OUTPUT_TRUNCATED));
    assert!(content.answer.ends_with("tail"));
    assert!(content.answer.len() <= 256 * 1024);

    let mut progress = TelegramRichContent::new("codex-dev".to_string());
    for index in 0..100 {
        progress.apply(RunEvent::Output(OutputEvent::Progress {
            id: index.to_string(),
            text: format!("progress-{index}"),
            status: ProgressStatus::Running,
        }));
    }
    assert!(progress.process.len() <= 64);
}

#[test]
fn telegram_rich_message_uses_the_same_safe_fallback_while_running() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.apply(RunEvent::Output(OutputEvent::Thinking {
        text: "Reviewing <changes>".to_string(),
    }));
    content.apply(RunEvent::Output(OutputEvent::Answer {
        text: "streaming answer\n".repeat(3_000),
    }));

    let rendered = content.render(true);

    assert!(rendered.contains(i18n::OUTPUT_TRUNCATED.trim()));
    assert!(rendered.contains("<tg-thinking>正在等待 Agent 输出...</tg-thinking>"));
    assert!(!rendered.contains("Reviewing"));
    assert_eq!(rendered.matches("<pre>").count(), 1);
    assert_eq!(rendered.matches("</pre>").count(), 1);
}

#[test]
fn telegram_rich_message_uses_native_thinking_only_for_active_drafts() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());

    assert_eq!(
        content.render(true),
        format!("<tg-thinking>{}</tg-thinking>", i18n::WAITING_FOR_AGENT)
    );
    assert!(
        content
            .render(false)
            .starts_with("<details open><summary>运行中</summary>")
    );
    content.apply(RunEvent::Output(OutputEvent::Thinking {
        text: "Reviewing the change".into(),
    }));
    assert!(!content.render(true).contains("Reviewing the change"));
    content.apply(RunEvent::Output(OutputEvent::Commentary {
        id: "public".into(),
        text: "Inspecting <files>".into(),
    }));
    assert_eq!(
        content.render(true),
        "<tg-thinking>Inspecting &lt;files&gt;</tg-thinking>"
    );
    assert!(!content.render(false).contains("<tg-thinking>"));
    content.apply(RunEvent::Completed { exit_code: 0 });
    assert!(!content.render(true).contains("<tg-thinking>"));
    assert!(content.render(true).contains("Inspecting &lt;files&gt;"));
}

#[test]
fn telegram_rich_message_renders_queue_stop_and_interruption_states() {
    let mut queued = TelegramRichContent::new("codex-dev".to_string());
    queued.apply(RunEvent::Queued { ahead: 2 });
    assert_eq!(
        queued.render(false),
        "<details open><summary>排队中</summary>\n\ncodex-dev：正在排队，前面还有 2 个任务...\n\n</details>"
    );

    let mut stopped = TelegramRichContent::new("codex-dev".to_string());
    stopped.apply(RunEvent::Output(OutputEvent::Answer {
        text: "Partial work".to_string(),
    }));
    stopped.apply(RunEvent::Stopped);
    let stopped = stopped.render(false);
    assert_eq!(stopped, "已停止");

    let mut interrupted = TelegramRichContent::new("codex-dev".to_string());
    interrupted.apply(RunEvent::Interrupted);
    let interrupted = interrupted.render(false);
    assert!(interrupted.starts_with("**任务已中断**"));
    assert!(interrupted.contains("Agora Node 即将退出"));
}

#[test]
fn telegram_rich_message_hides_raw_failure_details() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.apply(RunEvent::Failed {
        message: "secret backend process exited with token=abc".to_string(),
    });

    let rendered = content.render(false);

    assert!(rendered.starts_with("**任务失败**"));
    assert!(rendered.contains("Agent 进程在完成任务前退出。"));
    assert!(!rendered.contains("token=abc"));
}

#[test]
fn telegram_rich_message_ignores_output_after_a_terminal_event() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.apply(RunEvent::Completed { exit_code: 0 });
    let completed = content.render(false);

    content.apply(RunEvent::Output(OutputEvent::Answer {
        text: "must be ignored".to_string(),
    }));

    assert_eq!(content.render(false), completed);
    assert!(!content.render(false).contains("must be ignored"));
}

#[test]
fn telegram_section_splitting_handles_oversized_and_combined_sections() {
    let oversized = "line<&>\n".repeat(1_000);
    let messages = TelegramRichContent::split_sections(vec![
        "first".to_string(),
        oversized,
        "second".to_string(),
        "third".to_string(),
    ]);

    assert!(messages.len() >= 3);
    assert!(
        messages
            .first()
            .is_some_and(|message| message.starts_with("first\n\n<pre>"))
    );
    assert!(
        messages
            .iter()
            .all(|message| TelegramRichContent::within_limits(message))
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("&lt;&amp;&gt;"))
    );
    assert!(messages.last().unwrap().contains("second\n\nthird"));

    let individually_valid =
        TelegramRichContent::split_sections(vec!["a".repeat(20_000), "b".repeat(20_000)]);
    assert_eq!(individually_valid.len(), 2);
    assert!(
        individually_valid
            .iter()
            .all(|message| TelegramRichContent::within_limits(message))
    );
}

#[test]
fn telegram_truncated_failed_answer_keeps_error_and_escapes_its_partial_output() {
    let mut failed = TelegramRichContent::new("codex-dev".to_string());
    failed.apply(RunEvent::Output(OutputEvent::Answer {
        text: "界<&>".repeat(20_000),
    }));
    failed.apply(RunEvent::Failed {
        message: "failed".to_string(),
    });
    let truncated = failed.render_truncated(false);
    assert!(TelegramRichContent::within_limits(&truncated));
    assert!(truncated.starts_with("**任务失败**"));
    assert!(truncated.contains(i18n::PARTIAL_ANSWER_TITLE));
    assert!(truncated.contains("&lt;&amp;&gt;"));
}

#[test]
fn telegram_stopping_a_long_stream_replaces_everything_with_one_status() {
    let mut content = TelegramRichContent::new("agent".into());
    content.apply(RunEvent::Output(OutputEvent::Answer {
        text: "line\n".repeat(10000),
    }));
    content.apply(RunEvent::Stopped);
    assert_eq!(content.render_messages(false, None), ["已停止"]);
    assert_eq!(content.render_truncated(true), "已停止");
}

#[test]
fn telegram_commands_distinguish_failures_stops_and_missing_results() {
    let mut content = TelegramRichContent::new("agent".into());
    for (id, status, exit_code) in [
        ("ok", ProgressStatus::Completed, None),
        ("bad", ProgressStatus::Failed, Some(9)),
        ("stopped", ProgressStatus::Stopped, None),
        ("unknown", ProgressStatus::Running, None),
    ] {
        content.apply(RunEvent::Output(OutputEvent::CommandExecution {
            id: id.into(),
            command: id.into(),
            status,
            exit_code,
        }));
    }
    content.apply(RunEvent::Completed { exit_code: 0 });
    let rendered = content.render(false);
    for label in [
        "$ ok\n\n✓ Success",
        "$ bad\n\n✗ Failed · exit 9",
        "$ stopped\n\n■ Stopped",
        "$ unknown\n\n未确认完成",
    ] {
        assert!(rendered.contains(label), "{rendered}");
    }
}

#[test]
fn telegram_escape_tail_accounts_for_structural_character_expansion() {
    assert_eq!(
        TelegramRichContent::escape_tail("prefix&<>", 13, 5),
        "&amp;&lt;&gt;"
    );
    assert_eq!(TelegramRichContent::escape_tail("prefix&<>", 4, 5), "&gt;");
}
#[test]
fn cot_tool_rows_keep_their_first_position_and_show_the_latest_result() {
    use crate::task::WebAction;
    let mut content = TelegramRichContent::new("agent".into());
    for event in [
        OutputEvent::Commentary {
            id: "before".into(),
            text: "先核对现场".into(),
        },
        OutputEvent::ToolCall {
            id: "search".into(),
            text: "Search ``".into(),
            status: ProgressStatus::Running,
            web_action: Some(WebAction::Search { queries: vec![] }),
        },
        OutputEvent::CommandExecution {
            id: "cmd".into(),
            command: "cargo test".into(),
            status: ProgressStatus::Running,
            exit_code: None,
        },
        OutputEvent::Commentary {
            id: "after".into(),
            text: "再核对结论".into(),
        },
        OutputEvent::ToolCall {
            id: "search".into(),
            text: "Search ``".into(),
            status: ProgressStatus::Completed,
            web_action: Some(WebAction::Search {
                queries: vec!["北京天气".into()],
            }),
        },
        OutputEvent::CommandExecution {
            id: "cmd".into(),
            command: "cargo test".into(),
            status: ProgressStatus::Completed,
            exit_code: Some(0),
        },
        OutputEvent::ToolOutput {
            id: "cmd".into(),
            text: "passed </details> &".into(),
            elapsed_ms: Some(1250),
        },
        OutputEvent::Diagnostic {
            id: "diag".into(),
            text: "ignored backend setting".into(),
        },
        OutputEvent::Usage(TokenUsage {
            input_tokens: 4242,
            cached_input_tokens: 123,
            output_tokens: 84,
            reasoning_output_tokens: 7,
        }),
    ] {
        content.apply(RunEvent::Output(event));
    }
    content.apply(RunEvent::Completed { exit_code: 0 });
    let view = content.render_messages(false, None).join("\n");
    let mut position = 0;
    for part in [
        "先核对现场",
        "已调用 2 次工具",
        "搜索 · 北京天气",
        "完成",
        "$ cargo test",
        "passed &lt;/details&gt; &amp;",
        "✓ Success",
        "再核对结论",
    ] {
        position += view[position..]
            .find(part)
            .unwrap_or_else(|| panic!("missing or out of order: {part} in {view}"))
            + part.len();
    }
    assert_eq!(view.matches("北京天气").count(), 1);
    assert!(view.contains("<details><summary>已调用 2 次工具</summary>"));
    assert!(!view.contains("Search ``"));
    assert!(!view.contains("ignored backend setting"));
    assert!(!view.contains("<footer>"));
    assert!(!view.contains("4242"));
}

#[test]
fn cot_user_stop_only_shows_the_stopped_status() {
    let mut content = TelegramRichContent::new("agent".into());
    content.apply(RunEvent::Output(OutputEvent::Answer {
        text: "partial answer".into(),
    }));
    content.apply(RunEvent::Stopped);
    assert_eq!(content.render_messages(false, None), ["已停止"]);
}

#[test]
fn cot_hides_summary_in_persistent_updates_and_drafts_and_keeps_answer_first() {
    let mut content = TelegramRichContent::new("agent".into());
    content.apply(RunEvent::Output(OutputEvent::Thinking {
        text: "Checking URL formatting".into(),
    }));
    for draft in [true, false] {
        assert!(!content.render(draft).contains("Checking URL formatting"));
    }
    content.apply(RunEvent::Output(OutputEvent::Commentary {
        id: "step".into(),
        text: "已核对 URL".into(),
    }));
    content.apply(RunEvent::Output(OutputEvent::Answer {
        text: "地址可以访问。".into(),
    }));
    content.apply(RunEvent::Completed { exit_code: 0 });
    let messages = content.render_messages(false, None);
    assert_eq!(messages.len(), 1);
    assert!(messages[0].starts_with("地址可以访问。\n\n<details><summary>已完成</summary>"));
    assert!(messages[0].contains("已核对 URL"));
    assert!(!messages[0].contains("Checking URL formatting"));
}

#[test]
fn command_output_preview_keeps_the_input_and_last_500_characters() {
    let mut content = TelegramRichContent::new("agent".into());
    let command = format!("printf '%s' '{}结束'", "input".repeat(1900));
    content.apply(RunEvent::Output(OutputEvent::CommandExecution {
        id: "cmd".into(),
        command: command.clone(),
        status: ProgressStatus::Completed,
        exit_code: Some(0),
    }));
    let tail = format!("{}<&", "界🦀".repeat(249));
    for (output, expected) in [
        (tail.clone(), tail.clone()),
        (
            format!("删{tail}"),
            format!("{}{tail}", i18n::OUTPUT_TRUNCATED),
        ),
    ] {
        content.apply(RunEvent::Output(OutputEvent::ToolOutput {
            id: "cmd".into(),
            text: output,
            elapsed_ms: None,
        }));
        let view = content.render_messages(false, None).join("\n");
        let expected = TelegramRichContent::escape_structural_text(&expected);
        assert!(view.contains(&format!("$ {command}\n\n{expected}\n\n✓ Success")));
        assert!(!view.contains(i18n::PROCESS_TRUNCATED));
    }
}

#[test]
fn oversized_command_keeps_its_prefix_and_leaves_room_for_output_and_status() {
    for padding in ["&".repeat(4500), "\n".repeat(300)] {
        let mut content = TelegramRichContent::new("agent".into());
        content.apply(RunEvent::Output(OutputEvent::CommandExecution {
            id: "shell".into(),
            command: format!("echo start{padding}end"),
            status: ProgressStatus::Failed,
            exit_code: Some(1),
        }));
        content.apply(RunEvent::Output(OutputEvent::ToolOutput {
            id: "shell".into(),
            text: "error".into(),
            elapsed_ms: None,
        }));
        let view = content.render(false);
        assert!(TelegramRichContent::within_limits(&view));
        assert!(view.contains("$ echo start"));
        assert!(view.contains("… [命令已截断]\n\nerror\n\n✗ Failed · exit 1"));
    }
}

#[test]
fn cot_oversized_stream_still_has_a_working_disclosure_and_escaped_shell_tail() {
    let mut content = TelegramRichContent::new("agent".into());
    content.apply(RunEvent::Output(OutputEvent::CommandExecution {
        id: "shell".into(),
        command: "check-logs".into(),
        status: ProgressStatus::Completed,
        exit_code: Some(0),
    }));
    content.apply(RunEvent::Output(OutputEvent::ToolOutput {
        id: "shell".into(),
        text: format!("{}last <&>", "old line\n".repeat(3_000)),
        elapsed_ms: None,
    }));
    content.apply(RunEvent::Output(OutputEvent::Answer {
        text: format!("{}answer tail", "答\n".repeat(10_000)),
    }));
    let messages = content.render_messages(false, None);
    assert_eq!(messages.len(), 1);
    let message = &messages[0];
    assert!(TelegramRichContent::within_limits(message));
    assert!(
        message.find("answer tail").unwrap()
            < message.find("<details><summary>运行中</summary>").unwrap()
    );
    assert!(message.contains("last &lt;&amp;&gt;"));
    assert!(message.contains("✓ Success"));
}

#[test]
fn cot_replayed_tool_start_does_not_reopen_a_finished_command_or_erase_its_duration() {
    let mut content = TelegramRichContent::new("agent".into());
    for (status, exit_code) in [
        (ProgressStatus::Running, None),
        (ProgressStatus::Failed, Some(3)),
    ] {
        content.apply(RunEvent::Output(OutputEvent::CommandExecution {
            id: "shell".into(),
            command: "check".into(),
            status,
            exit_code,
        }));
    }
    for elapsed_ms in [Some(1750), None] {
        content.apply(RunEvent::Output(OutputEvent::ToolOutput {
            id: "shell".into(),
            text: "bad input".into(),
            elapsed_ms,
        }));
    }
    content.apply(RunEvent::Output(OutputEvent::CommandExecution {
        id: "shell".into(),
        command: "check".into(),
        status: ProgressStatus::Running,
        exit_code: None,
    }));
    content.apply(RunEvent::Completed { exit_code: 0 });
    let rendered = content.render(false);
    assert_eq!(
        rendered
            .matches("<details><summary>运行 check</summary>")
            .count(),
        1
    );
    assert!(rendered.contains("bad input\n\n✗ Failed · exit 3 · 约 1.750 秒"));
}

#[test]
fn cot_complete_answer_keeps_one_message_by_trimming_old_process_entries() {
    let mut content = TelegramRichContent::new("agent".into());
    let answer = "答".repeat(31_000);
    content.apply(RunEvent::Output(OutputEvent::Commentary {
        id: "old".into(),
        text: "old".repeat(1200),
    }));
    content.apply(RunEvent::Output(OutputEvent::Commentary {
        id: "new".into(),
        text: "收到最新结果".into(),
    }));
    content.apply(RunEvent::Output(OutputEvent::Answer {
        text: answer.clone(),
    }));
    content.apply(RunEvent::Completed { exit_code: 0 });
    let parts = content.render_messages(false, None);
    assert_eq!(parts.len(), 1);
    assert!(parts[0].starts_with(&answer));
    assert!(parts[0].contains("部分运行过程已省略"));
    assert!(parts[0].contains("收到最新结果"));
    assert!(TelegramRichContent::within_limits(&parts[0]));
}

#[test]
fn clipping_multibyte_tool_text_reserves_space_for_the_omission_marker() {
    let text = format!("{}{}z", "\n".repeat(65), "界".repeat(2708));
    assert_eq!(text.len(), 8190);
    let (retained, clipped) = TelegramRichContent::retain_process_text(text);
    assert!(clipped && retained.starts_with(i18n::OUTPUT_TRUNCATED));
    assert!(retained.ends_with("界z"));
    assert!(retained.len() <= 8192);
    assert!(retained.matches('\n').count() <= 64);
}
