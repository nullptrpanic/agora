use super::{FailureCopy, RunStatus};
use crate::task::ProgressStatus;

pub(crate) const THINKING_STATUS: &str = "思考中";
pub(crate) const TASK_COMPLETED: &str = "任务已完成。";
pub(crate) const PROCESS_TRUNCATED: &str = "部分运行过程已省略，最终回答不受过程上限影响。";
pub(crate) const TOOL_COMPLETED: &str = "完成";
pub(crate) const TOOL_FAILED: &str = "失败";
pub(crate) const TOOL_STOPPED: &str = "已停止";
pub(crate) const TOOL_UNCONFIRMED: &str = "未确认完成";
pub(crate) const TOOL_COMMAND: &str = "执行命令";
pub(crate) const RUN_PROGRAM: &str = "运行";
pub(crate) const REQUEST_HOST: &str = "请求";
pub(crate) const NO_PUBLIC_STEPS: &str = "没有可展示的运行步骤。";
pub(crate) const TOOL_NO_OUTPUT: &str = "（无文本输出）";
pub(crate) const LARK_COMPLETED: &str = "任务已完成";
pub(crate) const LARK_IMAGE_REFERENCE: &str = "（图片见本对话的附件；发送状态见任务过程）";
pub(crate) const COMMAND_TRUNCATED: &str = "… [命令已截断]";

pub(crate) fn shell_status(status: ProgressStatus, exit: Option<i32>) -> String {
    let label = match status {
        ProgressStatus::Completed if exit.is_none_or(|code| code == 0) => "✓ Success",
        ProgressStatus::Completed | ProgressStatus::Failed => "✗ Failed",
        ProgressStatus::Running => "● Running",
        ProgressStatus::Stopped => "■ Stopped",
    };
    match exit {
        Some(code) if code != 0 => format!("{label} · exit {code}"),
        _ => label.into(),
    }
}

pub(crate) fn shell_output(
    command: &str,
    output: &str,
    status: &str,
    elapsed_ms: Option<u64>,
) -> String {
    let mut text = format!("$ {command}\n\n");
    if !output.is_empty() {
        text.push_str(output);
        text.push_str("\n\n");
    }
    text.push_str(status);
    if let Some(ms) = elapsed_ms {
        text.push_str(&format!(" · 约 {}.{:03} 秒", ms / 1000, ms % 1000));
    }
    text
}

pub(crate) fn tool_title(text: &str) -> String {
    let mut chars = text
        .split_whitespace()
        .flat_map(|word| word.chars().chain(std::iter::once(' ')));
    let mut title: String = chars.by_ref().take(96).collect();
    if chars.next().is_some() {
        title.pop();
        title.push('…');
    }
    title.trim_end().to_owned()
}

pub(crate) fn web_tool_title(action: &crate::task::WebAction) -> String {
    use crate::task::WebAction;
    match action {
        WebAction::Search { queries } if queries.is_empty() => "搜索资料".into(),
        WebAction::Search { queries } => format!("搜索 · {}", queries.join("；")),
        WebAction::OpenPage { url } => url
            .as_ref()
            .map_or_else(|| "打开网页".into(), |url| format!("打开网页 · {url}")),
        WebAction::FindInPage { url, pattern } => std::iter::once("页内查找")
            .chain(pattern.as_deref())
            .chain(url.as_deref())
            .collect::<Vec<_>>()
            .join(" · "),
    }
}

pub(crate) fn tool_output_header(status: &str, elapsed_ms: Option<u64>) -> String {
    let elapsed = elapsed_ms.map_or_else(String::new, |ms| {
        format!("\n耗时：约 {}.{:03} 秒", ms / 1000, ms % 1000)
    });
    format!("{status}{elapsed}\n\n输出：\n")
}
pub(crate) const PARTIAL_ANSWER_TITLE: &str = "部分回答";
pub(crate) const RUN_FAILED_TITLE: &str = "任务失败";
pub(crate) const RUN_INTERRUPTED_TITLE: &str = "任务已中断";
pub(crate) const WAITING_FOR_AGENT: &str = "正在等待 Agent 输出...";
pub(crate) const NATIVE_RETRY_ADVICE: &str = "建议：请重试；如果仍然失败，请查看 daemon 日志。";
pub(crate) const RUN_INTERRUPTED_BODY: &str =
    "Agora Node 即将退出，本次任务已中断，当前输出已保留。\nNode 恢复后，请重新发送消息继续。";
pub(crate) const STOP_TASK: &str = "结束任务";
pub(crate) const IMAGE_SENDING: &str = "正在发送图片附件…";
pub(crate) const IMAGE_SENT: &str = "图片附件已发送。";
pub(crate) const IMAGE_SEND_FAILED: &str = "图片未确认送达，请重新请求发送；文字回答已保留。";
pub(crate) const IMAGE_UNAVAILABLE: &str =
    "图片无法发送：文件不可读、超出允许范围，或格式/大小/数量不受支持。";
pub(crate) const IMAGE_REFERENCE: &str = "（图片见本话题的附件；发送状态见任务过程）";
pub(crate) const IMAGE_REFERENCE_FAILED: &str = "（图片无法发送，详见任务过程）";
pub(crate) const STOP_BUTTON_EXPIRED: &str =
    "任务已结束或按钮已失效；如需停止当前对话的任务，请发送 /stop。";
pub(crate) const OUTPUT_TRUNCATED: &str = "[输出已截断]\n\n";
pub(crate) const PERMISSION_DENIED_TITLE: &str = "无权访问此 Channel";
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
pub(crate) const NODE_BUSY: &str = "服务繁忙，请稍后重试。";

pub(crate) const STOP_COMMAND_DESCRIPTION: &str = "停止当前对话中正在运行或排队的 Agent 任务。";
pub(crate) const STOP_AGENT_ARGUMENT_DESCRIPTION: &str =
    "已配置的 Agent 名称；省略时停止全部 Agent。";
pub(crate) const RESET_COMMAND_DESCRIPTION: &str = "停止任务并重置后端 Agent 会话。";
pub(crate) const RESET_SUCCESSFUL: &str = "重置成功。";
pub(crate) const ASK_COMMAND_DESCRIPTION: &str = "向指定 Agent 提问。";
pub(crate) const AGENT_COMMAND_DESCRIPTION: &str = "管理 Agent 的消息接收状态。";
pub(crate) const AGENT_REPLY_TOO_LARGE: &str = "Agent 列表或回复超出卡片大小限制，无法展示完整列表和按钮。请使用 /agent status {agent_name} 查看单个状态，使用 /agent enable {agent_name} 或 /agent disable {agent_name} 管理消息接收。";
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

pub(crate) fn tool_call_count(count: usize) -> String {
    format!("已调用 {count} 次工具")
}

pub(crate) fn queued_message(ahead: usize) -> String {
    if ahead == 0 {
        "已接收，正在等待可用执行资源...".to_string()
    } else {
        format!("正在排队，前面还有 {ahead} 个任务...")
    }
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
    format!(
        "使用 /help {} {{子命令}} 查看详情。",
        command_path.trim_start_matches('/')
    )
}

pub(crate) fn root_command_details_hint() -> &'static str {
    "使用 /help {command} 查看详情。"
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
