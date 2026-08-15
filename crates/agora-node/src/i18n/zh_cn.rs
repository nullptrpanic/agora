use super::{FailureCopy, RunStatus};
use crate::task::ProgressStatus;

pub(crate) const THINKING_TITLE: &str = "思考过程";
pub(crate) const PROCESS_TITLE: &str = "任务过程";
pub(crate) const SHELL_TITLE: &str = "SHELL";
pub(crate) const SHELL_RUNNING: &str = "Running";
pub(crate) const SHELL_COMPLETED: &str = "Completed";
pub(crate) const SHELL_FAILED: &str = "Failed";
pub(crate) const SHELL_STOPPED: &str = "Stopped";
pub(crate) const FINAL_ANSWER_TITLE: &str = "最终回答";
pub(crate) const PARTIAL_ANSWER_TITLE: &str = "部分回答";
pub(crate) const RUN_FAILED_TITLE: &str = "任务失败";
pub(crate) const RUN_STOPPED_TITLE: &str = "任务已停止";
pub(crate) const RUN_INTERRUPTED_TITLE: &str = "任务已中断";
pub(crate) const TECHNICAL_DETAILS_TITLE: &str = "技术详情";
pub(crate) const WAITING_FOR_AGENT: &str = "正在等待 Agent 输出...";
pub(crate) const AGENT_RUN_FAILED: &str = "Agent 未能完成本次任务。";
pub(crate) const RETRY_ADVICE: &str = "建议：请重试；如果仍然失败，请查看技术详情和 daemon 日志。";
pub(crate) const ERROR_WRITTEN_TO_LOG: &str = "完整错误已写入 daemon 日志。";
pub(crate) const RUN_STOPPED_BODY: &str = "已按请求停止任务，已有输出已保留。";
pub(crate) const RUN_INTERRUPTED_BODY: &str =
    "Agora Node 即将退出，本次任务已中断，当前输出已保留。\nNode 恢复后，请重新发送消息继续。";
pub(crate) const STOP_TASK: &str = "结束任务";
pub(crate) const TOTAL: &str = "Total";
pub(crate) const INPUT: &str = "Input";
pub(crate) const OUTPUT: &str = "Output";
pub(crate) const REASONING: &str = "Reasoning";
pub(crate) const TOKENS: &str = "tokens";
pub(crate) const REASONING_DETAIL: &str = "of output";
pub(crate) const OUTPUT_TRUNCATED: &str = "[输出已截断]\n\n";
pub(crate) const PERMISSION_DENIED_TITLE: &str = "无权访问此 Channel";
pub(crate) const PERMISSION_DENIED_HEADER_TITLE: &str = "访问受限";
pub(crate) const PERMISSION_DENIED_SUBTITLE: &str = "请更新 Channel 权限配置";
pub(crate) const PERMISSION_IDENTIFIERS_TITLE: &str = "访问标识";
pub(crate) const PERMISSION_CONFIG_EXAMPLE_TITLE: &str = "配置示例";
pub(crate) const PERMISSION_USER_NOT_ALLOWED: &str = "当前用户未在允许列表中。";
pub(crate) const PERMISSION_GROUP_NOT_ALLOWED: &str = "当前群聊未在允许列表中。";

pub(crate) const AGENT_STATUS_TITLE: &str = "当前对话的 Agent 状态";
pub(crate) const CURRENT_CONVERSATION: &str = "当前对话";
pub(crate) const CURRENT_CONVERSATION_ONLY: &str = "配置仅对当前对话生效";
pub(crate) const MESSAGE_DELIVERY_STATUS: &str = "消息接收状态";
pub(crate) const AGENT_ENABLED: &str = "已启用";
pub(crate) const AGENT_DISABLED: &str = "已禁用";
pub(crate) const AGENT_ENABLED_DESCRIPTION: &str = "接收后续消息";
pub(crate) const AGENT_DISABLED_DESCRIPTION: &str = "不接收后续消息";
pub(crate) const ENABLE_AGENT: &str = "启用";
pub(crate) const DISABLE_AGENT: &str = "禁用";
pub(crate) const NO_ENABLED_AGENTS: &str = "当前对话没有启用的 Agent。";

pub(crate) const STOP_COMMAND_DESCRIPTION: &str = "停止当前对话中正在运行或排队的 Agent 任务。";
pub(crate) const STOP_AGENT_ARGUMENT_DESCRIPTION: &str =
    "已配置的 Agent 名称；省略时停止全部 Agent。";
pub(crate) const RESET_COMMAND_DESCRIPTION: &str = "停止任务并重置后端 Agent 会话。";
pub(crate) const RESET_SUCCESSFUL: &str = "重置成功。";
pub(crate) const ASK_COMMAND_DESCRIPTION: &str = "向指定 Agent 提问或控制 Agent 的消息接收状态。";
pub(crate) const AGENT_NAME_ARGUMENT_DESCRIPTION: &str = "当前对话中已配置的 Agent 名称。";
pub(crate) const ASK_PROMPT_ARGUMENT_DESCRIPTION: &str = "仅发送给指定 Agent 的提示词。";
pub(crate) const ASK_LIST_DESCRIPTION: &str = "列出所有已订阅 Agent 及其当前状态。";
pub(crate) const ASK_STATUS_DESCRIPTION: &str = "查看指定 Agent 的当前状态。";
pub(crate) const ASK_DISABLE_DESCRIPTION: &str = "禁止指定 Agent 接收后续消息。";
pub(crate) const ASK_ENABLE_DESCRIPTION: &str = "允许指定 Agent 接收后续消息。";

pub(crate) const REQUIRED: &str = "必填";
pub(crate) const OPTIONAL: &str = "可选";
pub(crate) const USAGE_TITLE: &str = "用法：";
pub(crate) const ARGUMENTS_TITLE: &str = "参数：";
pub(crate) const SUBCOMMANDS_TITLE: &str = "子命令：";
pub(crate) const COMMANDS_TITLE: &str = "Agora 命令：";
pub(crate) const HELP_DESCRIPTION: &str = "显示所有命令。";
pub(crate) const UNKNOWN_STRUCTURED_COMMAND: &str = "未知的结构化命令。";

pub(crate) fn run_status(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Queued => "排队中",
        RunStatus::Running => "运行中",
        RunStatus::Completed => "已完成",
        RunStatus::Failed => "失败",
        RunStatus::Stopped => "已停止",
        RunStatus::Interrupted => "已中断",
    }
}

pub(crate) fn phase_count(count: usize) -> String {
    format!("{count} 个阶段")
}

pub(crate) fn truncated_phase_count(count: usize) -> String {
    if count == 0 {
        "已省略当前阶段中的较早过程".to_string()
    } else {
        format!("已省略 {count} 个较早阶段")
    }
}

pub(crate) fn progress_count(status: ProgressStatus, count: usize) -> String {
    match status {
        ProgressStatus::Running => format!("{count} 项进行中"),
        ProgressStatus::Completed => format!("{count} 项已完成"),
        ProgressStatus::Failed => format!("{count} 项失败"),
        ProgressStatus::Stopped => format!("{count} 项已停止"),
    }
}

pub(crate) fn queued_message(ahead: usize) -> String {
    format!("正在排队，前面还有 {ahead} 个任务...")
}

pub(crate) fn cached_tokens(tokens: impl std::fmt::Display) -> String {
    format!("{tokens} cached")
}

pub(crate) fn failure_copy(message: &str) -> FailureCopy {
    let message = message.to_ascii_lowercase();
    if message.contains("timed out") || message.contains("timeout") {
        FailureCopy {
            category: "执行超时",
            summary: "Agent 执行超时。",
        }
    } else if message.contains("session")
        && (message.contains("not found")
            || message.contains("missing")
            || message.contains("unavailable"))
    {
        FailureCopy {
            category: "会话不可用",
            summary: "Agent 会话不可用。",
        }
    } else if message.contains("attachment") {
        FailureCopy {
            category: "附件错误",
            summary: "Agent 无法处理附件。",
        }
    } else if message.contains("exit") {
        FailureCopy {
            category: "进程退出",
            summary: "Agent 进程在完成任务前退出。",
        }
    } else {
        FailureCopy {
            category: "Agent 错误",
            summary: "Agent 执行失败。",
        }
    }
}

pub(crate) fn unknown_agent(agent_name: &str) -> String {
    format!("当前对话中不存在 Agent：{agent_name}。")
}

pub(crate) fn agent_count(count: usize) -> String {
    format!("当前对话 · {count} 个 Agent")
}

pub(crate) fn no_running_agent(agent_name: &str) -> String {
    format!("当前对话中没有名为 {agent_name} 的运行中 Agent。")
}

pub(crate) fn no_running_agents() -> &'static str {
    "当前对话中没有运行中或排队中的 Agent。"
}

pub(crate) fn stopped_agents(agent_names: &[String]) -> String {
    format!(
        "已停止 {} 个 Agent：{}。",
        agent_names.len(),
        agent_names.join("、")
    )
}

pub(crate) fn reset_failed(agent_names: &[String]) -> String {
    format!("以下 Agent 重置失败：{}。", agent_names.join("、"))
}

pub(crate) fn command_details_hint(command_path: &str) -> String {
    format!("使用 {command_path} {{子命令}} help 查看详情。")
}

pub(crate) fn root_command_details_hint() -> &'static str {
    "使用 /{command} help 查看详情。"
}

pub(crate) fn usage(syntax: &str) -> String {
    format!("用法：{syntax}")
}

pub(crate) fn unknown_command(command: &str) -> String {
    format!("未知命令：{command}\n使用 /help 查看全部命令。")
}

pub(crate) fn unknown_subcommand(command_path: &str, subcommand: &str) -> String {
    format!("未知子命令：{command_path} {subcommand}\n使用 {command_path} help 查看用法。")
}
