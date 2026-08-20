CREATE TABLE IF NOT EXISTS agent_sessions (
    agent_name                TEXT    NOT NULL CHECK (length(agent_name) > 0),
    backend_type              TEXT    NOT NULL CHECK (length(backend_type) > 0),
    canonical_workspace       TEXT    NOT NULL CHECK (length(canonical_workspace) > 0),
    isolation_scope           TEXT    NOT NULL CHECK (isolation_scope IN ('shared', 'session')),
    channel_name              TEXT,
    provider_type             TEXT,
    provider_account_identity TEXT,
    channel_session_id        TEXT,
    agent_session_id          TEXT    NOT NULL CHECK (length(agent_session_id) > 0),
    created_at                INTEGER NOT NULL,
    updated_at                INTEGER NOT NULL,
    CHECK (
        (
            isolation_scope = 'shared'
            AND channel_name IS NULL
            AND provider_type IS NULL
            AND provider_account_identity IS NULL
            AND channel_session_id IS NULL
        )
        OR
        (
            isolation_scope = 'session'
            AND channel_name IS NOT NULL AND length(channel_name) > 0
            AND provider_type IS NOT NULL AND length(provider_type) > 0
            AND provider_account_identity IS NOT NULL
                AND length(provider_account_identity) > 0
            AND channel_session_id IS NOT NULL AND length(channel_session_id) > 0
        )
    ),
    UNIQUE (agent_name, backend_type, canonical_workspace, agent_session_id)
);

CREATE UNIQUE INDEX IF NOT EXISTS agent_sessions_shared_scope
    ON agent_sessions (agent_name, backend_type, canonical_workspace)
    WHERE isolation_scope = 'shared';

CREATE UNIQUE INDEX IF NOT EXISTS agent_sessions_session_scope
    ON agent_sessions (
        agent_name,
        backend_type,
        canonical_workspace,
        channel_name,
        provider_type,
        provider_account_identity,
        channel_session_id
    )
    WHERE isolation_scope = 'session';

CREATE TABLE IF NOT EXISTS channel_session_agent_blocks (
    channel_name              TEXT    NOT NULL CHECK (length(channel_name) > 0),
    provider_type             TEXT    NOT NULL CHECK (length(provider_type) > 0),
    provider_account_identity TEXT    NOT NULL CHECK (length(provider_account_identity) > 0),
    channel_session_id        TEXT    NOT NULL CHECK (length(channel_session_id) > 0),
    agent_name                TEXT    NOT NULL CHECK (length(agent_name) > 0),
    backend_type              TEXT    NOT NULL CHECK (length(backend_type) > 0),
    canonical_workspace       TEXT    NOT NULL CHECK (length(canonical_workspace) > 0),
    created_at                INTEGER NOT NULL,
    PRIMARY KEY (
        channel_name,
        provider_type,
        provider_account_identity,
        channel_session_id,
        agent_name,
        backend_type,
        canonical_workspace
    )
) WITHOUT ROWID;
