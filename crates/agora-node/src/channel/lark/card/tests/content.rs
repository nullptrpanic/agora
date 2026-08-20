use super::super::MAX_ANSWER_BYTES;
use super::*;
use crate::i18n;

#[test]
fn lark_permission_denial_card_owns_its_markdown_layout() {
    let denial = PermissionDenial::new(
        "lark1",
        "ou_user_1",
        Some("oc_group_1"),
        "当前用户未在允许列表中。",
    );

    let card = LarkReplyCard::permission_denied(&denial);
    let markdown = card
        .pointer("/body/elements/0/content")
        .and_then(serde_json::Value::as_str)
        .unwrap();

    assert_eq!(card["schema"], "2.0");
    assert_eq!(card["header"]["title"]["content"], "访问受限");
    assert!(markdown.starts_with("**无权访问此 Channel**"));
    assert!(markdown.contains("> 当前用户未在允许列表中。"));
    assert!(markdown.contains("- Channel：`lark1`"));
    assert!(markdown.contains("- Group ID：`oc_group_1`"));
    assert!(markdown.contains("```jsonc"));
    assert!(markdown.contains(r#""channels": ["#));
    assert!(markdown.contains("// ..."));
    assert!(!markdown.contains(r#""name": "lark1""#));
}

#[test]
fn lark_card_uses_json_v2_for_standard_markdown() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    content.apply_output(OutputEvent::Thinking {
        text: "Inspecting the channel".to_string(),
    });

    let card = content.build_card();

    assert_eq!(
        card.pointer("/schema").and_then(|v| v.as_str()),
        Some("2.0")
    );
    assert!(card.get("elements").is_none());
    assert!(card.pointer("/config/wide_screen_mode").is_none());
    assert_eq!(
        card.pointer("/body/elements/0/tag").unwrap(),
        "collapsible_panel"
    );
    assert_eq!(
        card.pointer("/body/elements/0/elements/0/content")
            .and_then(|v| v.as_str()),
        Some(
            "<font color='blue'>**01**</font>  **思考过程**\n<font color='blue'>✦</font> Inspecting the channel"
        )
    );
}

#[test]
fn lark_agent_list_card_renders_one_right_aligned_toggle_button_per_agent() {
    let reply = ChannelReply::agent_list(vec![
        agent_status_with_button("codex-dev", true),
        agent_status_with_button("reviewer", false),
    ]);

    let card = LarkReplyCard::build(&reply, LarkConversation::Private);
    assert_eq!(
        card.pointer("/header/title/content").unwrap(),
        "当前对话的 Agent 状态"
    );
    assert_eq!(
        card.pointer("/header/subtitle/content").unwrap(),
        "当前对话 · 2 个 Agent"
    );
    let rows = card
        .pointer("/body/elements")
        .and_then(serde_json::Value::as_array)
        .unwrap();
    let first_button = rows[0].pointer("/columns/1/elements/0").unwrap();
    let second_button = rows[2].pointer("/columns/1/elements/0").unwrap();
    assert_eq!(first_button.pointer("/text/content").unwrap(), "Disable");
    assert_eq!(first_button["type"], "default");
    assert_eq!(
        first_button.pointer("/behaviors/0/value").unwrap(),
        &serde_json::json!({
            "agora_command": {
                "path": ["ask", "disable"],
                "arguments": { "agent_name": "codex-dev" }
            },
            "agora_conversation": "private"
        })
    );
    assert_eq!(second_button.pointer("/text/content").unwrap(), "Enable");
    assert_eq!(second_button["type"], "primary");
    assert_eq!(
        second_button.pointer("/behaviors/0/value").unwrap(),
        &serde_json::json!({
            "agora_command": {
                "path": ["ask", "enable"],
                "arguments": { "agent_name": "reviewer" }
            },
            "agora_conversation": "private"
        })
    );
    let rendered = serde_json::to_string(&card).unwrap();
    assert!(rendered.contains("已启用</font> · 接收后续消息"));
    assert!(rendered.contains("已禁用</font> · 不接收后续消息"));
    assert!(rendered.contains("配置仅对当前对话生效"));
}

#[test]
fn lark_agent_status_card_is_compact_and_has_no_toggle_button() {
    let reply = ChannelReply::agent_status(ChannelAgentStatus::new("reviewer", false));

    let card = LarkReplyCard::build(&reply, LarkConversation::Private);
    let rendered = serde_json::to_string(&card).unwrap();
    assert_eq!(
        card.pointer("/header/subtitle/content").unwrap(),
        "当前对话"
    );
    assert!(rendered.contains("**reviewer**"));
    assert!(rendered.contains("已禁用"));
    assert_eq!(
        card.pointer("/body/elements/0/columns/1/elements/0/text_align")
            .unwrap(),
        "right"
    );
    assert!(!rendered.contains("set_agent_enabled"));
    assert!(!rendered.contains("\"tag\":\"button\""));
}

#[test]
fn lark_card_uses_chinese_system_labels() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    content.apply_output(OutputEvent::Thinking {
        text: "Inspecting the project".to_string(),
    });
    content.apply_output(OutputEvent::CommandExecution {
        id: "command-1".to_string(),
        command: "cargo test".to_string(),
        status: ProgressStatus::Completed,
        exit_code: None,
    });
    content.apply_output(OutputEvent::Answer {
        text: "All checks passed.".to_string(),
    });
    content.apply_output(OutputEvent::Usage(TokenUsage {
        input_tokens: 42_800,
        cached_input_tokens: 31_600,
        output_tokens: 3_200,
        reasoning_output_tokens: 1_900,
    }));
    content.complete();

    let card = content.build_card();
    let rendered = serde_json::to_string(&card).unwrap();

    assert_eq!(
        card.pointer("/header/text_tag_list/0/text/content")
            .and_then(serde_json::Value::as_str),
        Some("已完成")
    );
    assert!(rendered.contains("**任务过程**"));
    assert!(rendered.contains("1 个阶段"));
    assert!(rendered.contains("1 项已完成"));
    assert!(rendered.contains("**最终回答**"));
    assert!(rendered.contains("<font color='grey'>Total</font>"));
    assert!(rendered.contains("<font color='grey'>Input</font>"));
    assert!(rendered.contains("<font color='grey'>Output</font>"));
    assert!(rendered.contains("<font color='grey'>Reasoning</font>"));
}

#[test]
fn lark_card_groups_thinking_and_running_progress_in_one_expanded_panel() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    content.apply_output(OutputEvent::Thinking {
        text: "Inspecting the channel".to_string(),
    });
    content.apply_output(OutputEvent::CommandExecution {
        id: "command-1".to_string(),
        command: "cargo test --workspace".to_string(),
        status: ProgressStatus::Running,
        exit_code: None,
    });
    content.apply_output(OutputEvent::Progress {
        id: "message-1".to_string(),
        text: "Checking test results".to_string(),
        status: ProgressStatus::Completed,
    });

    let card = content.build_card();
    let process = card.pointer("/body/elements/0").unwrap();

    assert_eq!(process["tag"], "collapsible_panel");
    assert_eq!(process["expanded"], true);
    assert_eq!(process["background_color"], "grey-50");
    assert_eq!(process.pointer("/border/color").unwrap(), "grey-200");
    assert_eq!(process.pointer("/border/corner_radius").unwrap(), "8px");
    assert!(process.pointer("/header/background_color").is_none());
    assert_eq!(process["padding"], "2px 12px 10px 12px");
    assert_eq!(
        process.pointer("/header/padding").unwrap(),
        "8px 12px 8px 12px"
    );
    assert_eq!(
        process.pointer("/elements/0/content").unwrap(),
        "<font color='blue'>**01**</font>  **思考过程**\n<font color='blue'>✦</font> Inspecting the channel"
    );
    assert_eq!(process.pointer("/elements/1/tag").unwrap(), "column_set");
    assert_eq!(
        process
            .pointer("/elements/1/columns/0/elements/0/columns/0/elements/0/content")
            .unwrap(),
        "**SHELL**"
    );
    assert_eq!(
        process
            .pointer("/elements/1/columns/0/elements/0/columns/1/elements/0/content")
            .unwrap(),
        "<font color='blue'>●  Running</font>"
    );
    assert_eq!(
        process
            .pointer("/elements/1/columns/0/elements/1/content")
            .unwrap(),
        "```bash\n$ cargo test --workspace\n```"
    );
    assert_eq!(
        process.pointer("/elements/2/content").unwrap(),
        "<font color='green'>✓</font>  Checking test results"
    );
    assert_eq!(
        process.pointer("/header/title/content").unwrap(),
        "**任务过程**  <font color='grey'>· 1 个阶段</font> · <font color='green'>✓</font> <font color='grey'>1 项已完成</font> · <font color='blue'>●</font> <font color='grey'>1 项进行中</font>"
    );
}

#[test]
fn lark_card_renders_agent_command_in_one_light_console() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    content.apply_output(OutputEvent::CommandExecution {
        id: "command-1".to_string(),
        command: "/bin/bash -lc 'cargo test' && echo $HOME".to_string(),
        status: ProgressStatus::Completed,
        exit_code: Some(0),
    });

    let card = content.build_card();
    let console = card.pointer("/body/elements/0/elements/0").unwrap();
    let console_header = console.pointer("/columns/0/elements/0").unwrap();
    let command = console.pointer("/columns/0/elements/1").unwrap();

    assert_eq!(console["tag"], "column_set");
    assert_eq!(console["background_style"], "cus-0");
    assert_eq!(
        console.pointer("/columns/0/padding").unwrap(),
        "6px 8px 8px 8px"
    );
    assert_eq!(console_header["tag"], "column_set");
    assert_eq!(
        console_header
            .pointer("/columns/0/elements/0/content")
            .unwrap(),
        "**SHELL**"
    );
    assert_eq!(
        console_header
            .pointer("/columns/1/elements/0/content")
            .unwrap(),
        "<font color='green'>✓  exit 0</font>"
    );
    assert_eq!(
        console_header
            .pointer("/columns/1/elements/0/text_align")
            .unwrap(),
        "right"
    );
    assert_eq!(command["tag"], "markdown");
    assert_eq!(
        command["content"],
        "```bash\n$ /bin/bash -lc 'cargo test' && echo $HOME\n```"
    );
    assert_eq!(
        card.pointer("/config/style/color/cus-0/light_mode")
            .unwrap(),
        "rgba(230, 233, 238, 1)"
    );
}

#[test]
fn lark_card_renders_the_complete_agent_command() {
    let command = format!(
        "/bin/bash -lc \"pwd && {} && echo end-of-command\"",
        "rg --files ".repeat(20)
    );
    assert!(command.chars().count() > 160);
    let mut content = LarkCardContent::new("codex-dev".to_string());
    content.apply_output(OutputEvent::CommandExecution {
        id: "command-1".to_string(),
        command,
        status: ProgressStatus::Running,
        exit_code: None,
    });

    let card = content.build_card();
    let rendered = card
        .pointer("/body/elements/0/elements/0/columns/0/elements/1/content")
        .and_then(serde_json::Value::as_str)
        .unwrap();

    assert!(rendered.starts_with("```bash\n$ /bin/bash"));
    assert!(rendered.contains("end-of-command"));
    assert!(!rendered.contains("..."));
    assert!(rendered.ends_with("\n```"));
}

#[test]
fn lark_card_collapses_progress_after_completion() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    content.apply_output(OutputEvent::CommandExecution {
        id: "command-1".to_string(),
        command: "cargo test".to_string(),
        status: ProgressStatus::Completed,
        exit_code: None,
    });
    content.complete();

    let card = content.build_card();
    let process = card.pointer("/body/elements/0").unwrap();

    assert_eq!(process["tag"], "collapsible_panel");
    assert_eq!(process["expanded"], false);
    assert_eq!(
        process.pointer("/header/title/content").unwrap(),
        "**任务过程**  <font color='grey'>· 1 个阶段</font> · <font color='green'>✓</font> <font color='grey'>1 项已完成</font>"
    );
}

#[test]
fn lark_card_progress_summary_shows_completed_and_failed_statuses() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    for index in 0..2 {
        content.apply_output(OutputEvent::Progress {
            id: format!("completed-{index}"),
            text: format!("Completed {index}"),
            status: ProgressStatus::Completed,
        });
    }
    content.apply_output(OutputEvent::Progress {
        id: "failed-1".to_string(),
        text: "Failed 1".to_string(),
        status: ProgressStatus::Failed,
    });
    content.complete();

    let card = content.build_card();
    let process = card.pointer("/body/elements/0").unwrap();

    assert_eq!(
        process.pointer("/header/title/content").unwrap(),
        "**任务过程**  <font color='grey'>· 1 个阶段</font> · <font color='green'>✓</font> <font color='grey'>2 项已完成</font> · <font color='red'>×</font> <font color='grey'>1 项失败</font>"
    );
}

#[test]
fn lark_card_failure_shows_safe_summary_and_collapsed_details() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    content.fail("agent process exited with code 1; Authorization: Bearer top-secret".to_string());

    let card = content.build_card();
    let rendered = serde_json::to_string(&card).unwrap();
    let details = card.pointer("/body/elements/1").unwrap();

    assert_eq!(
        card.pointer("/header/text_tag_list/0/text/content")
            .and_then(serde_json::Value::as_str),
        Some("失败")
    );
    assert!(rendered.contains("<font color='red'>▌</font> **任务失败**"));
    assert!(rendered.contains("Agent 进程在完成任务前退出。"));
    assert!(rendered.contains("建议：请重试"));
    assert!(!rendered.contains("Authorization"));
    assert!(!rendered.contains("top-secret"));
    assert_eq!(details["tag"], "collapsible_panel");
    assert_eq!(details["expanded"], false);
    assert_eq!(
        details.pointer("/header/title/content").unwrap(),
        "**技术详情**  <font color='grey'>· 进程退出</font>"
    );
    assert_eq!(
        details.pointer("/elements/0/content").unwrap(),
        "完整错误已写入 daemon 日志。"
    );
}

#[test]
fn lark_card_labels_an_answer_as_partial_when_the_run_fails() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    content.apply_output(OutputEvent::Answer {
        text: "Work completed before the error.".to_string(),
    });
    content.fail("agent process exited with code 1".to_string());

    let rendered = serde_json::to_string(&content.build_card()).unwrap();
    let failure_index = rendered.find("**任务失败**").unwrap();
    let answer_index = rendered.find("**部分回答**").unwrap();

    assert!(failure_index < answer_index);
    assert!(rendered.contains("<font color='blue'>▌</font> **部分回答**"));
    assert!(!rendered.contains("**最终回答**"));
    assert!(rendered.contains("Work completed before the error."));
}

#[test]
fn lark_card_preserves_output_and_marks_the_run_as_stopped() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    content.apply_output(OutputEvent::Thinking {
        text: "Inspecting the project".to_string(),
    });
    content.apply_output(OutputEvent::CommandExecution {
        id: "command-1".to_string(),
        command: "cargo test".to_string(),
        status: ProgressStatus::Running,
        exit_code: None,
    });
    content.apply_output(OutputEvent::Answer {
        text: "Work completed before the stop.".to_string(),
    });
    content.stop();

    let card = content.build_card();
    let rendered = serde_json::to_string(&card).unwrap();
    let elements = card
        .pointer("/body/elements")
        .and_then(serde_json::Value::as_array)
        .unwrap();

    assert_eq!(
        card.pointer("/header/text_tag_list/0/text/content")
            .and_then(serde_json::Value::as_str),
        Some("已停止")
    );
    assert_eq!(
        card.pointer("/header/template")
            .and_then(serde_json::Value::as_str),
        Some("grey")
    );
    assert!(rendered.contains("■  Stopped"));
    assert!(rendered.contains("```bash\\n$ cargo test\\n```"));
    assert!(rendered.contains("<font color='grey'>1 项已停止</font>"));
    assert!(rendered.contains("<font color='grey'>▌</font> **任务已停止**"));
    assert!(rendered.contains("已按请求停止任务，已有输出已保留。"));
    assert!(rendered.contains("<font color='blue'>▌</font> **部分回答**"));
    assert!(rendered.contains("Work completed before the stop."));
    assert!(
        elements[0]["content"]
            .as_str()
            .is_some_and(|content| content.contains("**任务已停止**"))
    );
    assert_eq!(elements[1]["tag"], "hr");
    assert!(
        elements[2]["content"]
            .as_str()
            .is_some_and(|content| content.contains("**部分回答**"))
    );
    assert_eq!(elements[3]["tag"], "hr");
    assert_eq!(elements[4]["tag"], "collapsible_panel");
}

#[test]
fn lark_card_preserves_output_and_marks_the_run_as_interrupted() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    content.apply_output(OutputEvent::CommandExecution {
        id: "command-1".to_string(),
        command: "cargo test".to_string(),
        status: ProgressStatus::Running,
        exit_code: None,
    });
    content.apply_output(OutputEvent::Answer {
        text: "Work completed before shutdown.".to_string(),
    });

    content.interrupt();

    let card = content.build_card();
    let rendered = serde_json::to_string(&card).unwrap();
    assert_eq!(
        card.pointer("/header/text_tag_list/0/text/content")
            .and_then(serde_json::Value::as_str),
        Some("已中断")
    );
    assert_eq!(
        card.pointer("/header/template")
            .and_then(serde_json::Value::as_str),
        Some("orange")
    );
    assert!(rendered.contains("■  Stopped"));
    assert!(rendered.contains("```bash\\n$ cargo test\\n```"));
    assert!(rendered.contains("<font color='orange'>▌</font> **任务已中断**"));
    assert!(rendered.contains("Agora Node 即将退出，本次任务已中断，当前输出已保留。"));
    assert!(rendered.contains("Node 恢复后，请重新发送消息继续。"));
    assert!(rendered.contains("<font color='blue'>▌</font> **部分回答**"));
    assert!(rendered.contains("Work completed before shutdown."));
}

#[test]
fn lark_card_groups_process_and_keeps_final_answer_separate() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    content.apply_output(OutputEvent::Thinking {
        text: "Inspecting the channel\nChecking reply delivery".to_string(),
    });
    content.apply_output(OutputEvent::CommandExecution {
        id: "command-1".to_string(),
        command: "cargo test".to_string(),
        status: ProgressStatus::Running,
        exit_code: None,
    });
    content.apply_output(OutputEvent::CommandExecution {
        id: "command-1".to_string(),
        command: "cargo test".to_string(),
        status: ProgressStatus::Completed,
        exit_code: Some(0),
    });
    content.apply_output(OutputEvent::Answer {
        text: "The Lark path is ready.".to_string(),
    });
    content.complete();

    let card = content.build_card();
    assert_eq!(
        card.pointer("/header/title/content")
            .and_then(|v| v.as_str()),
        Some("codex-dev")
    );
    assert_eq!(
        card.pointer("/header/text_tag_list/0/text/content")
            .and_then(|v| v.as_str()),
        Some("已完成")
    );
    let rendered = serde_json::to_string(&card).unwrap();
    let elements = card
        .pointer("/body/elements")
        .and_then(serde_json::Value::as_array)
        .unwrap();
    assert!(
        elements[0]["content"]
            .as_str()
            .is_some_and(|content| content.contains("**最终回答**"))
    );
    assert_eq!(elements[1]["tag"], "hr");
    assert_eq!(elements[2]["tag"], "collapsible_panel");
    assert_eq!(elements[2]["expanded"], false);
    assert!(rendered.contains("**任务过程**"));
    assert!(rendered.contains("<font color='blue'>**01**</font>  **思考过程**"));
    assert!(rendered.contains("<font color='blue'>✦</font> Inspecting the channel"));
    assert!(rendered.contains("Checking reply delivery"));
    assert!(rendered.contains("✓  exit 0"));
    assert!(rendered.contains("```bash\\n$ cargo test\\n```"));
    assert!(rendered.contains("<font color='blue'>▌</font> **最终回答**"));
    assert!(rendered.contains("The Lark path is ready."));
    assert!(!rendered.contains("正在等待 Agent 输出"));
    assert_eq!(rendered.matches("```bash\\n$ cargo test\\n```").count(), 1);
    assert_eq!(rendered.matches("collapsible_panel").count(), 1);
}

#[test]
fn lark_card_shows_a_placeholder_before_agent_output() {
    let content = LarkCardContent::new("codex-dev".to_string());

    let rendered = serde_json::to_string(&content.build_card()).unwrap();

    assert!(rendered.contains("> 正在等待 Agent 输出..."));
}

#[test]
fn lark_card_shows_a_bottom_stop_button_only_while_the_task_is_active() {
    let mut content = LarkCardContent::with_interrupt(
        "codex-dev".to_string(),
        Some("interrupt-42".to_string()),
        LarkConversation::Private,
    );

    let running = content.build_card();
    let elements = running
        .pointer("/body/elements")
        .and_then(serde_json::Value::as_array)
        .unwrap();
    let action_row = elements.last().unwrap();
    let button = action_row.pointer("/columns/0/elements/0").unwrap();

    assert_eq!(action_row["tag"], "column_set");
    assert_eq!(action_row["horizontal_align"], "right");
    assert_eq!(button["tag"], "button");
    assert_eq!(button["type"], "danger");
    assert_eq!(button.pointer("/text/content").unwrap(), "结束任务");
    assert_eq!(button.pointer("/behaviors/0/type").unwrap(), "callback");
    assert_eq!(
        button.pointer("/behaviors/0/value").unwrap(),
        &serde_json::json!({
            "agora_interrupt": "interrupt-42",
            "agora_conversation": "private"
        })
    );

    content.queue(2);
    assert!(
        serde_json::to_string(&content.build_card())
            .unwrap()
            .contains("agora_interrupt")
    );

    content.complete();
    assert!(
        !serde_json::to_string(&content.build_card())
            .unwrap()
            .contains("agora_interrupt")
    );
}

#[test]
fn lark_card_shows_queued_state_until_the_agent_starts() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    content.queue(2);

    let queued = content.build_card();
    assert_eq!(
        queued
            .pointer("/header/text_tag_list/0/text/content")
            .and_then(serde_json::Value::as_str),
        Some("排队中")
    );
    assert_eq!(
        queued
            .pointer("/header/template")
            .and_then(serde_json::Value::as_str),
        Some("grey")
    );
    assert_eq!(
        queued
            .pointer("/body/elements/0/content")
            .and_then(serde_json::Value::as_str),
        Some("> 正在排队，前面还有 2 个任务...")
    );

    content.queue(1);
    assert_eq!(
        content
            .build_card()
            .pointer("/body/elements/0/content")
            .and_then(serde_json::Value::as_str),
        Some("> 正在排队，前面还有 1 个任务...")
    );

    content.start();

    let running = content.build_card();
    assert_eq!(
        running
            .pointer("/header/text_tag_list/0/text/content")
            .and_then(serde_json::Value::as_str),
        Some("运行中")
    );
    assert_eq!(
        running
            .pointer("/body/elements/0/content")
            .and_then(serde_json::Value::as_str),
        Some("> 正在等待 Agent 输出...")
    );
}

#[test]
fn lark_card_keeps_all_thinking_updates_with_latest_last() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    for index in 0..5 {
        content.apply_output(OutputEvent::Thinking {
            text: format!("Thinking {index}"),
        });
    }

    let card = content.build_card();
    let rendered =
        serde_json::to_string(card.pointer("/body/elements/0/elements").unwrap()).unwrap();
    let positions = (0..5)
        .map(|index| rendered.find(&format!("Thinking {index}")).unwrap())
        .collect::<Vec<_>>();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(rendered.contains("<font color='blue'>**01**</font>"));
    assert!(rendered.contains("<font color='blue'>**05**</font>"));
}

#[test]
fn lark_card_keeps_all_progress_entries_with_latest_first() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    for index in 0..6 {
        content.apply_output(OutputEvent::Progress {
            id: format!("progress-{index}"),
            text: format!("Progress {index}"),
            status: ProgressStatus::Completed,
        });
    }

    let card = content.build_card();
    let rendered = card
        .pointer("/body/elements/0/elements/0/content")
        .and_then(|value| value.as_str())
        .unwrap();
    assert_eq!(
        rendered,
        "<font color='green'>✓</font>  Progress 5\n<font color='green'>✓</font>  Progress 4\n<font color='green'>✓</font>  Progress 3\n<font color='green'>✓</font>  Progress 2\n<font color='green'>✓</font>  Progress 1\n<font color='green'>✓</font>  Progress 0"
    );
}

#[test]
fn lark_card_groups_progress_under_the_latest_thinking_phase() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    content.apply_output(OutputEvent::Thinking {
        text: "Inspect the project".to_string(),
    });
    content.apply_output(OutputEvent::Progress {
        id: "read-config".to_string(),
        text: "Read config".to_string(),
        status: ProgressStatus::Running,
    });
    content.apply_output(OutputEvent::Progress {
        id: "read-source".to_string(),
        text: "Read source".to_string(),
        status: ProgressStatus::Completed,
    });
    content.apply_output(OutputEvent::Thinking {
        text: "Verify behavior".to_string(),
    });
    content.apply_output(OutputEvent::Progress {
        id: "run-tests".to_string(),
        text: "Run tests".to_string(),
        status: ProgressStatus::Completed,
    });
    content.apply_output(OutputEvent::Progress {
        id: "read-config".to_string(),
        text: "Read config".to_string(),
        status: ProgressStatus::Completed,
    });

    let process = content.build_card();
    let process = process.pointer("/body/elements/0").unwrap();
    let rendered = serde_json::to_string(process.pointer("/elements").unwrap()).unwrap();

    assert_eq!(
        process.pointer("/header/title/content").unwrap(),
        "**任务过程**  <font color='grey'>· 2 个阶段</font> · <font color='green'>✓</font> <font color='grey'>3 项已完成</font>"
    );
    let inspect = rendered.find("Inspect the project").unwrap();
    let read_config = rendered.find("Read config").unwrap();
    let read_source = rendered.find("Read source").unwrap();
    let verify = rendered.find("Verify behavior").unwrap();
    let run_tests = rendered.find("Run tests").unwrap();
    assert!(inspect < read_config);
    assert!(read_config < read_source);
    assert!(read_source < verify);
    assert!(verify < run_tests);
    assert!(rendered.contains("<font color='blue'>**01**</font>"));
    assert!(rendered.contains("<font color='blue'>**02**</font>"));
}

#[test]
fn lark_card_limits_rendered_elements_and_keeps_the_latest_process() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    for index in 0..40 {
        content.apply_output(OutputEvent::Thinking {
            text: format!("Thinking {index}"),
        });
        content.apply_output(OutputEvent::CommandExecution {
            id: format!("command-{index}"),
            command: format!("cargo test package-{index}"),
            status: ProgressStatus::Completed,
            exit_code: Some(0),
        });
    }
    content.apply_output(OutputEvent::Answer {
        text: "Partial answer".to_string(),
    });
    content.apply_output(OutputEvent::Usage(TokenUsage {
        input_tokens: 42_800,
        cached_input_tokens: 31_600,
        output_tokens: 3_200,
        reasoning_output_tokens: 1_900,
    }));
    content.fail("agent exited unexpectedly".to_string());

    let card = content.build_card();
    let rendered = serde_json::to_string(&card).unwrap();

    assert!(tagged_element_count(&card) <= 200);
    assert!(rendered.contains("Thinking 39"));
    assert!(!rendered.contains("Thinking 0"));
    assert!(rendered.contains("已省略 24 个较早阶段"));
}

fn tagged_element_count(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Array(values) => values.iter().map(tagged_element_count).sum(),
        serde_json::Value::Object(values) => {
            usize::from(values.contains_key("tag"))
                + values.values().map(tagged_element_count).sum::<usize>()
        }
        _ => 0,
    }
}

#[test]
fn lark_card_renders_token_usage_without_a_heading() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    content.apply_output(OutputEvent::Answer {
        text: "All checks passed.".to_string(),
    });
    content.apply_output(OutputEvent::Usage(TokenUsage {
        input_tokens: 42_800,
        cached_input_tokens: 31_600,
        output_tokens: 3_200,
        reasoning_output_tokens: 1_900,
    }));

    content.complete();
    let card = content.build_card();
    let rendered = serde_json::to_string(&card).unwrap();
    assert!(rendered.contains("All checks passed."));
    assert!(!rendered.contains("Usage"));
    let elements = card
        .pointer("/body/elements")
        .and_then(|value| value.as_array())
        .unwrap();
    assert_eq!(elements[elements.len() - 2]["tag"], "hr");
    let usage = elements.last().unwrap();
    assert_eq!(usage["tag"], "column_set");
    let columns = usage["columns"].as_array().unwrap();
    assert_eq!(columns.len(), 4);
    assert_eq!(
        columns[0].pointer("/elements/0/content").unwrap(),
        "<font color='grey'>Total</font>\n**46.0K**\n<font color='grey'>tokens</font>"
    );
    assert_eq!(
        columns[1].pointer("/elements/0/content").unwrap(),
        "<font color='grey'>Input</font>\n**42.8K**\n<font color='grey'>31.6K cached</font>"
    );
    assert_eq!(
        columns[2].pointer("/elements/0/content").unwrap(),
        "<font color='grey'>Output</font>\n**3.2K**\n<font color='grey'>tokens</font>"
    );
    assert_eq!(
        columns[3].pointer("/elements/0/content").unwrap(),
        "<font color='grey'>Reasoning</font>\n**1.9K**\n<font color='grey'>of output</font>"
    );
}

#[test]
fn lark_reply_card_supports_plain_text_and_danger_actions() {
    let text = LarkReplyCard::build(
        &ChannelReply::Text("plain reply".to_string()),
        LarkConversation::Private,
    );
    assert_eq!(
        text.pointer("/body/elements/0/content")
            .and_then(serde_json::Value::as_str),
        Some("plain reply")
    );

    let danger = ChannelButton::new(
        "Delete",
        ChannelButtonStyle::Danger,
        CommandRequest::new(["delete"]),
    );
    assert_eq!(
        LarkReplyCard::button(&danger, LarkConversation::Private)["type"],
        "danger"
    );
}

#[test]
fn lark_card_bounds_one_large_phase_and_preserves_terminal_markers() {
    let mut content = LarkCardContent::new("codex-dev".to_string());
    content.apply_output(OutputEvent::Thinking {
        text: "One large phase".to_string(),
    });
    for index in 0..60 {
        content.apply_output(OutputEvent::CommandExecution {
            id: format!("command-{index}"),
            command: format!("command {index}"),
            status: if index == 58 {
                ProgressStatus::Failed
            } else if index == 59 {
                ProgressStatus::Stopped
            } else {
                ProgressStatus::Completed
            },
            exit_code: None,
        });
    }
    let mut markers = LarkCardContent::new("codex-dev".to_string());
    markers.apply_output(OutputEvent::Progress {
        id: "running-message".to_string(),
        text: "Still running".to_string(),
        status: ProgressStatus::Running,
    });
    markers.apply_output(OutputEvent::Progress {
        id: "stopped-message".to_string(),
        text: "Was stopped".to_string(),
        status: ProgressStatus::Stopped,
    });

    let card = content.build_card();
    let rendered = serde_json::to_string(&card).unwrap();
    assert!(tagged_element_count(&card) <= 200);
    assert!(rendered.contains("已截断"));
    assert!(rendered.contains("×  Failed"));
    assert!(rendered.contains("■  Stopped"));
    let markers = serde_json::to_string(&markers.build_card()).unwrap();
    assert!(markers.contains("<font color='blue'>●</font>  Still running"));
    assert!(markers.contains("<font color='grey'>■</font>  Was stopped"));
}

#[test]
fn lark_card_formats_token_extremes_and_truncates_unicode_on_a_boundary() {
    assert_eq!(LarkCardContent::format_tokens(999), "999");
    assert_eq!(LarkCardContent::format_tokens(1_000_000), "1.0M");

    let answer = format!("prefix{}tail", "界".repeat(MAX_ANSWER_BYTES));
    let truncated = LarkCardContent::truncate_answer(&answer);
    assert!(truncated.starts_with(i18n::OUTPUT_TRUNCATED));
    assert!(truncated.ends_with("tail"));
    assert!(truncated.len() <= MAX_ANSWER_BYTES + "界".len());

    let mut content = LarkCardContent::new("codex-dev".to_string());
    content.apply_output(OutputEvent::Answer { text: answer });
    assert!(content.answer.starts_with(i18n::OUTPUT_TRUNCATED));
    assert!(content.answer.ends_with("tail"));
    assert!(content.answer.len() <= MAX_ANSWER_BYTES);

    for index in 0..100 {
        content.apply_output(OutputEvent::Thinking {
            text: format!("phase-{index}"),
        });
    }
    assert!(content.process.len() <= 64);

    let mut progress = LarkCardContent::new("codex-dev".to_string());
    for index in 0..100 {
        progress.apply_output(OutputEvent::Progress {
            id: index.to_string(),
            text: format!("progress-{index}"),
            status: ProgressStatus::Running,
        });
    }
    assert!(progress.process.front().unwrap().progress.len() <= 64);
}
