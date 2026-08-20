CREATE TABLE IF NOT EXISTS agent_sessions (
    isolation_scope   TEXT    NOT NULL CHECK (isolation_scope IN ('shared', 'session')),
    channel_name      TEXT,
    channel_session_id TEXT,
    agent_name        TEXT    NOT NULL,
    agent_session_id  TEXT    NOT NULL CHECK (length(agent_session_id) > 0),
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,

    CHECK (
        (isolation_scope = 'shared' AND channel_name IS NULL AND channel_session_id IS NULL)
        OR
        (
            isolation_scope = 'session'
            AND channel_name IS NOT NULL
            AND channel_session_id IS NOT NULL
            AND length(channel_name) > 0
            AND length(channel_session_id) > 0
        )
    ),
    UNIQUE (agent_name, agent_session_id)
);

CREATE UNIQUE INDEX IF NOT EXISTS agent_sessions_shared_scope
    ON agent_sessions (agent_name)
    WHERE isolation_scope = 'shared';

CREATE UNIQUE INDEX IF NOT EXISTS agent_sessions_session_scope
    ON agent_sessions (agent_name, channel_name, channel_session_id)
    WHERE isolation_scope = 'session';

CREATE TABLE IF NOT EXISTS channel_session_agent_blocks (
    channel_name       TEXT NOT NULL CHECK (length(channel_name) > 0),
    channel_session_id TEXT NOT NULL CHECK (length(channel_session_id) > 0),
    agent_name         TEXT NOT NULL CHECK (length(agent_name) > 0),
    PRIMARY KEY (channel_name, channel_session_id, agent_name)
) WITHOUT ROWID;
