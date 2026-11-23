use super::*;
use crate::task::{TokenUsage, WebAction};

#[test]
fn web_titles_update_in_place_and_completed_results_do_not_repeat_descriptions() {
    let mut content = content();
    for queries in [vec![], vec!["北京天气".into()], vec!["北京天气".into()]] {
        content.apply(RunEvent::Output(OutputEvent::ToolCall {
            id: "search".into(),
            text: "Search ``".into(),
            web_action: Some(WebAction::Search { queries }),
            status: ProgressStatus::Running,
        }));
    }
    content.apply(RunEvent::Output(OutputEvent::ToolCall {
        id: "search".into(),
        text: "Search `北京天气`".into(),
        web_action: Some(WebAction::Search {
            queries: vec!["北京天气".into()],
        }),
        status: ProgressStatus::Completed,
    }));
    content.apply(RunEvent::Completed { exit_code: 0 });
    let events: Vec<_> = content.pending.iter().cloned().collect();
    let starts: Vec<_> = events
        .iter()
        .filter(|e| e["event_type"] == "TOOL_CALL_START")
        .map(event_content)
        .collect();
    let updates: Vec<_> = events
        .iter()
        .filter(|e| e["event_type"] == "TOOL_CALL_CHUNK")
        .map(event_content)
        .collect();
    let results: Vec<_> = events
        .iter()
        .filter(|e| e["event_type"] == "TOOL_CALL_RESULT")
        .map(event_content)
        .collect();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0]["title"], "搜索资料");
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["title"], "搜索 · 北京天气");
    assert_eq!(updates[0]["delta"], "");
    assert_eq!(updates[0]["toolCallId"], starts[0]["toolCallId"]);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["content"], "完成");
    assert_eq!(results[0]["toolCallId"], starts[0]["toolCallId"]);
}

#[test]
fn web_action_titles_distinguish_search_open_and_find_without_empty_markup() {
    for (action, title) in [
        (
            WebAction::Search {
                queries: vec!["上海天气".into(), "北京天气".into()],
            },
            "搜索 · 上海天气；北京天气",
        ),
        (
            WebAction::OpenPage {
                url: Some("https://example.com".into()),
            },
            "打开网页 · https://example.com",
        ),
        (WebAction::OpenPage { url: None }, "打开网页"),
        (
            WebAction::FindInPage {
                url: Some("https://example.com".into()),
                pattern: Some("降雨".into()),
            },
            "页内查找 · 降雨 · https://example.com",
        ),
        (
            WebAction::FindInPage {
                url: None,
                pattern: None,
            },
            "页内查找",
        ),
    ] {
        let mut content = content();
        content.apply(RunEvent::Output(OutputEvent::ToolCall {
            id: "web".into(),
            text: "Search ``".into(),
            web_action: Some(action),
            status: ProgressStatus::Completed,
        }));
        let start = content
            .pending
            .iter()
            .find(|e| e["event_type"] == "TOOL_CALL_START")
            .unwrap();
        assert_eq!(event_content(start)["title"], title);
    }
}

#[test]
fn status_only_tool_results_do_not_repeat_the_visible_title() {
    for (status, expected) in [
        (ProgressStatus::Completed, "完成"),
        (ProgressStatus::Failed, "失败"),
        (ProgressStatus::Stopped, "已停止"),
    ] {
        let mut content = content();
        for status in [ProgressStatus::Running, status, status] {
            content.apply(RunEvent::Output(OutputEvent::ToolCall {
                id: "history".into(),
                text: "读取当前聊天记录".into(),
                status,
                web_action: None,
            }));
        }
        let results: Vec<_> = content
            .pending
            .iter()
            .filter(|event| event["event_type"] == "TOOL_CALL_RESULT")
            .map(event_content)
            .collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["content"], expected);
        if status == ProgressStatus::Failed {
            content.apply(RunEvent::Output(OutputEvent::ToolOutput {
                id: "history".into(),
                text: "权限不足".into(),
                elapsed_ms: None,
            }));
            let result = event_content(content.pending.back().unwrap());
            let output: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
            assert_eq!(output["code"], "失败\n\n输出：\n权限不足");
        }
    }
}

#[test]
fn updating_a_completed_tool_title_restores_its_terminal_result() {
    for update_status in [ProgressStatus::Completed, ProgressStatus::Running] {
        let mut content = content();
        for (query, status) in [
            ("北京天气", ProgressStatus::Completed),
            ("北京实时天气", update_status),
            ("北京实时天气", update_status),
        ] {
            content.apply(RunEvent::Output(OutputEvent::ToolCall {
                id: "search".into(),
                text: "Search".into(),
                status,
                web_action: Some(WebAction::Search {
                    queries: vec![query.into()],
                }),
            }));
        }
        let events: Vec<_> = content.pending.iter().collect();
        assert_eq!(
            events
                .iter()
                .filter(|e| e["event_type"] == "TOOL_CALL_START")
                .count(),
            1
        );
        let update = events
            .iter()
            .position(|e| e["event_type"] == "TOOL_CALL_CHUNK")
            .unwrap();
        // CHUNK reopens args in the native renderer even when delta is empty.
        // Repeated snapshots must not duplicate the restored terminal result.
        assert_eq!(
            events[update..]
                .iter()
                .map(|e| e["event_type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["TOOL_CALL_CHUNK", "TOOL_CALL_END", "TOOL_CALL_RESULT"]
        );
        assert_eq!(event_content(events[update + 2])["content"], "完成");
    }
}

#[test]
fn nonfatal_diagnostics_are_hidden_without_hiding_failures_or_agent_text() {
    let mut content = content();
    let before = content.pending.clone();
    content.apply(RunEvent::Output(OutputEvent::Diagnostic {
        id: "warning".into(),
        text: "ignored configuration".into(),
    }));
    assert_eq!(content.pending, before);
    content.apply(commentary("explanation", "ignored configuration"));
    content.apply(RunEvent::Output(OutputEvent::Progress {
        id: "image".into(),
        text: "图片无法发送".into(),
        status: ProgressStatus::Failed,
    }));
    content.apply(answer("后端任务失败"));
    content.apply(RunEvent::Failed {
        message: "exit code 1".into(),
    });
    let events: Vec<_> = content.pending.iter().cloned().collect();
    assert!(text(&events, "TEXT_MESSAGE_CONTENT").contains("图片无法发送"));
    assert!(text(&events, "TEXT_MESSAGE_CONTENT").contains("ignored configuration"));
    assert!(content.final_text().contains("后端任务失败"));
}

fn content() -> Content {
    Content::new("agent".into(), "source".into())
}

fn text(events: &[Value], kind: &str) -> String {
    events
        .iter()
        .filter(|e| e["event_type"] == kind)
        .map(|e| event_content(e)["delta"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn thinking_is_omitted_while_commentary_tools_and_answer_keep_their_roles() {
    let mut content = content();
    let before_summaries = content.pending.clone();
    for summary in ["**Checking URL formatting**", "核对链接格式"] {
        content.apply(RunEvent::Output(OutputEvent::Thinking {
            text: summary.into(),
        }));
    }
    assert_eq!(content.pending, before_summaries);
    let explanation = "Checking URL formatting — 已去掉引号内末尾的换行。";
    content.apply(commentary("comment", explanation));
    content.apply(commentary("comment", explanation));
    for status in [
        ProgressStatus::Running,
        ProgressStatus::Completed,
        ProgressStatus::Completed,
    ] {
        content.apply(RunEvent::Output(OutputEvent::CommandExecution {
            id: "shell".into(),
            command: "cargo test --workspace".into(),
            status,
            exit_code: Some(0),
        }));
    }
    content.apply(RunEvent::Output(OutputEvent::ToolCall {
        web_action: None,
        id: "read".into(),
        text: "读取配置".into(),
        status: ProgressStatus::Failed,
    }));
    content.apply(answer("最终"));
    content.apply(answer("回答"));
    let before_usage = content.pending.clone();
    content.apply(RunEvent::Output(OutputEvent::Usage(TokenUsage {
        input_tokens: 987654,
        cached_input_tokens: 0,
        output_tokens: 5,
        reasoning_output_tokens: 2,
    })));
    assert_eq!(content.pending, before_usage);
    content.apply(RunEvent::Completed { exit_code: 0 });
    let events: Vec<_> = content.pending.iter().cloned().collect();
    assert_eq!(text(&events, "TEXT_MESSAGE_CONTENT"), explanation);
    let kinds: Vec<_> = events
        .iter()
        .map(|e| e["event_type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "RUN_STARTED",
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_CONTENT",
            "TEXT_MESSAGE_END",
            "TOOL_CALL_START",
            "TOOL_CALL_ARGS",
            "TOOL_CALL_END",
            "TOOL_CALL_RESULT",
            "TOOL_CALL_START",
            "TOOL_CALL_ARGS",
            "TOOL_CALL_END",
            "TOOL_CALL_RESULT",
            "RUN_FINISHED"
        ]
    );
    let result = event_content(&events[7]);
    let shell: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert_eq!(shell["code"], "$ cargo test --workspace\n\n✓ Success");
    assert_eq!(
        result["toolCallId"],
        event_content(&events[4])["toolCallId"]
    );
    let shell_events = events
        .iter()
        .filter(|e| event_content(e)["toolCallId"] == result["toolCallId"])
        .cloned()
        .collect::<Vec<_>>();
    let args: Value = serde_json::from_str(&text(&shell_events, "TOOL_CALL_ARGS")).unwrap();
    assert_eq!(args["command"], "cargo test --workspace");
    assert_eq!(content.final_text(), "最终回答");
    assert!(!serde_json::to_string(&events).unwrap().contains("最终回答"));
    assert!(events.windows(2).all(|pair| {
        pair[0]["timestamp"]
            .as_str()
            .unwrap()
            .parse::<u128>()
            .unwrap()
            < pair[1]["timestamp"]
                .as_str()
                .unwrap()
                .parse::<u128>()
                .unwrap()
    }));
}

#[test]
fn escaped_unicode_chunks_reassemble_without_exceeding_native_event_limits() {
    let mut content = content();
    let raw = "中🦀\n\"\\\u{0001}".repeat(400);
    content.apply(commentary("comment", &raw));
    content.apply(RunEvent::Output(OutputEvent::CommandExecution {
        id: "shell".into(),
        command: raw.clone(),
        status: ProgressStatus::Completed,
        exit_code: Some(0),
    }));
    content.apply(RunEvent::Completed { exit_code: 0 });
    let events: Vec<_> = content.pending.iter().cloned().collect();
    assert_eq!(text(&events, "TEXT_MESSAGE_CONTENT"), raw);
    let args: Value = serde_json::from_str(&text(&events, "TOOL_CALL_ARGS")).unwrap();
    assert_eq!(args["command"], raw);
    assert!(
        events
            .iter()
            .all(|e| e["content"].as_str().unwrap().len() < 1200)
    );
}

#[test]
fn process_limits_leave_room_for_tool_closure_and_a_separate_complete_answer() {
    let mut content = content();
    for n in 0..64 {
        content.apply(RunEvent::Output(OutputEvent::ToolCall {
            web_action: None,
            id: n.to_string(),
            text: "读取文件".into(),
            status: ProgressStatus::Running,
        }));
    }
    for n in 0..1000 {
        content.apply(commentary(&format!("comment-{n}"), "很多过程"));
    }
    content.apply(answer("最终回答完整保留"));
    content.apply(RunEvent::Completed { exit_code: 0 });
    let events: Vec<_> = content.pending.iter().cloned().collect();
    assert!(events.len() < 1000);
    let mut counts = std::collections::HashMap::new();
    for event in &events {
        *counts
            .entry(event["event_type"].as_str().unwrap())
            .or_insert(0) += 1;
    }
    assert!(counts.values().all(|count| *count <= 500));
    assert_eq!(counts["TOOL_CALL_START"], 64);
    assert_eq!(counts["TOOL_CALL_RESULT"], 64);
    assert_eq!(events.last().unwrap()["event_type"], "RUN_FINISHED");
    assert!(text(&events, "TEXT_MESSAGE_CONTENT").contains("部分运行过程已省略"));
    assert_eq!(content.final_text(), "最终回答完整保留");
    let before = content.pending.clone();
    content.apply(commentary("late", "不得出现在终态后"));
    content.apply(RunEvent::Stopped);
    assert_eq!(content.pending, before);
}

#[test]
fn failed_and_interrupted_runs_retain_partial_answers_and_close_running_tools_safely() {
    for (terminal, kind, expected, tool_status) in [
        (
            RunEvent::Failed {
                message: "timeout secret=private-credential".into(),
            },
            "RUN_ERROR",
            "执行超时",
            "未确认完成",
        ),
        (
            RunEvent::Interrupted,
            "RUN_FINISHED",
            "任务已中断",
            "已停止",
        ),
    ] {
        let mut content = content();
        content.apply(RunEvent::Output(OutputEvent::ToolCall {
            web_action: None,
            id: "t".into(),
            text: "读取文件".into(),
            status: ProgressStatus::Running,
        }));
        content.apply(answer("已生成的部分文字"));
        content.apply(terminal);
        assert_eq!(content.pending.back().unwrap()["event_type"], kind);
        let result = content
            .pending
            .iter()
            .find(|e| e["event_type"] == "TOOL_CALL_RESULT")
            .unwrap();
        assert_eq!(event_content(result)["content"], tool_status);
        assert!(content.final_text().contains(expected));
        assert!(content.final_text().contains("部分回答"));
        assert!(content.final_text().contains("已生成的部分文字"));
        assert!(!content.final_text().contains("private-credential"));
        assert!(
            !serde_json::to_string(&content.pending)
                .unwrap()
                .contains("private-credential")
        );
    }
}

#[test]
fn long_tool_details_and_changed_completion_text_remain_available_on_the_same_tool() {
    let mut content = content();
    let description = "查询带有较长路径和参数的工具。".repeat(80);
    content.apply(RunEvent::Output(OutputEvent::ToolCall {
        web_action: None,
        id: "t".into(),
        text: description.clone(),
        status: ProgressStatus::Running,
    }));
    content.apply(RunEvent::Output(OutputEvent::ToolCall {
        web_action: None,
        id: "t".into(),
        text: "已查询 6 个文件".into(),
        status: ProgressStatus::Completed,
    }));
    content.apply(RunEvent::Completed { exit_code: 0 });
    let events: Vec<_> = content.pending.iter().cloned().collect();
    let starts: Vec<_> = events
        .iter()
        .filter(|e| e["event_type"] == "TOOL_CALL_START")
        .collect();
    assert_eq!(starts.len(), 1);
    let start = event_content(starts[0]);
    assert!(start["title"].as_str().unwrap().chars().count() <= 96);
    assert!(start["title"].as_str().unwrap().ends_with('…'));
    let args: Value = serde_json::from_str(&text(&events, "TOOL_CALL_ARGS")).unwrap();
    assert_eq!(args["description"], description);
    let result = event_content(
        events
            .iter()
            .find(|e| e["event_type"] == "TOOL_CALL_RESULT")
            .unwrap(),
    );
    assert_eq!(result["toolCallId"], start["toolCallId"]);
    assert_eq!(result["content"], "完成");
    let updates: Vec<_> = events
        .iter()
        .filter(|e| e["event_type"] == "TOOL_CALL_CHUNK")
        .map(event_content)
        .collect();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["title"], "已查询 6 个文件");
    assert_eq!(updates[0]["toolCallId"], start["toolCallId"]);
    assert_eq!(updates[0]["delta"], "");
    assert_eq!(
        event_content(events.last().unwrap())["input"]["statusText"]["done"]["zh_cn"],
        "任务已完成"
    );
}

#[test]
fn exhausting_the_process_budget_does_not_lose_known_tool_failures() {
    let mut content = content();
    for n in 0..64 {
        content.apply(RunEvent::Output(OutputEvent::ToolCall {
            web_action: None,
            id: n.to_string(),
            text: "读取文件".into(),
            status: ProgressStatus::Running,
        }));
    }
    for n in 0..1000 {
        content.apply(commentary(&format!("comment-{n}"), "很多过程"));
    }
    // The process event budget is now full; tool status still needs to be
    // reported from the reserved terminal capacity, including after draining.
    content.pending.clear();
    for n in 0..64 {
        content.apply(RunEvent::Output(OutputEvent::ToolCall {
            web_action: None,
            id: n.to_string(),
            text: "读取文件失败".into(),
            status: ProgressStatus::Failed,
        }));
    }
    content.apply(RunEvent::Completed { exit_code: 0 });
    let results: Vec<_> = content
        .pending
        .iter()
        .filter(|e| e["event_type"] == "TOOL_CALL_RESULT")
        .map(event_content)
        .collect();
    assert_eq!(results.len(), 64);
    assert!(results.iter().all(|r| r["content"] == "失败\n读取文件失败"));
    assert!(content.pending.len() < 100);
}

#[test]
fn tool_output_replaces_the_status_on_one_tool_with_literal_native_code() {
    let mut content = content();
    let output = "Script completed\n{\"value\":\"```<tag>中🦀\"}\n";
    content.apply(RunEvent::Output(OutputEvent::CommandExecution {
        id: "cmd".into(),
        command: "cargo test".into(),
        status: ProgressStatus::Completed,
        exit_code: Some(0),
    }));
    let result = RunEvent::Output(OutputEvent::ToolOutput {
        id: "cmd".into(),
        text: output.into(),
        elapsed_ms: Some(1600),
    });
    content.apply(result.clone());
    content.apply(RunEvent::Output(OutputEvent::ToolOutput {
        id: "cmd".into(),
        text: output.into(),
        elapsed_ms: None,
    }));
    let before = content.pending.clone();
    content.apply(result.clone());
    // Repeated completion and output must not replace code with a status line.
    content.apply(RunEvent::Output(OutputEvent::CommandExecution {
        id: "cmd".into(),
        command: "cargo test".into(),
        status: ProgressStatus::Completed,
        exit_code: Some(0),
    }));
    assert_eq!(content.pending, before);
    content.apply(RunEvent::Completed { exit_code: 0 });
    let results: Vec<_> = content
        .pending
        .iter()
        .filter(|e| e["event_type"] == "TOOL_CALL_RESULT")
        .map(event_content)
        .collect();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["toolCallId"], results[1]["toolCallId"]);
    let code: Value = serde_json::from_str(results[1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(code["type"], "code");
    assert_eq!(code["language"], "bash");
    assert_eq!(
        code["code"],
        format!("$ cargo test\n\n{output}\n\n✓ Success · 约 1.600 秒")
    );
    let before = content.pending.clone();
    content.apply(result);
    assert_eq!(content.pending, before);
}

#[test]
fn long_tool_outputs_fit_the_full_wire_event_and_survive_exhausted_budgets() {
    let mut content = content();
    for n in 0..64 {
        content.apply(RunEvent::Output(OutputEvent::CommandExecution {
            id: n.to_string(),
            command: "cargo test".into(),
            status: ProgressStatus::Running,
            exit_code: None,
        }));
    }
    for n in 0..1000 {
        content.apply(commentary(&format!("comment-{n}"), "过程"));
    }
    content.pending.clear();
    let output = format!("{}最后一行", "中🦀\n\"\\\u{0001}".repeat(2000));
    for n in 0..64 {
        content.apply(RunEvent::Output(OutputEvent::CommandExecution {
            id: n.to_string(),
            command: "cargo test".into(),
            status: ProgressStatus::Failed,
            exit_code: Some(1),
        }));
        content.apply(RunEvent::Output(OutputEvent::ToolOutput {
            id: n.to_string(),
            text: output.clone(),
            elapsed_ms: None,
        }));
    }
    content.apply(RunEvent::Completed { exit_code: 0 });
    let mut results = std::collections::HashMap::new();
    for event in content
        .pending
        .iter()
        .filter(|e| e["event_type"] == "TOOL_CALL_RESULT")
    {
        assert!(event.to_string().len() <= 4096);
        let result = event_content(event);
        results.insert(result["toolCallId"].as_str().unwrap().to_owned(), result);
    }
    assert_eq!(results.len(), 64);
    for result in results.values() {
        let code: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
        let text = code["code"].as_str().unwrap();
        assert!(text.starts_with("$ cargo test\n\n"));
        assert!(text.contains("[输出已截断]"));
        assert!(text.ends_with("最后一行\n\n✗ Failed · exit 1"));
        assert!(!text.contains("耗时"));
    }
}

#[test]
fn empty_and_early_tool_output_never_invent_a_success_or_duration() {
    let mut content = content();
    let output = RunEvent::Output(OutputEvent::ToolOutput {
        id: "t".into(),
        text: String::new(),
        elapsed_ms: None,
    });
    let before = content.pending.clone();
    content.apply(output.clone()); // An unknown id cannot invent a tool.
    assert_eq!(content.pending, before);
    content.apply(RunEvent::Output(OutputEvent::ToolCall {
        web_action: None,
        id: "t".into(),
        text: "读取文件".into(),
        status: ProgressStatus::Running,
    }));
    content.apply(output);
    assert!(
        !content
            .pending
            .iter()
            .any(|e| e["event_type"] == "TOOL_CALL_RESULT")
    );
    content.apply(RunEvent::Stopped);
    let result = event_content(
        content
            .pending
            .iter()
            .find(|e| e["event_type"] == "TOOL_CALL_RESULT")
            .unwrap(),
    );
    let code: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert_eq!(code["language"], "text");
    assert_eq!(code["code"], "已停止\n\n输出：\n（无文本输出）");
}

#[test]
fn command_output_preview_keeps_the_input_and_last_500_characters() {
    let mut content = content();
    let command = format!("printf '%s' '{}结束'", "input".repeat(90));
    content.apply(RunEvent::Output(OutputEvent::CommandExecution {
        id: "cmd".into(),
        command: command.clone(),
        status: ProgressStatus::Failed,
        exit_code: Some(3),
    }));
    let tail = format!("{}错误", "界🦀".repeat(249));
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
            elapsed_ms: Some(120),
        }));
        let result = content
            .pending
            .iter()
            .rev()
            .find(|e| e["event_type"] == "TOOL_CALL_RESULT")
            .unwrap();
        let panel: Value =
            serde_json::from_str(event_content(result)["content"].as_str().unwrap()).unwrap();
        assert_eq!(
            panel["code"],
            format!("$ {command}\n\n{expected}\n\n✗ Failed · exit 3 · 约 0.120 秒")
        );
    }
    let events: Vec<_> = content.pending.iter().cloned().collect();
    let args: Value = serde_json::from_str(&text(&events, "TOOL_CALL_ARGS")).unwrap();
    assert_eq!(args["command"], command);
    content.apply(RunEvent::Completed { exit_code: 0 });
    let events: Vec<_> = content.pending.iter().cloned().collect();
    assert!(!text(&events, "TEXT_MESSAGE_CONTENT").contains(i18n::PROCESS_TRUNCATED));
}

#[test]
fn shell_input_takes_priority_when_escaped_output_fills_the_result_event() {
    let mut content = content();
    let command = format!("echo {}", "z".repeat(3000));
    content.apply(RunEvent::Output(OutputEvent::CommandExecution {
        id: "shell".into(),
        command: command.clone(),
        status: ProgressStatus::Completed,
        exit_code: Some(0),
    }));
    content.apply(RunEvent::Output(OutputEvent::ToolOutput {
        id: "shell".into(),
        text: format!("{}last", "\u{0001}".repeat(501)),
        elapsed_ms: None,
    }));
    let event = content.pending.back().unwrap();
    assert_eq!(event["event_type"], "TOOL_CALL_RESULT");
    assert!(event.to_string().len() <= 4096);
    let panel: Value =
        serde_json::from_str(event_content(event)["content"].as_str().unwrap()).unwrap();
    let code = panel["code"].as_str().unwrap();
    assert!(code.starts_with(&format!("$ {command}\n\n{}", i18n::OUTPUT_TRUNCATED)));
    assert_eq!(code.matches(i18n::OUTPUT_TRUNCATED).count(), 1);
    assert!(code.ends_with("last\n\n✓ Success"));
}

#[test]
fn shell_panel_bounds_the_command_and_preserves_output_and_failure_footer() {
    let mut content = content();
    // Control characters exercise every nested JSON encoding, not only UTF-8 length.
    let command = format!("printf start{}end", "\u{0001}\"\\中🦀".repeat(1000));
    content.apply(RunEvent::Output(OutputEvent::CommandExecution {
        id: "shell".into(),
        command: command.clone(),
        status: ProgressStatus::Failed,
        exit_code: Some(7),
    }));
    content.apply(RunEvent::Output(OutputEvent::ToolOutput {
        id: "shell".into(),
        text: "last diagnostic".into(),
        elapsed_ms: Some(250),
    }));
    let events: Vec<_> = content.pending.iter().cloned().collect();
    let args: Value = serde_json::from_str(&text(&events, "TOOL_CALL_ARGS")).unwrap();
    assert_eq!(args["command"], command);
    let results: Vec<_> = content
        .pending
        .iter()
        .filter(|e| e["event_type"] == "TOOL_CALL_RESULT")
        .collect();
    assert!(results.iter().all(|event| event.to_string().len() <= 4096));
    let result = event_content(results.last().unwrap());
    let code: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    let text = code["code"].as_str().unwrap();
    assert!(text.starts_with("$ printf start"));
    assert!(text.contains("[命令已截断]"));
    assert!(text.ends_with("last diagnostic\n\n✗ Failed · exit 7 · 约 0.250 秒"));
    assert!(!text.contains("Success"));
}

#[test]
fn shell_panel_closes_unfinished_commands_without_reporting_success() {
    for (terminal, expected) in [
        (RunEvent::Stopped, "已停止"),
        (RunEvent::Completed { exit_code: 0 }, "未确认完成"),
    ] {
        let mut content = content();
        content.apply(RunEvent::Output(OutputEvent::CommandExecution {
            id: "shell".into(),
            command: "sleep 10".into(),
            status: ProgressStatus::Running,
            exit_code: None,
        }));
        content.apply(terminal);
        let result = event_content(
            content
                .pending
                .iter()
                .find(|e| e["event_type"] == "TOOL_CALL_RESULT")
                .unwrap(),
        );
        let code: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
        assert_eq!(code["code"], format!("$ sleep 10\n\n{expected}"));
    }
}
