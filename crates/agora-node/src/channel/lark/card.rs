use super::LarkReplyTarget;
use super::channel::LarkConversation;
use super::lark_api::{LarkApi, LarkHttpStatusError};
use crate::channel::permission::PermissionDenial;
use crate::channel::{
    ChannelAgentStatus, ChannelButton, ChannelButtonStyle, ChannelReply, ChannelRun,
    InterruptRegistration, RunEvent,
};
use crate::i18n::{self, RunStatus};
use crate::task::{OutputEvent, ProgressStatus, TokenUsage};
use agora_core::logger;
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const MAX_ANSWER_BYTES: usize = 20 * 1024;
const MAX_PROCESS_ELEMENTS: usize = 160;
const CARD_UPDATE_INTERVAL: Duration = Duration::from_millis(400);
const TENANT_TOKEN_CACHE_TTL: Duration = Duration::from_secs(50 * 60);

pub(super) struct LarkAgentCard {
    inner: Arc<LarkAgentCardInner>,
}

impl Clone for LarkAgentCard {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct LarkAgentCardInner {
    target: LarkReplyTarget,
    api: LarkApi,
    _interrupt: Option<InterruptRegistration>,
    state: Mutex<LarkAgentCardState>,
    flush: Mutex<()>,
}

struct LarkAgentCardState {
    token: Option<CachedToken>,
    message_id: Option<String>,
    content: LarkCardContent,
    version: u64,
    sent_version: u64,
    last_update: Option<Instant>,
    flush_scheduled: bool,
}

#[derive(Clone)]
struct CachedToken {
    value: String,
    expires_at: Instant,
}

pub(super) struct LarkCardContent {
    agent_name: String,
    interrupt: Option<String>,
    process: VecDeque<LarkProcessPhase>,
    answer: String,
    usage: Option<TokenUsage>,
    state: LarkRunState,
    conversation: LarkConversation,
}

struct LarkProcessPhase {
    thinking: Option<String>,
    progress: VecDeque<LarkProgressEntry>,
}

enum LarkRunState {
    Queued { ahead: usize },
    Running,
    Completed,
    Failed(String),
    Stopped,
    Interrupted,
}

struct LarkProgressEntry {
    id: String,
    text: String,
    status: ProgressStatus,
    kind: LarkProgressKind,
    exit_code: Option<i32>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LarkProgressKind {
    Message,
    Command,
}

pub(super) struct LarkReplyCard;

impl LarkReplyCard {
    pub(super) fn permission_denied(denial: &PermissionDenial) -> Value {
        Self::card(
            i18n::PERMISSION_DENIED_HEADER_TITLE,
            i18n::PERMISSION_DENIED_SUBTITLE.to_string(),
            vec![json!({
                "tag": "markdown",
                "content": Self::permission_denied_markdown(denial)
            })],
        )
    }

    fn permission_denied_markdown(denial: &PermissionDenial) -> String {
        let mut identifiers = vec![
            format!("- Channel：`{}`", denial.channel_name()),
            format!("- User ID：`{}`", denial.user_id()),
        ];
        if let Some(group_id) = denial.group_id() {
            identifiers.push(format!("- Group ID：`{group_id}`"));
        }
        let configuration = denial.configuration_example();
        format!(
            "**{}**\n\n> {}\n\n**{}**\n{}\n\n**{}**\n```jsonc\n{}\n```",
            i18n::PERMISSION_DENIED_TITLE,
            denial.reason(),
            i18n::PERMISSION_IDENTIFIERS_TITLE,
            identifiers.join("\n"),
            i18n::PERMISSION_CONFIG_EXAMPLE_TITLE,
            configuration
        )
    }

    pub(super) fn build(reply: &ChannelReply, conversation: LarkConversation) -> Value {
        match reply {
            ChannelReply::Text(text) => Self::card(
                i18n::AGENT_STATUS_TITLE,
                i18n::CURRENT_CONVERSATION.to_string(),
                vec![json!({
                    "tag": "markdown",
                    "content": text
                })],
            ),
            ChannelReply::AgentList(agents) => Self::agent_list(agents, conversation),
            ChannelReply::AgentStatus(agent) => Self::agent_status(agent),
        }
    }

    fn agent_list(agents: &[ChannelAgentStatus], conversation: LarkConversation) -> Value {
        let mut elements = Vec::new();
        for (index, agent) in agents.iter().enumerate() {
            if index > 0 {
                elements.push(json!({ "tag": "hr" }));
            }
            elements.push(Self::agent_row(agent, conversation));
        }
        if !elements.is_empty() {
            elements.push(json!({ "tag": "hr" }));
        }
        elements.push(json!({
            "tag": "markdown",
            "content": format!(
                "<font color='grey'>{}</font>",
                i18n::CURRENT_CONVERSATION_ONLY
            )
        }));
        Self::card(
            i18n::AGENT_STATUS_TITLE,
            i18n::agent_count(agents.len()),
            elements,
        )
    }

    fn agent_status(agent: &ChannelAgentStatus) -> Value {
        let (color, state, _) = Self::status_text(agent.enabled());
        Self::card(
            i18n::AGENT_STATUS_TITLE,
            i18n::CURRENT_CONVERSATION.to_string(),
            vec![json!({
                "tag": "column_set",
                "flex_mode": "none",
                "columns": [
                    {
                        "tag": "column",
                        "width": "weighted",
                        "weight": 1,
                        "vertical_align": "center",
                        "elements": [{
                            "tag": "markdown",
                            "content": format!(
                                "**{}**\n<font color='grey'>{}</font>",
                                agent.name(),
                                i18n::MESSAGE_DELIVERY_STATUS
                            )
                        }]
                    },
                    {
                        "tag": "column",
                        "width": "auto",
                        "vertical_align": "center",
                        "elements": [{
                            "tag": "markdown",
                            "content": format!("<font color='{color}'>● {state}</font>"),
                            "text_align": "right"
                        }]
                    }
                ]
            })],
        )
    }

    fn agent_row(agent: &ChannelAgentStatus, conversation: LarkConversation) -> Value {
        let (color, state, description) = Self::status_text(agent.enabled());
        let mut columns = vec![json!({
            "tag": "column",
            "width": "weighted",
            "weight": 1,
            "vertical_align": "center",
            "elements": [{
                "tag": "markdown",
                "content": format!(
                    "**{}**\n<font color='{color}'>{state}</font> · {description}",
                    agent.name()
                )
            }]
        })];
        if let Some(button) = agent.button() {
            columns.push(json!({
                "tag": "column",
                "width": "auto",
                "vertical_align": "center",
                "elements": [Self::button(button, conversation)]
            }));
        }
        json!({
            "tag": "column_set",
            "flex_mode": "none",
            "horizontal_spacing": "default",
            "columns": columns
        })
    }

    fn button(button: &ChannelButton, conversation: LarkConversation) -> Value {
        let button_type = match button.style() {
            ChannelButtonStyle::Default => "default",
            ChannelButtonStyle::Primary => "primary",
            ChannelButtonStyle::Danger => "danger",
        };
        json!({
            "tag": "button",
            "text": {
                "tag": "plain_text",
                "content": button.text()
            },
            "type": button_type,
            "size": "medium",
            "behaviors": [{
                "type": "callback",
                "value": {
                    "agora_command": button.command(),
                    "agora_conversation": conversation.as_str()
                }
            }]
        })
    }

    fn status_text(enabled: bool) -> (&'static str, &'static str, &'static str) {
        if enabled {
            (
                "green",
                i18n::AGENT_ENABLED,
                i18n::AGENT_ENABLED_DESCRIPTION,
            )
        } else {
            (
                "grey",
                i18n::AGENT_DISABLED,
                i18n::AGENT_DISABLED_DESCRIPTION,
            )
        }
    }

    fn card(title: &str, subtitle: String, elements: Vec<Value>) -> Value {
        json!({
            "schema": "2.0",
            "config": {
                "update_multi": true,
                "summary": {
                    "content": title
                }
            },
            "header": {
                "template": "blue",
                "title": {
                    "tag": "plain_text",
                    "content": title
                },
                "subtitle": {
                    "tag": "plain_text",
                    "content": subtitle
                }
            },
            "body": {
                "elements": elements
            }
        })
    }
}

impl LarkCardContent {
    pub(super) fn new(agent_name: String) -> Self {
        Self {
            agent_name,
            interrupt: None,
            process: VecDeque::new(),
            answer: String::new(),
            usage: None,
            state: LarkRunState::Running,
            conversation: LarkConversation::Private,
        }
    }

    pub(super) fn with_interrupt(
        agent_name: String,
        interrupt: Option<String>,
        conversation: LarkConversation,
    ) -> Self {
        Self {
            interrupt,
            conversation,
            ..Self::new(agent_name)
        }
    }

    pub(super) fn apply_output(&mut self, event: OutputEvent) {
        match event {
            OutputEvent::Thinking { text } => {
                if !text.trim().is_empty() {
                    self.process.push_front(LarkProcessPhase {
                        thinking: Some(text),
                        progress: VecDeque::new(),
                    });
                }
            }
            OutputEvent::Progress { id, text, status } => {
                self.apply_progress(id, text, status, LarkProgressKind::Message, None);
            }
            OutputEvent::CommandExecution {
                id,
                command,
                status,
                exit_code,
            } => {
                self.apply_progress(id, command, status, LarkProgressKind::Command, exit_code);
            }
            OutputEvent::Answer { text } => self.answer.push_str(&text),
            OutputEvent::Usage(usage) => self.usage = Some(usage),
        }
    }

    fn apply_progress(
        &mut self,
        id: String,
        text: String,
        status: ProgressStatus,
        kind: LarkProgressKind,
        exit_code: Option<i32>,
    ) {
        for phase in &mut self.process {
            if let Some(index) = phase.progress.iter().position(|entry| entry.id == id) {
                phase.progress.remove(index);
                phase.progress.push_front(LarkProgressEntry {
                    id,
                    text,
                    status,
                    kind,
                    exit_code,
                });
                return;
            }
        }
        if self.process.is_empty() {
            self.process.push_front(LarkProcessPhase {
                thinking: None,
                progress: VecDeque::new(),
            });
        }
        if let Some(phase) = self.process.front_mut() {
            phase.progress.push_front(LarkProgressEntry {
                id,
                text,
                status,
                kind,
                exit_code,
            });
        }
    }

    pub(super) fn complete(&mut self) {
        self.state = LarkRunState::Completed;
    }

    pub(super) fn queue(&mut self, ahead: usize) {
        self.state = LarkRunState::Queued { ahead };
    }

    pub(super) fn start(&mut self) {
        self.state = LarkRunState::Running;
    }

    pub(super) fn fail(&mut self, message: String) {
        self.state = LarkRunState::Failed(message);
    }

    pub(super) fn stop(&mut self) {
        for phase in &mut self.process {
            for entry in &mut phase.progress {
                if entry.status == ProgressStatus::Running {
                    entry.status = ProgressStatus::Stopped;
                }
            }
        }
        self.state = LarkRunState::Stopped;
    }

    pub(super) fn interrupt(&mut self) {
        for phase in &mut self.process {
            for entry in &mut phase.progress {
                if entry.status == ProgressStatus::Running {
                    entry.status = ProgressStatus::Stopped;
                }
            }
        }
        self.state = LarkRunState::Interrupted;
    }

    pub(super) fn build_card(&self) -> Value {
        let (template, status, status_color) = match &self.state {
            LarkRunState::Queued { .. } => ("grey", i18n::run_status(RunStatus::Queued), "grey"),
            LarkRunState::Running => ("blue", i18n::run_status(RunStatus::Running), "blue"),
            LarkRunState::Completed => ("green", i18n::run_status(RunStatus::Completed), "green"),
            LarkRunState::Failed(_) => ("red", i18n::run_status(RunStatus::Failed), "red"),
            LarkRunState::Stopped => ("grey", i18n::run_status(RunStatus::Stopped), "grey"),
            LarkRunState::Interrupted => {
                ("orange", i18n::run_status(RunStatus::Interrupted), "orange")
            }
        };
        let failure_view = match &self.state {
            LarkRunState::Failed(message) => Some(Self::failure_view(message)),
            _ => None,
        };
        let finished = !matches!(
            &self.state,
            LarkRunState::Queued { .. } | LarkRunState::Running
        );
        let mut elements = Vec::new();
        let mut process_element = if self.process.is_empty() {
            None
        } else {
            let summary = self.progress_summary();
            let status = if summary.is_empty() {
                String::new()
            } else {
                format!(" · {summary}")
            };
            Some(Self::collapsible_panel_elements(
                format!(
                    "**{}**  <font color='grey'>· {}</font>{status}",
                    i18n::PROCESS_TITLE,
                    i18n::phase_count(self.process.len())
                ),
                !finished,
                self.process_elements(),
            ))
        };
        if !finished && let Some(process) = process_element.take() {
            elements.push(process);
        }

        if let Some((category, summary)) = failure_view {
            if !elements.is_empty() {
                elements.push(json!({ "tag": "hr" }));
            }
            elements.push(json!({
                "tag": "markdown",
                "content": format!(
                    "<font color='red'>▌</font> **{}**\n{}\n\n<font color='grey'>{summary}</font>\n<font color='grey'>{}</font>",
                    i18n::RUN_FAILED_TITLE,
                    i18n::AGENT_RUN_FAILED,
                    i18n::RETRY_ADVICE
                )
            }));
            elements.push(Self::collapsible_panel(
                format!(
                    "**{}**  <font color='grey'>· {category}</font>",
                    i18n::TECHNICAL_DETAILS_TITLE
                ),
                false,
                i18n::ERROR_WRITTEN_TO_LOG.to_string(),
            ));
        }

        if matches!(&self.state, LarkRunState::Stopped) {
            if !elements.is_empty() {
                elements.push(json!({ "tag": "hr" }));
            }
            elements.push(json!({
                "tag": "markdown",
                "content": format!(
                    "<font color='grey'>▌</font> **{}**\n{}",
                    i18n::RUN_STOPPED_TITLE,
                    i18n::RUN_STOPPED_BODY
                )
            }));
        }

        if matches!(&self.state, LarkRunState::Interrupted) {
            if !elements.is_empty() {
                elements.push(json!({ "tag": "hr" }));
            }
            elements.push(json!({
                "tag": "markdown",
                "content": format!(
                    "<font color='orange'>▌</font> **{}**\n{}",
                    i18n::RUN_INTERRUPTED_TITLE,
                    i18n::RUN_INTERRUPTED_BODY
                )
            }));
        }

        if !self.answer.is_empty() {
            if !elements.is_empty() {
                elements.push(json!({ "tag": "hr" }));
            }
            let title = if matches!(
                &self.state,
                LarkRunState::Failed(_) | LarkRunState::Stopped | LarkRunState::Interrupted
            ) {
                i18n::PARTIAL_ANSWER_TITLE
            } else {
                i18n::FINAL_ANSWER_TITLE
            };
            elements.push(json!({
                "tag": "markdown",
                "content": format!(
                    "<font color='blue'>▌</font> **{title}**\n{}",
                    Self::truncate_answer(&self.answer)
                )
            }));
        }

        if finished && let Some(process) = process_element {
            if !elements.is_empty() {
                elements.push(json!({ "tag": "hr" }));
            }
            elements.push(process);
        }

        if finished && let Some(usage) = self.usage {
            if !elements.is_empty() {
                elements.push(json!({ "tag": "hr" }));
            }
            elements.push(Self::usage_element(usage));
        }

        if elements.is_empty() && !finished {
            elements.push(json!({
                "tag": "markdown",
                "content": match &self.state {
                    LarkRunState::Queued { ahead } => {
                        format!("> {}", i18n::queued_message(*ahead))
                    }
                    _ => format!("> {}", i18n::WAITING_FOR_AGENT),
                }
            }));
        }

        if !finished && self.interrupt.is_some() {
            if !elements.is_empty() {
                elements.push(json!({ "tag": "hr" }));
            }
            if let Some(action_row) = self.action_row() {
                elements.push(action_row);
            }
        }

        let mut card = json!({
            "schema": "2.0",
            "config": {
                "update_multi": true,
                "summary": {
                    "content": format!("{}: {}", self.agent_name, status)
                },
                "style": {
                    "color": {
                        "cus-0": {
                            "light_mode": "rgba(230, 233, 238, 1)",
                            "dark_mode": "rgba(45, 48, 54, 1)"
                        }
                    }
                }
            },
            "header": {
                "template": template,
                "title": {
                    "tag": "plain_text",
                    "content": self.agent_name
                },
                "text_tag_list": [{
                    "tag": "text_tag",
                    "text": {
                        "tag": "plain_text",
                        "content": status
                    },
                    "color": status_color
                }]
            }
        });
        if !elements.is_empty() {
            card["body"] = json!({ "elements": elements });
        }
        card
    }

    fn action_row(&self) -> Option<Value> {
        let interrupt = self.interrupt.as_ref()?;
        Some(json!({
            "tag": "column_set",
            "flex_mode": "none",
            "horizontal_align": "right",
            "columns": [json!({
                "tag": "column",
                "width": "auto",
                "elements": [{
                    "tag": "button",
                    "text": {
                        "tag": "plain_text",
                        "content": i18n::STOP_TASK
                    },
                    "type": "danger",
                    "size": "medium",
                    "behaviors": [{
                        "type": "callback",
                        "value": {
                            "agora_interrupt": interrupt,
                            "agora_conversation": self.conversation.as_str()
                        }
                    }]
                }]
            })]
        }))
    }

    fn collapsible_panel(title: String, expanded: bool, content: String) -> Value {
        Self::collapsible_panel_elements(
            title,
            expanded,
            vec![json!({
                "tag": "markdown",
                "content": content
            })],
        )
    }

    fn collapsible_panel_elements(title: String, expanded: bool, elements: Vec<Value>) -> Value {
        json!({
            "tag": "collapsible_panel",
            "expanded": expanded,
            "background_color": "grey-50",
            "header": {
                "title": {
                    "tag": "markdown",
                    "content": title
                },
                "vertical_align": "center",
                "padding": "8px 12px 8px 12px",
                "icon": {
                    "tag": "standard_icon",
                    "token": "down-small-ccm_outlined",
                    "size": "16px 16px"
                },
                "icon_position": "right",
                "icon_expanded_angle": -180
            },
            "border": {
                "color": "grey-200",
                "corner_radius": "8px"
            },
            "vertical_spacing": "6px",
            "padding": "2px 12px 10px 12px",
            "elements": elements
        })
    }

    fn progress_summary(&self) -> String {
        let completed = self
            .progress_entries()
            .filter(|entry| entry.status == ProgressStatus::Completed)
            .count();
        let running = self
            .progress_entries()
            .filter(|entry| entry.status == ProgressStatus::Running)
            .count();
        let failed = self
            .progress_entries()
            .filter(|entry| entry.status == ProgressStatus::Failed)
            .count();
        let stopped = self
            .progress_entries()
            .filter(|entry| entry.status == ProgressStatus::Stopped)
            .count();

        let mut parts = Vec::new();
        if completed > 0 {
            parts.push(format!(
                "<font color='green'>✓</font> <font color='grey'>{}</font>",
                i18n::progress_count(ProgressStatus::Completed, completed)
            ));
        }
        if running > 0 {
            parts.push(format!(
                "<font color='blue'>●</font> <font color='grey'>{}</font>",
                i18n::progress_count(ProgressStatus::Running, running)
            ));
        }
        if failed > 0 {
            parts.push(format!(
                "<font color='red'>×</font> <font color='grey'>{}</font>",
                i18n::progress_count(ProgressStatus::Failed, failed)
            ));
        }
        if stopped > 0 {
            parts.push(format!(
                "<font color='grey'>■</font> <font color='grey'>{}</font>",
                i18n::progress_count(ProgressStatus::Stopped, stopped)
            ));
        }
        parts.join(" · ")
    }

    fn process_elements(&self) -> Vec<Value> {
        let mut phases = self
            .process
            .iter()
            .rev()
            .enumerate()
            .map(|(index, phase)| Self::phase_elements(phase, index + 1))
            .filter(|phase| !phase.is_empty())
            .collect::<VecDeque<_>>();
        let mut element_count = Self::process_element_count(&phases);
        let omitted = element_count > MAX_PROCESS_ELEMENTS;
        let mut omitted_phases = 0;

        if omitted {
            let available = MAX_PROCESS_ELEMENTS.saturating_sub(1);
            while element_count > available {
                if phases.len() > 1 {
                    let removed = phases.pop_front().unwrap_or_default();
                    omitted_phases += 1;
                    element_count = element_count.saturating_sub(
                        removed
                            .iter()
                            .map(Self::tagged_element_count)
                            .sum::<usize>()
                            + 1,
                    );
                } else if let Some(phase) = phases.front_mut() {
                    if let Some(removed) = phase.pop() {
                        element_count =
                            element_count.saturating_sub(Self::tagged_element_count(&removed));
                    } else {
                        phases.clear();
                        element_count = 0;
                    }
                } else {
                    break;
                }
            }
        }

        let mut elements = Vec::new();
        if omitted {
            elements.push(json!({
                "tag": "markdown",
                "content": format!(
                    "<font color='grey'>{}</font>",
                    i18n::truncated_phase_count(omitted_phases)
                )
            }));
        }
        for (index, phase) in phases.into_iter().enumerate() {
            if index > 0 {
                elements.push(json!({ "tag": "hr" }));
            }
            elements.extend(phase);
        }
        elements
    }

    fn process_element_count(phases: &VecDeque<Vec<Value>>) -> usize {
        phases
            .iter()
            .flatten()
            .map(Self::tagged_element_count)
            .sum::<usize>()
            + phases.len().saturating_sub(1)
    }

    fn tagged_element_count(value: &Value) -> usize {
        match value {
            Value::Array(values) => values.iter().map(Self::tagged_element_count).sum(),
            Value::Object(values) => {
                usize::from(values.contains_key("tag"))
                    + values
                        .values()
                        .map(Self::tagged_element_count)
                        .sum::<usize>()
            }
            _ => 0,
        }
    }

    fn phase_elements(phase: &LarkProcessPhase, phase_number: usize) -> Vec<Value> {
        let mut elements = Vec::new();
        if let Some(thinking) = &phase.thinking {
            let thinking = thinking
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            if !thinking.is_empty() {
                elements.push(json!({
                    "tag": "markdown",
                    "content": format!(
                        "<font color='blue'>**{phase_number:02}**</font>  **{}**\n<font color='blue'>✦</font> {thinking}",
                        i18n::THINKING_TITLE
                    )
                }));
            }
        }

        for command in phase
            .progress
            .iter()
            .filter(|entry| entry.kind == LarkProgressKind::Command)
        {
            elements.push(Self::terminal_element(command));
        }

        let progress = phase
            .progress
            .iter()
            .filter(|entry| entry.kind == LarkProgressKind::Message)
            .map(|entry| {
                let marker = Self::progress_marker(entry.status);
                format!("{marker}  {}", entry.text)
            })
            .collect::<Vec<_>>();
        if !progress.is_empty() {
            elements.push(json!({
                "tag": "markdown",
                "content": progress.join("\n")
            }));
        }

        elements
    }

    fn terminal_element(entry: &LarkProgressEntry) -> Value {
        let (marker, color, status) = match entry.status {
            ProgressStatus::Running => ("●", "blue", i18n::SHELL_RUNNING),
            ProgressStatus::Completed => ("✓", "green", i18n::SHELL_COMPLETED),
            ProgressStatus::Failed => ("×", "red", i18n::SHELL_FAILED),
            ProgressStatus::Stopped => ("■", "grey", i18n::SHELL_STOPPED),
        };
        let status = entry.exit_code.map_or_else(
            || status.to_string(),
            |exit_code| format!("exit {exit_code}"),
        );
        json!({
            "tag": "column_set",
            "flex_mode": "none",
            "background_style": "cus-0",
            "margin": "4px 0",
            "columns": [{
                "tag": "column",
                "width": "weighted",
                "weight": 1,
                "vertical_align": "top",
                "vertical_spacing": "2px",
                "padding": "6px 8px 8px 8px",
                "elements": [
                    {
                        "tag": "column_set",
                        "flex_mode": "none",
                        "columns": [
                            {
                                "tag": "column",
                                "width": "weighted",
                                "weight": 1,
                                "vertical_align": "center",
                                "elements": [{
                                    "tag": "markdown",
                                    "content": format!("**{}**", i18n::SHELL_TITLE),
                                    "text_size": "notation"
                                }]
                            },
                            {
                                "tag": "column",
                                "width": "auto",
                                "vertical_align": "center",
                                "elements": [{
                                    "tag": "markdown",
                                    "content": format!(
                                        "<font color='{color}'>{marker}  {status}</font>"
                                    ),
                                    "text_size": "notation",
                                    "text_align": "right"
                                }]
                            }
                        ]
                    },
                    {
                        "tag": "markdown",
                        "content": format!("```bash\n$ {}\n```", entry.text),
                        "text_size": "normal"
                    }
                ]
            }]
        })
    }

    fn progress_entries(&self) -> impl Iterator<Item = &LarkProgressEntry> {
        self.process.iter().flat_map(|phase| phase.progress.iter())
    }

    fn progress_marker(status: ProgressStatus) -> &'static str {
        match status {
            ProgressStatus::Running => "<font color='blue'>●</font>",
            ProgressStatus::Completed => "<font color='green'>✓</font>",
            ProgressStatus::Failed => "<font color='red'>×</font>",
            ProgressStatus::Stopped => "<font color='grey'>■</font>",
        }
    }

    fn failure_view(message: &str) -> (&'static str, &'static str) {
        let copy = i18n::failure_copy(message);
        (copy.category, copy.summary)
    }

    fn usage_element(usage: TokenUsage) -> Value {
        let total_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
        json!({
            "tag": "column_set",
            "flex_mode": "none",
            "horizontal_spacing": "small",
            "horizontal_align": "left",
            "columns": [
                Self::usage_column(i18n::TOTAL, total_tokens, i18n::TOKENS),
                Self::usage_column(
                    i18n::INPUT,
                    usage.input_tokens,
                    &i18n::cached_tokens(Self::format_tokens(usage.cached_input_tokens)),
                ),
                Self::usage_column(i18n::OUTPUT, usage.output_tokens, i18n::TOKENS),
                Self::usage_column(
                    i18n::REASONING,
                    usage.reasoning_output_tokens,
                    i18n::REASONING_DETAIL,
                ),
            ]
        })
    }

    fn usage_column(label: &str, tokens: u64, detail: &str) -> Value {
        json!({
            "tag": "column",
            "width": "weighted",
            "weight": 1,
            "vertical_align": "top",
            "vertical_spacing": "0px",
            "elements": [{
                "tag": "markdown",
                "content": format!(
                    "<font color='grey'>{label}</font>\n**{}**\n<font color='grey'>{detail}</font>",
                    Self::format_tokens(tokens),
                ),
                "text_align": "center",
                "text_size": "notation",
            }]
        })
    }

    fn format_tokens(tokens: u64) -> String {
        if tokens < 1_000 {
            tokens.to_string()
        } else if tokens < 1_000_000 {
            format!("{:.1}K", tokens as f64 / 1_000.0)
        } else {
            format!("{:.1}M", tokens as f64 / 1_000_000.0)
        }
    }

    fn truncate_answer(answer: &str) -> String {
        if answer.len() <= MAX_ANSWER_BYTES {
            return answer.to_string();
        }
        let marker = i18n::OUTPUT_TRUNCATED;
        let budget = MAX_ANSWER_BYTES.saturating_sub(marker.len());
        let mut start = answer.len().saturating_sub(budget);
        while !answer.is_char_boundary(start) {
            start += 1;
        }
        format!("{}{}", marker, &answer[start..])
    }
}

impl LarkAgentCard {
    pub(super) fn new(
        target: LarkReplyTarget,
        agent_name: String,
        interrupt: Option<InterruptRegistration>,
        conversation: LarkConversation,
        api: LarkApi,
    ) -> Self {
        let interrupt_id = interrupt
            .as_ref()
            .map(InterruptRegistration::id)
            .map(str::to_string);
        Self {
            inner: Arc::new(LarkAgentCardInner {
                target,
                api,
                _interrupt: interrupt,
                state: Mutex::new(LarkAgentCardState {
                    token: None,
                    message_id: None,
                    content: LarkCardContent::with_interrupt(
                        agent_name,
                        interrupt_id,
                        conversation,
                    ),
                    version: 0,
                    sent_version: 0,
                    last_update: None,
                    flush_scheduled: false,
                }),
                flush: Mutex::new(()),
            }),
        }
    }

    async fn publish_event(&self, event: RunEvent) -> Result<()> {
        let flush_now = {
            let mut state = self.inner.state.lock().await;
            let flush_now = match event {
                RunEvent::Queued { ahead } => {
                    state.content.queue(ahead);
                    true
                }
                RunEvent::Started { .. } => {
                    state.content.start();
                    true
                }
                RunEvent::Output(event) => {
                    state.content.apply_output(event);
                    false
                }
                RunEvent::Completed { .. } => {
                    state.content.complete();
                    true
                }
                RunEvent::Failed { message } => {
                    state.content.fail(message);
                    true
                }
                RunEvent::Stopped => {
                    state.content.stop();
                    true
                }
                RunEvent::Interrupted => {
                    state.content.interrupt();
                    true
                }
            };
            state.version = state.version.saturating_add(1);
            if !flush_now && !state.flush_scheduled {
                state.flush_scheduled = true;
                let delay = state
                    .last_update
                    .map(|last_update| CARD_UPDATE_INTERVAL.saturating_sub(last_update.elapsed()))
                    .unwrap_or_default();
                self.schedule_flush(delay);
            }
            flush_now
        };

        if flush_now {
            self.flush_latest().await
        } else {
            Ok(())
        }
    }

    fn schedule_flush(&self, delay: Duration) {
        let card = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            {
                let mut state = card.inner.state.lock().await;
                state.flush_scheduled = false;
            }
            if let Err(err) = card.flush_latest().await {
                logger::error!(
                    "lark card update failed source_message_id={} error={}",
                    card.inner.target.message_id,
                    err
                );
            }
        });
    }

    async fn flush_latest(&self) -> Result<()> {
        let _flush = self.inner.flush.lock().await;
        let (message_id, card, version) = {
            let state = self.inner.state.lock().await;
            if state.version == state.sent_version {
                return Ok(());
            }
            (
                state.message_id.clone(),
                state.content.build_card(),
                state.version,
            )
        };

        let mut refreshed = false;
        let published_message_id = loop {
            let token = self.token().await?;
            let result = if let Some(message_id) = message_id.as_deref() {
                self.inner
                    .api
                    .patch_card(&token, message_id, &card)
                    .await
                    .map(|()| None)
            } else {
                self.inner
                    .api
                    .reply_card(&token, &self.inner.target, &card)
                    .await
                    .map(Some)
            };
            match result {
                Err(error)
                    if !refreshed
                        && error
                            .downcast_ref::<LarkHttpStatusError>()
                            .is_some_and(LarkHttpStatusError::is_unauthorized) =>
                {
                    self.invalidate_token(&token).await;
                    refreshed = true;
                }
                result => break result?,
            }
        };

        let mut state = self.inner.state.lock().await;
        if state.message_id.is_none() {
            state.message_id = published_message_id;
        }
        state.sent_version = state.sent_version.max(version);
        state.last_update = Some(Instant::now());
        Ok(())
    }

    async fn token(&self) -> Result<String> {
        {
            let state = self.inner.state.lock().await;
            if let Some(token) = &state.token
                && token.expires_at > Instant::now()
            {
                return Ok(token.value.clone());
            }
        }
        let value = self.inner.api.tenant_access_token().await?;
        let mut state = self.inner.state.lock().await;
        state.token = Some(CachedToken {
            value: value.clone(),
            expires_at: Instant::now() + TENANT_TOKEN_CACHE_TTL,
        });
        Ok(value)
    }

    async fn invalidate_token(&self, value: &str) {
        let mut state = self.inner.state.lock().await;
        if state
            .token
            .as_ref()
            .is_some_and(|token| token.value == value)
        {
            state.token = None;
        }
    }
}

impl ChannelRun for LarkAgentCard {
    async fn publish(&self, event: RunEvent) -> Result<()> {
        self.publish_event(event).await
    }
}

#[cfg(test)]
mod tests;
