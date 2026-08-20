use super::*;

#[test]
fn telegram_rich_message_groups_thinking_and_commands_into_ordered_phases() {
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

    assert!(rendered.starts_with("## codex-dev\n\n> **已完成**"));
    assert!(rendered.contains("**01 · 思考过程**\n\n> ✦ Inspecting &lt;the project&gt;"));
    assert!(rendered.contains("**02 · 思考过程**\n\n> ✦ Checking the tests"));
    assert!(rendered.contains(
        "<pre><code class=\"language-bash\"># SHELL · ✓ exit 0\n$ cargo test</code></pre>"
    ));
    assert!(rendered.find("Inspecting") < rendered.find("Checking"));
    assert!(rendered.find("Checking") < rendered.find("# SHELL"));
    let answer = rendered.find("### 最终回答").unwrap();
    let process = rendered.find("<details><summary>").unwrap();
    let usage = rendered.find("*46.0K tokens").unwrap();
    assert!(answer < process);
    assert!(process < usage);
    assert!(rendered.contains("**All checks passed.**"));
    assert!(
        rendered.ends_with(
            "*46.0K tokens · Input 42.8K · 31.6K cached · Output 3.2K · Reasoning 1.9K*"
        )
    );
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
    assert!(
        rendered.starts_with(
            "## codex-dev\n\n> **运行中**\n\n<details open><summary>任务过程 · 2 个阶段 · ● 1 项进行中</summary>"
        )
    );
    assert!(rendered.contains(
        "<pre><code class=\"language-bash\"># SHELL · ● Running\n$ cargo test</code></pre>"
    ));
    assert_eq!(
        content.render(true),
        "<tg-thinking>Checking the tests\n\n● $ cargo test</tg-thinking>"
    );
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

    assert!(rendered.contains("<pre><code class=\"language-bash\"># SHELL · ● Running\n$ "));
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
fn telegram_rich_message_keeps_progress_in_its_original_phase() {
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
    assert!(rendered.find("Plan the checks") < rendered.find("Run tests"));
    assert!(rendered.find("Run tests") < rendered.find("Review the result"));
    assert!(rendered.find("Review the result") < rendered.find("Check formatting"));
}

#[test]
fn telegram_rich_message_renders_latest_thinking_last() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.apply(RunEvent::Output(OutputEvent::Thinking {
        text: "First update".to_string(),
    }));
    content.apply(RunEvent::Output(OutputEvent::Thinking {
        text: "Latest update".to_string(),
    }));

    let rendered = content.render(false);

    assert!(rendered.find("First update") < rendered.find("Latest update"));
}

#[test]
fn telegram_rich_message_marks_running_progress_stopped_when_the_run_stops() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.apply(RunEvent::Output(OutputEvent::Progress {
        id: "command-1".to_string(),
        text: "Run tests".to_string(),
        status: ProgressStatus::Running,
    }));

    content.apply(RunEvent::Stopped);

    assert!(content.render(false).contains("■ Run tests"));
}

#[test]
fn telegram_rich_message_omits_oldest_process_phases_before_latest_output() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    for index in 0..500 {
        content.apply(RunEvent::Output(OutputEvent::Thinking {
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

    let messages = content.render_messages(false);

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
    assert!(
        messages
            .first()
            .unwrap()
            .starts_with("## codex-dev\n\n> **已完成**")
    );
    assert!(
        messages
            .last()
            .unwrap()
            .ends_with("*2.0K tokens · Input 1.5K · 1.0K cached · Output 500 · Reasoning 250*")
    );
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
    assert!(progress.process.front().unwrap().progress.len() <= 64);
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
    assert!(rendered.contains("<tg-thinking>Reviewing &lt;changes&gt;</tg-thinking>"));
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
    assert_eq!(
        content.render(false),
        format!(
            "## codex-dev\n\n> **运行中**\n\n> {}",
            i18n::WAITING_FOR_AGENT
        )
    );

    content.apply(RunEvent::Output(OutputEvent::Thinking {
        text: "Reviewing the change".to_string(),
    }));
    assert!(
        content
            .render(true)
            .contains("<tg-thinking>Reviewing the change</tg-thinking>")
    );
    assert!(!content.render(false).contains("<tg-thinking>"));

    content.apply(RunEvent::Output(OutputEvent::Usage(TokenUsage {
        input_tokens: 800,
        cached_input_tokens: 600,
        output_tokens: 200,
        reasoning_output_tokens: 100,
    })));
    content.apply(RunEvent::Completed { exit_code: 0 });
    assert!(!content.render(true).contains("<tg-thinking>"));
    let rendered = content.render(false);
    assert!(rendered.starts_with(
        "## codex-dev\n\n> **已完成**\n\n<details><summary>任务过程 · 1 个阶段</summary>"
    ));
    assert!(rendered.contains("**01 · 思考过程**\n\n> ✦ Reviewing the change"));
    assert!(
        rendered.ends_with("*1.0K tokens · Input 800 · 600 cached · Output 200 · Reasoning 100*")
    );
}

#[test]
fn telegram_rich_message_renders_queue_stop_and_interruption_states() {
    let mut queued = TelegramRichContent::new("codex-dev".to_string());
    queued.apply(RunEvent::Queued { ahead: 2 });
    assert_eq!(
        queued.render(false),
        "## codex-dev\n\n> **排队中**\n\n> 正在排队，前面还有 2 个任务..."
    );

    let mut stopped = TelegramRichContent::new("codex-dev".to_string());
    stopped.apply(RunEvent::Output(OutputEvent::Answer {
        text: "Partial work".to_string(),
    }));
    stopped.apply(RunEvent::Stopped);
    let stopped = stopped.render(false);
    assert!(stopped.starts_with("## codex-dev\n\n> **已停止**\n\n**任务已停止**"));
    assert!(stopped.contains("### 部分回答\n\nPartial work"));

    let mut interrupted = TelegramRichContent::new("codex-dev".to_string());
    interrupted.apply(RunEvent::Interrupted);
    let interrupted = interrupted.render(false);
    assert!(interrupted.starts_with("## codex-dev\n\n> **已中断**\n\n**任务已中断**"));
    assert!(interrupted.contains("Agora Node 即将退出"));
}

#[test]
fn telegram_rich_message_hides_raw_failure_details() {
    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.apply(RunEvent::Failed {
        message: "secret backend process exited with token=abc".to_string(),
    });

    let rendered = content.render(false);

    assert!(rendered.starts_with("## codex-dev\n\n> **失败**\n\n**任务失败**"));
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
fn telegram_truncated_terminal_output_keeps_usage_and_partial_answer_state() {
    let usage = TokenUsage {
        input_tokens: 1_000_000,
        cached_input_tokens: 999,
        output_tokens: 0,
        reasoning_output_tokens: 0,
    };
    let mut completed = TelegramRichContent::new("codex-dev".to_string());
    completed.apply(RunEvent::Output(OutputEvent::Usage(usage)));
    completed.apply(RunEvent::Completed { exit_code: 0 });
    let truncated = completed.render_truncated(false);
    assert!(truncated.starts_with("## codex-dev\n\n> **已完成**"));
    assert!(truncated.contains("1.0M tokens"));
    assert!(truncated.contains("Input 1.0M"));

    let mut failed = TelegramRichContent::new("codex-dev".to_string());
    failed.apply(RunEvent::Output(OutputEvent::Answer {
        text: "界<&>".repeat(20_000),
    }));
    failed.apply(RunEvent::Failed {
        message: "failed".to_string(),
    });
    let truncated = failed.render_truncated(false);
    assert!(TelegramRichContent::within_limits(&truncated));
    assert!(truncated.starts_with("## codex-dev\n\n> **失败**"));
    assert!(truncated.contains(i18n::PARTIAL_ANSWER_TITLE));
    assert!(truncated.contains("&lt;&amp;&gt;"));
}

#[test]
fn telegram_draft_and_terminal_rendering_cover_all_progress_labels() {
    use super::super::{TelegramProcessPhase, TelegramProgressEntry, TelegramProgressKind};
    use std::collections::VecDeque;

    let mut content = TelegramRichContent::new("codex-dev".to_string());
    content.process.push_front(TelegramProcessPhase {
        thinking: None,
        progress: VecDeque::new(),
    });
    assert!(content.render(true).contains(i18n::WAITING_FOR_AGENT));

    content
        .process
        .front_mut()
        .unwrap()
        .progress
        .push_back(TelegramProgressEntry {
            id: "message".to_string(),
            text: "Checking".to_string(),
            status: ProgressStatus::Running,
            kind: TelegramProgressKind::Message,
            exit_code: None,
        });
    assert!(content.render(true).contains("● Checking"));

    content.process.push_front(TelegramProcessPhase {
        thinking: Some("first line\nsecond line".to_string()),
        progress: VecDeque::from([
            TelegramProgressEntry {
                id: "completed".to_string(),
                text: "true".to_string(),
                status: ProgressStatus::Completed,
                kind: TelegramProgressKind::Command,
                exit_code: None,
            },
            TelegramProgressEntry {
                id: "failed".to_string(),
                text: "false".to_string(),
                status: ProgressStatus::Failed,
                kind: TelegramProgressKind::Command,
                exit_code: None,
            },
            TelegramProgressEntry {
                id: "stopped".to_string(),
                text: "sleep 10".to_string(),
                status: ProgressStatus::Stopped,
                kind: TelegramProgressKind::Command,
                exit_code: None,
            },
        ]),
    });
    let rendered = content.render(false);
    assert!(rendered.contains("> ✦ first line\n> second line"));
    assert!(rendered.contains("✓ Completed"));
    assert!(rendered.contains("× Failed"));
    assert!(rendered.contains("■ Stopped"));
}

#[test]
fn telegram_escape_tail_accounts_for_structural_character_expansion() {
    assert_eq!(TelegramRichContent::format_tokens(1_000_000), "1.0M");
    assert_eq!(
        TelegramRichContent::escape_tail("prefix&<>", 13),
        "&amp;&lt;&gt;"
    );
    assert_eq!(TelegramRichContent::escape_tail("prefix&<>", 4), "&gt;");
}
