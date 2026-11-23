use crate::channel::{RunEvent, append_bounded_tail, bounded_tail, command_output_preview};
use crate::i18n;
use crate::task::{OutputEvent, ProgressStatus};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_TOOLS: usize = 64;
const MAX_TEXT_BYTES: usize = 8192;
const MAX_ANSWER_BYTES: usize = 20 * 1024;
const RESULT_EVENT_BYTES: usize = 4096;
const DELTA_CHUNK_BYTES: usize = 600;
// Reserve 100 events for all outstanding tool results, truncation, and closure.
const NORMAL_EVENT_LIMIT: usize = 800;
const NORMAL_TYPE_LIMIT: usize = 380;

pub(super) struct Content {
    pub pending: VecDeque<Value>,
    pub terminal: Option<Terminal>,
    pub answer: String,
    run_id: String,
    thread_id: String,
    agent: String,
    started: bool,
    queued: Option<usize>,
    tools: HashMap<String, Tool>,
    texts: VecDeque<(String, String)>,
    event_count: usize,
    type_counts: HashMap<&'static str, usize>,
    next_id: u64,
    timestamp: u128,
    truncated: bool,
}

struct Tool {
    wire_id: String,
    title: String,
    command: Option<String>,
    summary: Option<String>,
    output: Option<(String, Option<u64>)>,
    result: Option<String>,
    result_pending: bool,
}

pub(super) enum Terminal {
    Completed,
    Failed(String),
    Stopped,
    Interrupted,
}

impl Content {
    pub fn new(agent: String, thread_id: String) -> Self {
        let mut content = Self {
            pending: VecDeque::new(),
            terminal: None,
            answer: String::new(),
            run_id: uuid::Uuid::new_v4().to_string(),
            thread_id,
            agent: bounded_tail(agent, 128, "…").0,
            started: false,
            queued: None,
            tools: HashMap::new(),
            texts: VecDeque::new(),
            event_count: 0,
            type_counts: HashMap::new(),
            next_id: 0,
            timestamp: 0,
            truncated: false,
        };
        content.emit(vec![("RUN_STARTED", json!({
            "threadId": content.thread_id, "runId": content.run_id,
            "input": {"statusText": {
                "running": {"zh_cn":i18n::run_status(i18n::RunStatus::Running)}, "thinking":{"zh_cn":i18n::THINKING_STATUS},
                "done":{"zh_cn":i18n::LARK_COMPLETED}, "error":{"zh_cn":i18n::run_status(i18n::RunStatus::Failed)},
                "interrupted":{"zh_cn":i18n::run_status(i18n::RunStatus::Stopped)}
            }}
        }))], false);
        content
    }

    /// Returns whether the event participates in publishing a Lark reply.
    pub fn apply(&mut self, event: RunEvent) -> bool {
        if self.terminal.is_some() {
            return false;
        }
        match event {
            RunEvent::Queued { ahead } => {
                if self.queued != Some(ahead) {
                    self.queued = Some(ahead);
                    self.text(
                        "queue".into(),
                        format!("{}：{}", self.agent, i18n::queued_message(ahead)),
                    );
                }
            }
            RunEvent::Started { .. } => {
                if !self.started {
                    self.started = true;
                    self.text("started".into(), i18n::WAITING_FOR_AGENT.into());
                }
            }
            RunEvent::Output(
                OutputEvent::Thinking { .. }
                | OutputEvent::Diagnostic { .. }
                | OutputEvent::Usage(_),
            ) => return false,
            RunEvent::Output(
                OutputEvent::Commentary { id, text } | OutputEvent::Progress { id, text, .. },
            ) => {
                self.text(id, text);
            }
            RunEvent::Output(OutputEvent::ToolCall {
                id,
                text,
                status,
                web_action,
            }) => {
                let text = web_action
                    .as_ref()
                    .map(i18n::web_tool_title)
                    .unwrap_or(text);
                self.tool(id, text, status, None, false)
            }
            RunEvent::Output(OutputEvent::CommandExecution {
                id,
                command,
                status,
                exit_code,
            }) => self.tool(id, command, status, exit_code, true),
            RunEvent::Output(OutputEvent::ToolOutput {
                id,
                text,
                elapsed_ms,
            }) => {
                if let Some(tool) = self.tools.get_mut(&id) {
                    let text = if tool.command.is_some() {
                        command_output_preview(text)
                    } else {
                        let (text, clipped) =
                            bounded_tail(text, MAX_TEXT_BYTES, i18n::OUTPUT_TRUNCATED);
                        self.truncated |= clipped;
                        text
                    };
                    let elapsed_ms =
                        elapsed_ms.or_else(|| tool.output.as_ref().and_then(|(_, ms)| *ms));
                    tool.output = Some((text, elapsed_ms));
                    self.publish_tool_result(&id);
                }
            }
            RunEvent::Output(OutputEvent::Answer { text }) => {
                let text = text.replace(i18n::IMAGE_REFERENCE, i18n::LARK_IMAGE_REFERENCE);
                append_bounded_tail(
                    &mut self.answer,
                    &text,
                    MAX_ANSWER_BYTES,
                    i18n::OUTPUT_TRUNCATED,
                );
            }
            RunEvent::Output(OutputEvent::Image { id, .. }) => {
                self.text(id, i18n::IMAGE_SENDING.into())
            }
            RunEvent::Completed { .. } => self.finish(Terminal::Completed),
            RunEvent::Failed { message } => self.finish(Terminal::Failed(
                i18n::failure_copy(&message).summary.into(),
            )),
            RunEvent::Stopped => self.finish(Terminal::Stopped),
            RunEvent::Interrupted => self.finish(Terminal::Interrupted),
        }
        true
    }

    pub fn success(&self) -> bool {
        matches!(self.terminal, Some(Terminal::Completed))
    }

    pub fn final_text(&self) -> String {
        if matches!(self.terminal, Some(Terminal::Stopped)) {
            return String::new();
        }
        let notice = match &self.terminal {
            Some(Terminal::Failed(summary)) => format!(
                "**{}**\n{}\n\n{}",
                i18n::RUN_FAILED_TITLE,
                summary,
                i18n::NATIVE_RETRY_ADVICE
            ),
            Some(Terminal::Interrupted) => format!(
                "**{}**\n{}",
                i18n::RUN_INTERRUPTED_TITLE,
                i18n::RUN_INTERRUPTED_BODY
            ),
            _ => String::new(),
        };
        if notice.is_empty() {
            if self.answer.is_empty() && self.terminal.is_some() {
                i18n::TASK_COMPLETED.into()
            } else {
                self.answer.clone()
            }
        } else if self.answer.is_empty() {
            notice
        } else {
            format!(
                "{notice}\n\n**{}**\n{}",
                i18n::PARTIAL_ANSWER_TITLE,
                self.answer
            )
        }
    }

    fn id(&mut self) -> String {
        self.next_id += 1;
        format!("{}-{}", self.run_id, self.next_id)
    }

    fn text(&mut self, id: String, text: String) {
        if text.trim().is_empty() {
            return;
        }
        if id.len() > 512 {
            self.truncated = true;
            return;
        }
        let (text, clipped) = bounded_tail(text, MAX_TEXT_BYTES, i18n::OUTPUT_TRUNCATED);
        self.truncated |= clipped;
        if self
            .texts
            .iter()
            .any(|(key, value)| key == &id && value == &text)
        {
            return;
        }
        let wire_id = self.id();
        let events = text_events(&wire_id, &text);
        if self.emit(events, false) {
            if self.texts.len() == 64 {
                self.texts.pop_front();
            }
            self.texts.push_back((id, text));
        }
    }

    fn tool(
        &mut self,
        id: String,
        text: String,
        status: ProgressStatus,
        exit: Option<i32>,
        shell: bool,
    ) {
        if id.len() > 512 {
            self.truncated = true;
            return;
        }
        let title = i18n::tool_title(&if shell {
            format!("{} · {text}", i18n::TOOL_COMMAND)
        } else {
            text.clone()
        });
        // The arguments carry the input. Only retain a second full copy for
        // the result panel if it could fit in a single native result event.
        let command = shell.then(|| {
            if text.len() <= RESULT_EVENT_BYTES {
                text.clone()
            } else {
                abbreviated_command(&text, 256)
            }
        });
        if !self.tools.contains_key(&id) {
            if self.tools.len() == MAX_TOOLS {
                self.truncated = true;
                return;
            }
            let wire_id = self.id();
            let mut events = vec![(
                "TOOL_CALL_START",
                json!({"toolCallId":wire_id,"toolCallName":if shell {"shell"} else {"tool"},"title":title,"icon":if shell {"bash"} else {"default"}}),
            )];
            for chunk in self.argument_chunks(text, shell) {
                events.push((
                    "TOOL_CALL_ARGS",
                    json!({"toolCallId":wire_id,"delta":chunk}),
                ));
            }
            events.push(("TOOL_CALL_END", json!({"toolCallId":wire_id})));
            if !self.emit(events, false) {
                return;
            }
            self.tools.insert(
                id.clone(),
                Tool {
                    wire_id,
                    title: title.clone(),
                    command,
                    summary: None,
                    output: None,
                    result: None,
                    result_pending: false,
                },
            );
        } else {
            self.tools.get_mut(&id).unwrap().command = command;
            if self.tools[&id].title != title {
                let tool = &self.tools[&id];
                // CHUNK updates metadata but reopens the native argument stream.
                // Close it atomically, then restore any known terminal result.
                if self.emit(vec![
                    ("TOOL_CALL_CHUNK", json!({
                        "toolCallId":tool.wire_id, "toolCallName":if tool.command.is_some() {"shell"} else {"tool"},
                        "title":title, "delta":"",
                    })),
                    ("TOOL_CALL_END", json!({"toolCallId":tool.wire_id})),
                ], false) {
                    let tool = self.tools.get_mut(&id).unwrap();
                    tool.title = title.clone();
                    tool.result_pending |= tool.result.is_some();
                }
            }
        }
        if status == ProgressStatus::Running {
            self.publish_tool_result(&id);
            return;
        }
        let result = if shell {
            i18n::shell_status(status, exit)
        } else {
            match status {
                ProgressStatus::Completed => i18n::TOOL_COMPLETED,
                ProgressStatus::Failed => i18n::TOOL_FAILED,
                _ => i18n::TOOL_STOPPED,
            }
            .into()
        };
        // Preserve a changed failure detail when the event budget prevented its title update.
        let result =
            if !shell && status != ProgressStatus::Completed && self.tools[&id].title != title {
                format!("{result}\n{title}")
            } else {
                result
            };
        self.tools.get_mut(&id).unwrap().summary = Some(result);
        self.publish_tool_result(&id);
    }

    fn argument_chunks(&mut self, text: String, shell: bool) -> Vec<String> {
        if shell {
            // START and END share the run budget with the argument chunks.
            let available = NORMAL_TYPE_LIMIT
                .saturating_sub(*self.type_counts.get("TOOL_CALL_ARGS").unwrap_or(&0))
                .min(NORMAL_EVENT_LIMIT.saturating_sub(self.event_count + 2));
            if text.len() <= available * DELTA_CHUNK_BYTES {
                let args = chunks(&json!({"command":text}).to_string());
                if args.len() <= available {
                    return args;
                }
            }
            self.truncated = true;
            let prefix = abbreviated_command(&text, MAX_TEXT_BYTES - i18n::COMMAND_TRUNCATED.len());
            return chunks(&json!({"command":prefix}).to_string());
        }
        let (details, clipped) = bounded_tail(text, MAX_TEXT_BYTES, i18n::OUTPUT_TRUNCATED);
        self.truncated |= clipped;
        chunks(&json!({"description":details}).to_string())
    }

    fn publish_tool_result(&mut self, id: &str) {
        let tool = &self.tools[id];
        if tool.summary.is_none() {
            return;
        }
        let (result, clipped) = rendered_tool_result(tool, i18n::TOOL_UNCONFIRMED);
        self.truncated |= clipped;
        if tool.result.as_ref() == Some(&result) && !tool.result_pending {
            return;
        }
        let event = tool_result(&tool.wire_id, &result);
        let emitted = self.emit(vec![event], false);
        let tool = self.tools.get_mut(id).unwrap();
        tool.result = Some(result);
        // Keep the latest known result even after the ordinary event budget
        // is full; terminal capacity is reserved for every retained tool.
        tool.result_pending = !emitted;
    }

    fn finish(&mut self, terminal: Terminal) {
        let missing = if matches!(terminal, Terminal::Stopped | Terminal::Interrupted) {
            i18n::TOOL_STOPPED
        } else {
            i18n::TOOL_UNCONFIRMED
        };
        let open = self
            .tools
            .values()
            .filter(|t| t.result.is_none() || t.result_pending)
            .map(|t| tool_result(&t.wire_id, &rendered_tool_result(t, missing).0))
            .collect();
        self.emit(open, true);
        if self.truncated {
            let id = self.id();
            self.emit(text_events(&id, i18n::PROCESS_TRUNCATED), true);
        }
        let event = match &terminal {
            Terminal::Failed(message) => (
                "RUN_ERROR",
                json!({"threadId":self.thread_id,"runId":self.run_id,"message":message,
                    "input":{"statusText":{"error":{"zh_cn":i18n::run_status(i18n::RunStatus::Failed)}}}}),
            ),
            _ => {
                let (status, label) = match terminal {
                    Terminal::Completed => ("done", i18n::LARK_COMPLETED),
                    Terminal::Interrupted => (
                        "interrupted",
                        i18n::run_status(i18n::RunStatus::Interrupted),
                    ),
                    _ => ("interrupted", i18n::run_status(i18n::RunStatus::Stopped)),
                };
                (
                    "RUN_FINISHED",
                    json!({"threadId":self.thread_id,"runId":self.run_id,"status":status,
                    "input":{"statusText":{(status):{"zh_cn":label}}}}),
                )
            }
        };
        self.emit(vec![event], true);
        self.terminal = Some(terminal);
    }

    fn emit(&mut self, events: Vec<(&'static str, Value)>, reserved: bool) -> bool {
        let mut counts = self.type_counts.clone();
        for (kind, _) in &events {
            *counts.entry(kind).or_default() += 1;
        }
        if !reserved
            && (self.event_count + events.len() > NORMAL_EVENT_LIMIT
                || counts.values().any(|n| *n > NORMAL_TYPE_LIMIT))
        {
            self.truncated = true;
            return false;
        }
        self.event_count += events.len();
        self.type_counts = counts;
        for (kind, content) in events {
            self.timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .max(self.timestamp + 1);
            self.pending.push_back(json!({"event_type":kind,"content":content.to_string(),"timestamp":self.timestamp.to_string()}));
        }
        true
    }
}

fn tool_result(id: &str, result: &str) -> (&'static str, Value) {
    (
        "TOOL_CALL_RESULT",
        json!({"toolCallId":id,"messageId":format!("{id}-result"),"content":result,"role":"tool"}),
    )
}

fn abbreviated_command(command: &str, prefix_bytes: usize) -> String {
    if command.len() <= prefix_bytes {
        return command.to_owned();
    }
    let end = command.floor_char_boundary(prefix_bytes);
    format!("{}{}", &command[..end], i18n::COMMAND_TRUNCATED)
}

fn rendered_tool_result(tool: &Tool, fallback: &str) -> (String, bool) {
    let summary = tool.summary.as_deref().unwrap_or(fallback);
    if tool.output.is_none() && tool.command.is_none() {
        return (summary.to_owned(), false);
    }
    let output = tool.output.as_ref().map_or("", |(text, _)| {
        if text.is_empty() {
            i18n::TOOL_NO_OUTPUT
        } else {
            text
        }
    });
    let elapsed_ms = tool.output.as_ref().and_then(|(_, ms)| *ms);
    let encode = |command: Option<&str>, text: &str| {
        let code = if let Some(command) = command {
            i18n::shell_output(command, text, summary, elapsed_ms)
        } else {
            format!("{}{text}", i18n::tool_output_header(summary, elapsed_ms))
        };
        json!({
            "type":"code", "language":if tool.command.is_some() {"bash"} else {"text"},
            "code":code
        })
        .to_string()
    };
    // Budget all three JSON string encodings, including the wire envelope.
    // A new TOOL_CALL_RESULT replaces the old one, so it cannot be chunked.
    let fits = |result: &str| {
        let (kind, content) = tool_result(&tool.wire_id, result);
        json!({"event_type":kind,"content":content.to_string(),"timestamp":u128::MAX.to_string()})
            .to_string()
            .len()
            <= RESULT_EVENT_BYTES
    };
    let mut command = tool.command.as_deref();
    let result = encode(command, output);
    if fits(&result) {
        return (result, false);
    }
    let short_command;
    if let Some(full) = command
        && !fits(&encode(command, i18n::OUTPUT_TRUNCATED))
    {
        short_command = abbreviated_command(full, 256);
        command = Some(&short_command);
        let result = encode(command, output);
        if fits(&result) {
            return (result, true);
        }
    }
    let marker = i18n::OUTPUT_TRUNCATED;
    let mut result = encode(command, marker);
    let output = output.strip_prefix(marker).unwrap_or(output);
    let mut low = 0;
    let mut high = output.len();
    while low < high {
        let keep = low + (high - low).div_ceil(2);
        let start = output.ceil_char_boundary(output.len() - keep);
        let candidate = encode(command, &format!("{marker}{}", &output[start..]));
        if fits(&candidate) {
            result = candidate;
            low = keep;
        } else {
            high = keep - 1;
        }
    }
    (result, true)
}

fn text_events(id: &str, text: &str) -> Vec<(&'static str, Value)> {
    let mut events = vec![(
        "TEXT_MESSAGE_START",
        json!({"messageId":id,"role":"assistant"}),
    )];
    for chunk in chunks(text) {
        events.push((
            "TEXT_MESSAGE_CONTENT",
            json!({"messageId":id,"delta":chunk}),
        ));
    }
    events.push(("TEXT_MESSAGE_END", json!({"messageId":id})));
    events
}

fn chunks(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    let mut bytes = 0;
    for ch in text.chars() {
        let cost = serde_json::to_string(&ch.to_string()).unwrap().len() - 2;
        if bytes + cost > DELTA_CHUNK_BYTES && !chunk.is_empty() {
            chunks.push(std::mem::take(&mut chunk));
            bytes = 0;
        }
        chunk.push(ch);
        bytes += cost;
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}
