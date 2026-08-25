INSERT INTO agent_sessions (
    agent_name,
    backend_type,
    canonical_workspace,
    isolation_scope,
    channel_name,
    provider_type,
    provider_account_identity,
    channel_session_id,
    agent_session_id,
    created_at,
    updated_at
)
SELECT
    agent_name,
    'legacy_v3',
    'legacy_v3',
    isolation_scope,
    channel_name,
    CASE WHEN isolation_scope = 'session' THEN 'legacy_v3' END,
    CASE WHEN isolation_scope = 'session' THEN 'legacy_v3' END,
    channel_session_id,
    agent_session_id,
    created_at,
    updated_at
FROM agent_sessions_v3;

INSERT INTO channel_session_agent_blocks (
    channel_name,
    provider_type,
    provider_account_identity,
    channel_session_id,
    agent_name,
    backend_type,
    canonical_workspace,
    created_at
)
SELECT
    channel_name,
    'legacy_v3',
    'legacy_v3',
    channel_session_id,
    agent_name,
    'legacy_v3',
    'legacy_v3',
    CAST(strftime('%s', 'now') AS INTEGER) * 1000
FROM channel_session_agent_blocks_v3;

DROP TABLE agent_sessions_v3;
DROP TABLE channel_session_agent_blocks_v3;
