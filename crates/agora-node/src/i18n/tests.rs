use super::*;
use crate::task::ProgressStatus;

#[test]
fn chinese_copy_covers_every_status_and_failure_category() {
    assert_eq!(run_status(RunStatus::Queued), "排队中");
    assert_eq!(run_status(RunStatus::Running), "运行中");
    assert_eq!(run_status(RunStatus::Completed), "已完成");
    assert_eq!(run_status(RunStatus::Failed), "失败");
    assert_eq!(run_status(RunStatus::Stopped), "已停止");
    assert_eq!(run_status(RunStatus::Interrupted), "已中断");

    assert_eq!(progress_count(ProgressStatus::Running, 2), "2 项进行中");
    assert_eq!(progress_count(ProgressStatus::Completed, 2), "2 项已完成");
    assert_eq!(progress_count(ProgressStatus::Failed, 2), "2 项失败");
    assert_eq!(progress_count(ProgressStatus::Stopped, 2), "2 项已停止");

    assert_eq!(failure_copy("request timed out").category, "执行超时");
    assert_eq!(failure_copy("session unavailable").category, "会话不可用");
    assert_eq!(failure_copy("attachment rejected").category, "附件错误");
    assert_eq!(failure_copy("process exit 7").category, "进程退出");
    assert_eq!(failure_copy("unexpected failure").category, "Agent 错误");
}

#[test]
fn chinese_copy_formats_dynamic_command_and_agent_messages() {
    assert_eq!(phase_count(3), "3 个阶段");
    assert_eq!(queued_message(4), "正在排队，前面还有 4 个任务...");
    assert_eq!(cached_tokens(12), "12 cached");
    assert_eq!(
        unknown_agent("reviewer"),
        "当前对话中不存在 Agent：reviewer。"
    );
    assert_eq!(agent_count(2), "当前对话 · 2 个 Agent");
    assert_eq!(
        no_running_agent("reviewer"),
        "当前对话中没有名为 reviewer 的运行中 Agent。"
    );
    assert_eq!(
        no_running_agents(),
        "当前对话中没有运行中或排队中的 Agent。"
    );
    assert_eq!(
        stopped_agents(&["codex".to_string(), "reviewer".to_string()]),
        "已停止 2 个 Agent：codex、reviewer。"
    );
    assert_eq!(
        reset_failed(&["reviewer".to_string()]),
        "以下 Agent 重置失败：reviewer。"
    );
    assert_eq!(
        command_details_hint("/ask"),
        "使用 /ask {子命令} help 查看详情。"
    );
    assert_eq!(
        root_command_details_hint(),
        "使用 /{command} help 查看详情。"
    );
    assert_eq!(usage("/ask list"), "用法：/ask list");
    assert_eq!(
        unknown_command("oops"),
        "未知命令：oops\n使用 /help 查看全部命令。"
    );
    assert_eq!(
        unknown_subcommand("/ask", "oops"),
        "未知子命令：/ask oops\n使用 /ask help 查看用法。"
    );
}
