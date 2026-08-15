use super::*;

#[test]
fn command_registry_routes_default_handlers_and_subcommands() {
    let runtime = isolated_command_runtime();
    let registry = runtime.registry();

    let CommandResolution::Invocation(stop) = registry.route("/stop codex-dev") else {
        panic!("expected stop invocation");
    };
    let (_, stop_arguments) = stop.into_parts();
    assert_eq!(stop_arguments.argument("agent_name"), Some("codex-dev"));

    let CommandResolution::Invocation(_reset) = registry.route("/reset") else {
        panic!("expected reset invocation");
    };

    let CommandResolution::Invocation(enable) = registry.route("/ask enable codex-dev") else {
        panic!("expected ask enable invocation");
    };
    let (_, enable_arguments) = enable.into_parts();
    assert_eq!(enable_arguments.argument("agent_name"), Some("codex-dev"));
}

#[test]
fn command_registry_routes_ask_prompt_to_the_default_handler() {
    let runtime = isolated_command_runtime();
    let registry = runtime.registry();

    let CommandResolution::Invocation(invocation) =
        registry.route("/ask codex-dev review this project")
    else {
        panic!("expected targeted ask invocation");
    };
    let (_, arguments) = invocation.into_parts();
    assert_eq!(arguments.argument("agent_name"), Some("codex-dev"));
    assert_eq!(arguments.argument("prompt"), Some("review this project"));

    let CommandResolution::Invocation(list) = registry.route("/ask list") else {
        panic!("expected ask list invocation");
    };
    let (_, arguments) = list.into_parts();
    assert_eq!(arguments.argument("agent_name"), None);
    assert_eq!(arguments.argument("prompt"), None);
}

#[test]
fn command_registry_generates_root_and_command_help() {
    let runtime = isolated_command_runtime();
    let registry = runtime.registry();

    assert_eq!(
        registry.route("/help"),
        CommandResolution::Reply(
            "Agora 命令：\n\
/stop - 停止当前对话中正在运行或排队的 Agent 任务。\n\
/reset - 停止任务并重置后端 Agent 会话。\n\
/ask - 向指定 Agent 提问或控制 Agent 的消息接收状态。\n\
/help - 显示所有命令。\n\n\
使用 /{command} help 查看详情。"
                .to_string()
        )
    );
    assert_eq!(
        registry.route("/stop help"),
        CommandResolution::Reply(
            "/stop - 停止当前对话中正在运行或排队的 Agent 任务。\n\n\
用法：\n\
/stop [{agent_name}]\n\
\n参数：\n\
agent_name (可选) - 已配置的 Agent 名称；省略时停止全部 Agent。"
                .to_string()
        )
    );
    assert_eq!(
        registry.route("/reset help"),
        CommandResolution::Reply(
            "/reset - 停止任务并重置后端 Agent 会话。\n\n\
用法：\n\
/reset"
                .to_string()
        )
    );
}

#[test]
fn command_registry_generates_subcommand_and_argument_help() {
    let runtime = isolated_command_runtime();
    let registry = runtime.registry();
    let expected = concat!(
        "/ask - 向指定 Agent 提问或控制 Agent 的消息接收状态。\n\n",
        "用法：\n",
        "/ask {agent_name} {prompt}\n\n",
        "参数：\n",
        "agent_name (必填) - 当前对话中已配置的 Agent 名称。\n",
        "prompt (必填) - 仅发送给指定 Agent 的提示词。\n\n",
        "子命令：\n",
        "/ask list\n",
        "  列出所有已订阅 Agent 及其当前状态。\n",
        "/ask status {agent_name}\n",
        "  查看指定 Agent 的当前状态。\n",
        "  agent_name (必填) - 当前对话中已配置的 Agent 名称。\n",
        "/ask disable {agent_name}\n",
        "  禁止指定 Agent 接收后续消息。\n",
        "  agent_name (必填) - 当前对话中已配置的 Agent 名称。\n",
        "/ask enable {agent_name}\n",
        "  允许指定 Agent 接收后续消息。\n",
        "  agent_name (必填) - 当前对话中已配置的 Agent 名称。\n\n",
        "使用 /ask {子命令} help 查看详情。",
    );

    assert_eq!(
        registry.route("/ask help"),
        CommandResolution::Reply(expected.to_string())
    );
    assert_eq!(
        registry.route("/ask"),
        CommandResolution::Reply(expected.to_string())
    );
    assert_eq!(
        registry.route("/ask enable help"),
        CommandResolution::Reply(
            "/ask enable - 允许指定 Agent 接收后续消息。\n\n\
用法：\n\
/ask enable {agent_name}\n\n\
参数：\n\
agent_name (必填) - 当前对话中已配置的 Agent 名称。"
                .to_string()
        )
    );
}

#[test]
fn command_registry_generates_validation_errors_from_registered_arguments() {
    let runtime = isolated_command_runtime();
    let registry = runtime.registry();

    assert_eq!(
        registry.route("run cargo test"),
        CommandResolution::AgentInput
    );
    assert_eq!(
        registry.route("/stop codex-dev reviewer"),
        CommandResolution::Reply("用法：/stop [{agent_name}]".to_string())
    );
    assert_eq!(
        registry.route("/ask disable"),
        CommandResolution::Reply("用法：/ask disable {agent_name}".to_string())
    );
    assert_eq!(
        registry.route("/ask unknown"),
        CommandResolution::Reply("用法：/ask {agent_name} {prompt}".to_string())
    );
    assert_eq!(
        registry.route("/unknown"),
        CommandResolution::Reply("未知命令：/unknown\n使用 /help 查看全部命令。".to_string())
    );
}

#[test]
fn command_registry_resolves_arbitrarily_nested_subcommands() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum NestedHandler {
        Run,
    }

    let mut registry = CommandRegistry::new();
    registry
        .register(
            CommandNode::new("outer", "Outer command").subcommand(
                CommandNode::new("middle", "Middle command").subcommand(
                    CommandNode::new("inner", "Inner command")
                        .argument(Argument::required("value", "Value to process"))
                        .handler(NestedHandler::Run),
                ),
            ),
        )
        .unwrap();

    let CommandResolution::Invocation(invocation) = registry.route("/outer middle inner payload")
    else {
        panic!("expected nested invocation");
    };
    let (handler, arguments) = invocation.into_parts();
    assert_eq!(handler, NestedHandler::Run);
    assert_eq!(arguments.argument("value"), Some("payload"));
}

#[test]
fn command_registry_validates_structured_arguments() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum TestHandler {
        Enable,
    }

    let mut registry = CommandRegistry::new();
    registry
        .register(
            CommandNode::new("ask", "Control agents").subcommand(
                CommandNode::new("enable", "Enable one agent")
                    .argument(Argument::required("agent_name", "Agent name"))
                    .handler(TestHandler::Enable),
            ),
        )
        .unwrap();

    let valid = CommandRequest::new(["ask", "enable"]).with_argument("agent_name", "codex-dev");
    let CommandResolution::Invocation(invocation) = registry.route_structured(&valid) else {
        panic!("expected structured invocation");
    };
    let (handler, arguments) = invocation.into_parts();
    assert_eq!(handler, TestHandler::Enable);
    assert_eq!(arguments.argument("agent_name"), Some("codex-dev"));

    assert_eq!(
        registry.route_structured(&CommandRequest::new(["ask", "enable"])),
        CommandResolution::Reply("用法：/ask enable {agent_name}".to_string())
    );
}

#[tokio::test]
async fn command_registry_executes_a_registered_handler_without_central_dispatch() {
    let mut registry: CommandRegistry<CommandHandler> = CommandRegistry::new();
    registry
        .register(
            CommandNode::new("echo", "Echo one value")
                .argument(Argument::required("value", "Value to echo"))
                .handler(CommandHandler::new(|_context, arguments| async move {
                    let value = arguments.argument("value").unwrap_or_default();
                    Ok(Some(ChannelReply::new(format!("Echo: {value}"))))
                })),
        )
        .unwrap();
    let CommandResolution::Invocation(invocation) = registry.route_text("/echo hello") else {
        panic!("expected echo invocation");
    };
    let (handler, arguments) = invocation.into_parts();
    let execution = handler
        .execute(
            CommandContext::text("test", "session", Vec::new()),
            arguments,
        )
        .await
        .unwrap();
    let CommandExecution::Reply(reply) = execution else {
        panic!("expected command reply");
    };

    assert_eq!(reply, Some(ChannelReply::new("Echo: hello")));
}

#[test]
fn command_registry_rejects_invalid_tree_definitions() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum TestHandler {
        Run,
    }

    let duplicate = CommandNode::new("root", "Root command")
        .subcommand(CommandNode::new("child", "First child").handler(TestHandler::Run))
        .subcommand(CommandNode::new("child", "Second child").handler(TestHandler::Run));
    assert_eq!(
        CommandRegistry::new()
            .register(duplicate)
            .unwrap_err()
            .to_string(),
        "duplicate subcommand in /root: child"
    );

    let invalid_arguments = CommandNode::new("run", "Run a task")
        .argument(Argument::optional("first", "Optional first argument"))
        .argument(Argument::required("second", "Required second argument"))
        .handler(TestHandler::Run);
    assert_eq!(
        CommandRegistry::new()
            .register(invalid_arguments)
            .unwrap_err()
            .to_string(),
        "required argument follows an optional argument in /run"
    );

    let invalid_remaining = CommandNode::new("ask", "Ask one agent")
        .argument(Argument::required_remaining("prompt", "Prompt"))
        .argument(Argument::required("agent_name", "Agent name"))
        .handler(TestHandler::Run);
    assert_eq!(
        CommandRegistry::new()
            .register(invalid_remaining)
            .unwrap_err()
            .to_string(),
        "remaining argument is not last in /ask: prompt"
    );

    for (command, expected) in [
        (
            CommandNode::new("invalid name", "Invalid name").handler(TestHandler::Run),
            "invalid command name: invalid name",
        ),
        (
            CommandNode::new("help", "Reserved name").handler(TestHandler::Run),
            "command name is reserved: help",
        ),
        (
            CommandNode::new("empty", " ").handler(TestHandler::Run),
            "command description is empty: empty",
        ),
        (
            CommandNode::new("arguments", "Arguments without a handler")
                .argument(Argument::optional("value", "Value")),
            "command without a handler has arguments: arguments",
        ),
        (
            CommandNode::new("empty", "No behavior"),
            "command has neither a handler nor subcommands: empty",
        ),
        (
            CommandNode::new("run", "Invalid argument")
                .argument(Argument::required("invalid name", "Value"))
                .handler(TestHandler::Run),
            "invalid argument name in /run: invalid name",
        ),
        (
            CommandNode::new("run", "Duplicate argument")
                .argument(Argument::required("value", "First value"))
                .argument(Argument::optional("value", "Second value"))
                .handler(TestHandler::Run),
            "duplicate argument in /run: value",
        ),
    ] {
        assert_eq!(
            CommandRegistry::new()
                .register(command)
                .unwrap_err()
                .to_string(),
            expected
        );
    }

    let mut registry = CommandRegistry::new();
    registry
        .register(CommandNode::new("run", "First root").handler(TestHandler::Run))
        .unwrap();
    assert_eq!(
        registry
            .register(CommandNode::new("run", "Second root").handler(TestHandler::Run))
            .unwrap_err()
            .to_string(),
        "duplicate root command: run"
    );
}

#[test]
fn command_registry_handles_invalid_help_and_structured_paths() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum TestHandler {
        Run,
    }

    let mut registry = CommandRegistry::new();
    registry
        .register(
            CommandNode::new("root", "Root command").subcommand(
                CommandNode::new("child", "Child command")
                    .subcommand(CommandNode::new("run", "Run command").handler(TestHandler::Run)),
            ),
        )
        .unwrap();

    assert_eq!(
        registry.route("/help extra"),
        CommandResolution::Reply("用法：/help".to_string())
    );
    assert_eq!(
        registry.route("/root help extra"),
        CommandResolution::Reply("用法：/root help".to_string())
    );
    assert!(matches!(
        registry.route("/root"),
        CommandResolution::Reply(reply) if reply.contains("/root child")
    ));
    assert!(matches!(
        registry.route("/root unknown"),
        CommandResolution::Reply(reply) if reply.contains("未知子命令")
    ));

    assert!(matches!(
        registry.route_structured(&CommandRequest::new(std::iter::empty::<&str>())),
        CommandResolution::Reply(reply) if reply == crate::i18n::UNKNOWN_STRUCTURED_COMMAND
    ));
    assert!(matches!(
        registry.route_structured(&CommandRequest::new(["unknown"])),
        CommandResolution::Reply(reply) if reply.contains("未知命令")
    ));
    assert!(matches!(
        registry.route_structured(&CommandRequest::new(["root", "unknown"])),
        CommandResolution::Reply(reply) if reply.contains("未知命令")
    ));
    assert!(matches!(
        registry.route_structured(&CommandRequest::new(["root", "child"])),
        CommandResolution::Reply(reply) if reply.contains("/root child run")
    ));
}

#[tokio::test]
async fn help_commands_reply_without_starting_agents() {
    let temp = tempfile::tempdir().unwrap();
    let dispatcher =
        AgentDispatcher::new(SessionStore::open(temp.path().join("store.db")).unwrap());
    let replies = Arc::new(Mutex::new(Vec::new()));
    let channel = CommandTestChannel::new(Arc::clone(&replies));
    let mut runs = tokio::task::JoinSet::new();
    let commands = command_runtime(&dispatcher);
    let registry = commands.registry();

    Daemon::route_channel_task(
        &channel,
        &[],
        &dispatcher,
        &commands,
        CommandTestTask::new("ask-help", "chat-1", "/ask help"),
        &mut runs,
    )
    .await
    .unwrap();
    Daemon::route_channel_task(
        &channel,
        &[],
        &dispatcher,
        &commands,
        CommandTestTask::new("stop-help", "chat-1", "/stop help"),
        &mut runs,
    )
    .await
    .unwrap();
    Daemon::route_channel_task(
        &channel,
        &[],
        &dispatcher,
        &commands,
        CommandTestTask::new("reset-help", "chat-1", "/reset help"),
        &mut runs,
    )
    .await
    .unwrap();
    Daemon::route_channel_task(
        &channel,
        &[],
        &dispatcher,
        &commands,
        CommandTestTask::new("root-help", "chat-1", "/help"),
        &mut runs,
    )
    .await
    .unwrap();

    assert_eq!(
        replies.lock().unwrap().as_slice(),
        [
            command_reply(registry, "/ask help"),
            command_reply(registry, "/stop help"),
            command_reply(registry, "/reset help"),
            command_reply(registry, "/help"),
        ]
    );
}

fn command_reply(registry: &CommandRegistry<CommandHandler>, input: &str) -> ChannelReply {
    let CommandResolution::Reply(reply) = registry.route(input) else {
        panic!("expected command reply for {input}");
    };
    ChannelReply::new(reply)
}
