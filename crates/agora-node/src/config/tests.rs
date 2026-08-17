use super::*;

#[test]
fn node_runtime_limits_use_safe_defaults() {
    let config: NodeConfig = serde_json::from_str(r#"{"channels":[],"agents":[]}"#).unwrap();

    assert_eq!(config.runtime.max_in_flight_tasks, 32);
    assert_eq!(config.runtime.max_in_flight_runs, 64);
    assert_eq!(config.runtime.max_concurrent_runs, 4);
}

#[test]
fn node_runtime_limits_accept_explicit_values() {
    let config: NodeConfig = serde_json::from_str(
        r#"{
            "runtime": {
                "max_in_flight_tasks": 7,
                "max_in_flight_runs": 11,
                "max_concurrent_runs": 3
            },
            "channels": [],
            "agents": []
        }"#,
    )
    .unwrap();

    assert_eq!(config.runtime.max_in_flight_tasks, 7);
    assert_eq!(config.runtime.max_in_flight_runs, 11);
    assert_eq!(config.runtime.max_concurrent_runs, 3);
    config.validate().unwrap();
}

#[test]
fn node_runtime_limits_reject_zero_and_inconsistent_values() {
    for (runtime, expected) in [
        (
            r#"{"max_in_flight_tasks":0,"max_in_flight_runs":2,"max_concurrent_runs":1}"#,
            "runtime max_in_flight_tasks must be positive",
        ),
        (
            r#"{"max_in_flight_tasks":2,"max_in_flight_runs":0,"max_concurrent_runs":1}"#,
            "runtime max_in_flight_runs must be positive",
        ),
        (
            r#"{"max_in_flight_tasks":2,"max_in_flight_runs":2,"max_concurrent_runs":0}"#,
            "runtime max_concurrent_runs must be positive",
        ),
        (
            r#"{"max_in_flight_tasks":2,"max_in_flight_runs":2,"max_concurrent_runs":3}"#,
            "runtime max_concurrent_runs must not exceed max_in_flight_runs",
        ),
    ] {
        let document = format!(r#"{{"runtime":{runtime},"channels":[],"agents":[]}}"#);
        let config: NodeConfig = serde_json::from_str(&document).unwrap();

        let error = config.validate().unwrap_err();

        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn node_runtime_limits_reject_unadmittable_channel_fanout() {
    let config: NodeConfig = serde_json::from_str(
        r#"{
            "runtime": {
                "max_in_flight_tasks": 2,
                "max_in_flight_runs": 1,
                "max_concurrent_runs": 1
            },
            "channels": [
                {"type":"lark","name":"lark","app_id":"id","secret":"secret"}
            ],
            "agents": [
                {"name":"one","isolate":"none","type":"custom","path":"agent","subscribe":[{"channel":"lark"}]},
                {"name":"two","isolate":"none","type":"custom","path":"agent","subscribe":[{"channel":"lark"}]}
            ]
        }"#,
    )
    .unwrap();

    let error = config.validate().unwrap_err();

    assert_eq!(
        error.to_string(),
        "runtime max_in_flight_runs cannot admit channel fan-out: lark requires 2, limit is 1"
    );
}

#[test]
fn http_proxy_accepts_optional_credentials_and_rejects_invalid_addresses() {
    for (value, expected) in [
        ("proxy.local:8080", "http://proxy.local:8080"),
        (
            "http://user:password@proxy.local:8080",
            "http://user:password@proxy.local:8080",
        ),
        (
            ":password@proxy.local:8080",
            "http://:password@proxy.local:8080",
        ),
        ("user:@proxy.local:8080", "http://user:@proxy.local:8080"),
        (":@proxy.local:8080", "http://:@proxy.local:8080"),
        ("[::1]:8080", "http://[::1]:8080"),
    ] {
        assert_eq!(
            value.parse::<HttpProxy>().unwrap().environment_value(),
            expected
        );
    }

    for invalid in [
        "https://proxy.local:8080",
        "proxy.local",
        "proxy.local:0",
        "proxy.local:invalid",
        "user@proxy.local:8080",
        "bad host:8080",
        "::1:8080",
        "[::1:8080",
    ] {
        assert!(invalid.parse::<HttpProxy>().is_err(), "accepted {invalid}");
    }
}

#[test]
fn component_proxies_override_the_global_default() {
    let mut config: NodeConfig = serde_json::from_str(
        r#"{
            "proxy":"global:8000",
            "channels":[
                {"type":"lark","name":"lark","app_id":"id","secret":"secret"},
                {"type":"telegram","name":"telegram","token":"token","proxy":"tg:8001"},
                {"type":"local","name":"local"},
                {"type":"http","name":"http","proxy":"http:8002"}
            ],
            "agents":[
                {"name":"global","isolate":"none","type":"custom","path":"agent","subscribe":[]},
                {"name":"own","isolate":"none","type":"custom","path":"agent","proxy":"agent:8003","subscribe":[]}
            ]
        }"#,
    )
    .unwrap();

    config.apply_proxy_defaults();

    assert_eq!(
        config.agents[0].proxy.as_ref().unwrap().environment_value(),
        "http://global:8000"
    );
    assert_eq!(
        config.agents[1].proxy.as_ref().unwrap().environment_value(),
        "http://agent:8003"
    );
    let channel_proxies = config
        .channels
        .iter_mut()
        .map(|channel| channel.proxy_mut().as_ref().unwrap().environment_value())
        .collect::<Vec<_>>();
    assert_eq!(
        channel_proxies,
        [
            "http://global:8000",
            "http://tg:8001",
            "http://global:8000",
            "http://http:8002",
        ]
    );
}

#[test]
fn absent_global_proxy_leaves_components_unconfigured() {
    let mut config: NodeConfig = serde_json::from_str(r#"{"channels":[],"agents":[]}"#).unwrap();

    config.apply_proxy_defaults();

    assert_eq!(config.proxy, None);
}

#[test]
fn isolation_scope_and_sandbox_strings_cover_all_variants() {
    assert_eq!(IsolationScope::Shared.channel_name(), None);
    assert_eq!(IsolationScope::Shared.session_id(), None);
    assert_eq!(IsolationScope::Shared.as_str(), "shared");

    let session = IsolationScope::session("telegram", "chat-1");
    assert_eq!(session.channel_name(), Some("telegram"));
    assert_eq!(session.session_id(), Some("chat-1"));
    assert_eq!(session.as_str(), "session");

    assert_eq!(AgentSandbox::ReadOnly.as_str(), "read-only");
    assert_eq!(AgentSandbox::WorkspaceWrite.as_str(), "workspace-write");
    assert_eq!(
        AgentSandbox::DangerFullAccess.as_str(),
        "danger-full-access"
    );
}

#[test]
fn config_accessors_preserve_names_paths_and_proxy_credentials() {
    let config: NodeConfig = serde_json::from_str(
        r#"{
            "channels":[
                {"type":"lark","name":"lark","app_id":"id","secret":"secret"},
                {"type":"telegram","name":"telegram","token":"token"},
                {"type":"local","name":"local"},
                {"type":"http","name":"http"}
            ],
            "agents":[{
                "name":"agent",
                "isolate":"session",
                "workspace":"/tmp/agora-workspace",
                "type":"custom",
                "path":"agent",
                "subscribe":[]
            }]
        }"#,
    )
    .unwrap();

    assert_eq!(
        config
            .channels
            .iter()
            .map(ChannelConfig::name)
            .collect::<Vec<_>>(),
        ["lark", "telegram", "local", "http"]
    );
    assert_eq!(
        config.agents[0].workdir(),
        PathBuf::from("/tmp/agora-workspace")
    );
    assert_eq!(
        config.agents[0].isolation_scope("lark", "chat-1"),
        IsolationScope::session("lark", "chat-1")
    );

    let authenticated = "user:secret@proxy.local:8080".parse::<HttpProxy>().unwrap();
    assert_eq!(authenticated.address(), "proxy.local:8080");
    assert_eq!(authenticated.credentials(), Some(("user", "secret")));
    assert_eq!(
        format!("{authenticated:?}"),
        "HttpProxy { address: \"proxy.local:8080\", authenticated: true }"
    );

    let anonymous = "proxy.local:8080".parse::<HttpProxy>().unwrap();
    assert_eq!(anonymous.credentials(), None);
    assert_eq!(
        format!("{anonymous:?}"),
        "HttpProxy { address: \"proxy.local:8080\", authenticated: false }"
    );
}

#[test]
fn omitted_workspace_uses_the_agora_home_directory() {
    let config: AgentConfig = serde_json::from_str(
        r#"{
            "name":"agent",
            "isolate":"none",
            "type":"custom",
            "path":"agent",
            "subscribe":[]
        }"#,
    )
    .unwrap();

    assert!(config.workdir().ends_with(".agora/workspace"));
    assert_eq!(
        config.isolation_scope("ignored", "ignored"),
        IsolationScope::Shared
    );
    assert_eq!(config.timeout_seconds, 3600);
    assert_eq!(config.max_output_bytes, 67_108_864);
}

#[test]
fn agent_execution_limits_accept_explicit_values() {
    let config: AgentConfig = serde_json::from_str(
        r#"{
            "name":"agent",
            "isolate":"none",
            "type":"custom",
            "path":"agent",
            "timeout_seconds":15,
            "max_output_bytes":4096,
            "subscribe":[]
        }"#,
    )
    .unwrap();

    assert_eq!(config.timeout_seconds, 15);
    assert_eq!(config.max_output_bytes, 4096);
}

#[test]
fn node_config_rejects_zero_agent_execution_limits() {
    for (field, expected) in [
        ("timeout_seconds", "agent timeout_seconds must be positive"),
        (
            "max_output_bytes",
            "agent max_output_bytes must be positive",
        ),
    ] {
        let document = format!(
            r#"{{
                "channels":[],
                "agents":[{{
                    "name":"agent",
                    "isolate":"none",
                    "workspace":"/tmp/work",
                    "type":"custom",
                    "path":"agent",
                    "{field}":0,
                    "subscribe":[]
                }}]
            }}"#
        );
        let config: NodeConfig = serde_json::from_str(&document).unwrap();

        let error = config.validate().unwrap_err();

        assert!(
            error.to_string().contains(expected),
            "unexpected validation error: {error:#}"
        );
    }
}

#[test]
fn node_config_rejects_ambiguous_or_invalid_runtime_entries() {
    let cases = [
        (
            r#"{"channels":[{"type":"local","name":""}],"agents":[]}"#,
            "channel name must not be empty",
        ),
        (
            r#"{"channels":[{"type":"lark","name":"same","app_id":"id","secret":"secret"},{"type":"telegram","name":"same","token":"token"}],"agents":[]}"#,
            "duplicate channel name: same",
        ),
        (
            r#"{"channels":[{"type":"lark","name":"lark","app_id":"","secret":"secret"}],"agents":[]}"#,
            "lark app_id must not be empty: lark",
        ),
        (
            r#"{"channels":[{"type":"lark","name":"lark","app_id":"id","secret":" "}],"agents":[]}"#,
            "lark secret must not be empty: lark",
        ),
        (
            r#"{"channels":[{"type":"telegram","name":"telegram","token":""}],"agents":[]}"#,
            "telegram token must not be empty: telegram",
        ),
        (
            r#"{"channels":[{"type":"telegram","name":"telegram","token":"token","permission":{"users":[{"id":" "}]}}],"agents":[]}"#,
            "channel user permission id must not be empty: telegram",
        ),
        (
            r#"{"channels":[{"type":"lark","name":"lark","app_id":"id","secret":"secret","permission":{"groups":[{"id":""}]}}],"agents":[]}"#,
            "channel group permission id must not be empty: lark",
        ),
        (
            r#"{"channels":[{"type":"local","name":"local"}],"agents":[]}"#,
            "local channel is not implemented: local",
        ),
        (
            r#"{"channels":[{"type":"http","name":"http"}],"agents":[]}"#,
            "http channel is not implemented: http",
        ),
        (
            r#"{"channels":[],"agents":[{"name":"","isolate":"none","workspace":"/tmp/work","type":"custom","path":"agent","subscribe":[]}]}"#,
            "agent name must not be empty",
        ),
        (
            r#"{"channels":[],"agents":[{"name":"same","isolate":"none","workspace":"/tmp/one","type":"custom","path":"agent","subscribe":[]},{"name":"same","isolate":"none","workspace":"/tmp/two","type":"custom","path":"agent","subscribe":[]}]}"#,
            "duplicate agent name: same",
        ),
        (
            r#"{"channels":[],"agents":[{"name":"agent","isolate":"none","workspace":"/tmp/work","type":"custom","path":"","subscribe":[]}]}"#,
            "agent path must not be empty: agent",
        ),
        (
            r#"{"channels":[],"agents":[{"name":"agent","isolate":"none","workspace":"relative","type":"custom","path":"agent","subscribe":[]}]}"#,
            "agent workspace must be absolute: agent",
        ),
        (
            r#"{"channels":[],"agents":[{"name":"agent","isolate":"none","workspace":"/tmp/work","type":"custom","path":"agent","subscribe":[{"channel":"missing"}]}]}"#,
            "agent subscription references an unknown channel: agent -> missing",
        ),
    ];

    for (document, expected) in cases {
        let config: NodeConfig = serde_json::from_str(document).unwrap();
        let error = config.validate().unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "unexpected validation error: {error:#}"
        );
    }
}

#[test]
fn node_config_accepts_unique_entries_and_existing_subscriptions() {
    let config: NodeConfig = serde_json::from_str(
        r#"{
            "channels":[{"type":"lark","name":"lark","app_id":"id","secret":"secret"}],
            "agents":[{
                "name":"agent",
                "isolate":"none",
                "workspace":"/tmp/work",
                "type":"custom",
                "path":"agent",
                "subscribe":[{"channel":"lark"}]
            }]
        }"#,
    )
    .unwrap();

    config.validate().unwrap();
}
