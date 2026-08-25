use crate::config::IsolationScope;
use crate::instance::{StatePaths, secure_directory, secure_file};
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: i64 = 4;
const CREATE_SCHEMA: &str = include_str!("schema.sql");
const MIGRATE_V3: &str = include_str!("migrate_v3.sql");

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    agent_name: String,
    isolation_scope: IsolationScope,
}

impl SessionKey {
    pub fn new(agent_name: impl Into<String>, isolation_scope: IsolationScope) -> Self {
        Self {
            agent_name: agent_name.into(),
            isolation_scope,
        }
    }

    pub fn isolation_scope(&self) -> &IsolationScope {
        &self.isolation_scope
    }

    pub fn channel_name(&self) -> Option<&str> {
        self.isolation_scope.channel_name()
    }

    pub fn channel_session_id(&self) -> Option<&str> {
        self.isolation_scope.session_id()
    }

    pub fn agent_name(&self) -> &str {
        &self.agent_name
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentIdentity {
    configured_name: String,
    backend_type: String,
    canonical_workspace: String,
}

impl AgentIdentity {
    pub fn new(
        configured_name: impl Into<String>,
        backend_type: impl Into<String>,
        canonical_workspace: impl Into<String>,
    ) -> Self {
        Self {
            configured_name: configured_name.into(),
            backend_type: backend_type.into(),
            canonical_workspace: canonical_workspace.into(),
        }
    }

    fn validate(&self) -> Result<()> {
        validate_nonempty("agent configured name", &self.configured_name)?;
        validate_nonempty("agent backend type", &self.backend_type)?;
        validate_nonempty("agent canonical workspace", &self.canonical_workspace)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChannelIdentity {
    configured_name: String,
    provider_type: String,
    account_identity: String,
}

impl ChannelIdentity {
    pub fn new(
        configured_name: impl Into<String>,
        provider_type: impl Into<String>,
        account_identity: impl Into<String>,
    ) -> Self {
        Self {
            configured_name: configured_name.into(),
            provider_type: provider_type.into(),
            account_identity: account_identity.into(),
        }
    }

    pub fn configured_name(&self) -> &str {
        &self.configured_name
    }

    fn provider_type(&self) -> &str {
        &self.provider_type
    }

    fn account_identity(&self) -> &str {
        &self.account_identity
    }

    fn validate(&self) -> Result<()> {
        validate_nonempty("channel configured name", &self.configured_name)?;
        validate_nonempty("channel provider type", &self.provider_type)?;
        validate_nonempty("channel account identity", &self.account_identity)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum StoreSessionScope {
    Shared,
    Session {
        channel: ChannelIdentity,
        session_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StoreSessionKey {
    agent: AgentIdentity,
    scope: StoreSessionScope,
}

impl StoreSessionKey {
    pub fn shared(agent: AgentIdentity) -> Self {
        Self {
            agent,
            scope: StoreSessionScope::Shared,
        }
    }

    pub fn session(
        agent: AgentIdentity,
        channel: ChannelIdentity,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            agent,
            scope: StoreSessionScope::Session {
                channel,
                session_id: session_id.into(),
            },
        }
    }

    fn scope_name(&self) -> &'static str {
        match self.scope {
            StoreSessionScope::Shared => "shared",
            StoreSessionScope::Session { .. } => "session",
        }
    }

    fn channel(&self) -> Option<&ChannelIdentity> {
        match &self.scope {
            StoreSessionScope::Shared => None,
            StoreSessionScope::Session { channel, .. } => Some(channel),
        }
    }

    fn session_id(&self) -> Option<&str> {
        match &self.scope {
            StoreSessionScope::Shared => None,
            StoreSessionScope::Session { session_id, .. } => Some(session_id),
        }
    }

    fn validate(&self) -> Result<()> {
        self.agent.validate()?;
        if let StoreSessionScope::Session {
            channel,
            session_id,
        } = &self.scope
        {
            channel.validate()?;
            validate_nonempty("channel session id", session_id)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StoreChannelSessionKey {
    channel: ChannelIdentity,
    session_id: String,
}

impl StoreChannelSessionKey {
    pub fn new(channel: ChannelIdentity, session_id: impl Into<String>) -> Self {
        Self {
            channel,
            session_id: session_id.into(),
        }
    }

    fn validate(&self) -> Result<()> {
        self.channel.validate()?;
        validate_nonempty("channel session id", &self.session_id)
    }
}

#[derive(Clone)]
pub struct SessionStore {
    connection: Arc<Mutex<Connection>>,
}

impl SessionStore {
    pub fn open_default() -> Result<Self> {
        let paths = StatePaths::from_environment()?;
        secure_directory(paths.root())?;
        secure_directory(paths.db_dir())?;
        Self::open(paths.store_path())
    }

    pub fn default_path() -> Result<PathBuf> {
        Ok(StatePaths::from_environment()?.store_path().to_path_buf())
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("store path has no parent: {}", path.display()))?;
        let parent_existed = parent.exists();
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create store directory failed: {}", parent.display()))?;
        if !parent_existed {
            secure_directory(parent)?;
        }
        let mut connection = Connection::open(path)
            .with_context(|| format!("open sqlite store failed: {}", path.display()))?;
        secure_file(path)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .context("configure sqlite store busy timeout failed")?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;",
            )
            .context("configure sqlite store failed")?;
        Self::initialize(&mut connection)?;
        secure_sqlite_files(path)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn get(&self, key: &StoreSessionKey) -> Result<Option<String>> {
        key.validate()?;
        query_session(&self.lock_connection(), key)
    }

    pub fn observe(
        &self,
        key: &StoreSessionKey,
        expected_current: Option<&str>,
        observed: &str,
    ) -> Result<()> {
        key.validate()?;
        validate_nonempty("observed agent session id", observed)?;
        if expected_current.is_some_and(|expected| expected != observed) {
            bail!("resumed agent reported a different session id");
        }
        let mut connection = self.lock_connection();
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("begin agent session observation failed")?;
        let current = query_session(&transaction, key)?;
        if current.as_deref() == Some(observed) {
            transaction
                .commit()
                .context("commit unchanged agent session observation failed")?;
            return Ok(());
        }
        if current.as_deref() != expected_current {
            bail!("agent session mapping changed before observation");
        }
        let now = now_millis()?;
        insert_session(&transaction, key, observed, now)?;
        transaction
            .commit()
            .context("commit agent session observation failed")?;
        Ok(())
    }

    pub fn remove_if_matches(&self, key: &StoreSessionKey, expected: &str) -> Result<bool> {
        key.validate()?;
        validate_nonempty("expected agent session id", expected)?;
        let mut connection = self.lock_connection();
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("begin conditional agent session removal failed")?;
        let removed = match query_session(&transaction, key)?.as_deref() {
            None => false,
            Some(current) if current != expected => {
                bail!("agent session mapping changed before removal")
            }
            Some(_) => {
                delete_session(&transaction, key, expected)?;
                true
            }
        };
        transaction
            .commit()
            .context("commit conditional agent session removal failed")?;
        Ok(removed)
    }

    pub fn disable_agent(
        &self,
        key: &StoreChannelSessionKey,
        agent: &AgentIdentity,
    ) -> Result<bool> {
        key.validate()?;
        agent.validate()?;
        let inserted = self
            .lock_connection()
            .execute(
                "INSERT OR IGNORE INTO channel_session_agent_blocks (
                     channel_name,
                     provider_type,
                     provider_account_identity,
                     channel_session_id,
                     agent_name,
                     backend_type,
                     canonical_workspace,
                     created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    key.channel.configured_name,
                    key.channel.provider_type,
                    key.channel.account_identity,
                    key.session_id,
                    agent.configured_name,
                    agent.backend_type,
                    agent.canonical_workspace,
                    now_millis()?
                ],
            )
            .context("disable agent for resolved channel session failed")?;
        Ok(inserted > 0)
    }

    pub fn enable_agent(
        &self,
        key: &StoreChannelSessionKey,
        agent: &AgentIdentity,
    ) -> Result<bool> {
        key.validate()?;
        agent.validate()?;
        let removed = self
            .lock_connection()
            .execute(
                "DELETE FROM channel_session_agent_blocks
                 WHERE channel_name = ?1
                   AND provider_type = ?2
                   AND provider_account_identity = ?3
                   AND channel_session_id = ?4
                   AND agent_name = ?5
                   AND backend_type = ?6
                   AND canonical_workspace = ?7",
                params![
                    key.channel.configured_name,
                    key.channel.provider_type,
                    key.channel.account_identity,
                    key.session_id,
                    agent.configured_name,
                    agent.backend_type,
                    agent.canonical_workspace
                ],
            )
            .context("enable agent for resolved channel session failed")?;
        Ok(removed > 0)
    }

    pub fn is_agent_enabled(
        &self,
        key: &StoreChannelSessionKey,
        agent: &AgentIdentity,
    ) -> Result<bool> {
        key.validate()?;
        agent.validate()?;
        let blocked = self
            .lock_connection()
            .query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM channel_session_agent_blocks
                     WHERE channel_name = ?1
                       AND provider_type = ?2
                       AND provider_account_identity = ?3
                       AND channel_session_id = ?4
                       AND agent_name = ?5
                       AND backend_type = ?6
                       AND canonical_workspace = ?7
                 )",
                params![
                    key.channel.configured_name,
                    key.channel.provider_type,
                    key.channel.account_identity,
                    key.session_id,
                    agent.configured_name,
                    agent.backend_type,
                    agent.canonical_workspace
                ],
                |row| row.get::<_, bool>(0),
            )
            .context("query resolved agent status failed")?;
        Ok(!blocked)
    }

    pub fn disabled_agents(&self, key: &StoreChannelSessionKey) -> Result<Vec<AgentIdentity>> {
        key.validate()?;
        let connection = self.lock_connection();
        let mut statement = connection
            .prepare(
                "SELECT agent_name, backend_type, canonical_workspace
                 FROM channel_session_agent_blocks
                 WHERE channel_name = ?1
                   AND provider_type = ?2
                   AND provider_account_identity = ?3
                   AND channel_session_id = ?4
                 ORDER BY agent_name, backend_type, canonical_workspace",
            )
            .context("prepare resolved disabled-agent query failed")?;
        statement
            .query_map(
                params![
                    key.channel.configured_name,
                    key.channel.provider_type,
                    key.channel.account_identity,
                    key.session_id
                ],
                |row| {
                    Ok(AgentIdentity::new(
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .context("query resolved disabled agents failed")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("read resolved disabled agents failed")
    }

    fn lock_connection(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn initialize(connection: &mut Connection) -> Result<()> {
        let version = connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .context("read sqlite store schema version failed")?;
        match version {
            0 => create_schema(connection),
            3 => migrate_v3(connection),
            SCHEMA_VERSION => verify_schema(connection),
            version => bail!("unsupported sqlite store schema version: {version}"),
        }
    }
}

fn create_schema(connection: &mut Connection) -> Result<()> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Exclusive)
        .context("begin sqlite schema creation failed")?;
    transaction
        .execute_batch(CREATE_SCHEMA)
        .context("create sqlite store schema failed")?;
    transaction
        .pragma_update(None, "user_version", SCHEMA_VERSION)
        .context("write sqlite store schema version failed")?;
    transaction
        .commit()
        .context("commit sqlite schema creation failed")
}

fn verify_schema(connection: &mut Connection) -> Result<()> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Exclusive)
        .context("begin sqlite schema verification failed")?;
    transaction
        .execute_batch(CREATE_SCHEMA)
        .context("verify sqlite store schema failed")?;
    transaction
        .commit()
        .context("commit sqlite schema verification failed")
}

fn migrate_v3(connection: &mut Connection) -> Result<()> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Exclusive)
        .context("begin sqlite v3-to-v4 migration failed")?;
    let result = (|| -> Result<()> {
        transaction
            .execute_batch(
                "DROP INDEX IF EXISTS agent_sessions_shared_scope;
                 DROP INDEX IF EXISTS agent_sessions_session_scope;
                 ALTER TABLE agent_sessions RENAME TO agent_sessions_v3;
                 ALTER TABLE channel_session_agent_blocks
                    RENAME TO channel_session_agent_blocks_v3;",
            )
            .context("prepare sqlite v3 tables for migration failed")?;
        transaction
            .execute_batch(CREATE_SCHEMA)
            .context("create sqlite v4 schema during migration failed")?;
        transaction
            .execute_batch(MIGRATE_V3)
            .context("copy sqlite v3 data into quarantined v4 identities failed")?;
        transaction
            .pragma_update(None, "user_version", SCHEMA_VERSION)
            .context("write migrated sqlite store schema version failed")?;
        Ok(())
    })();
    if let Err(err) = result {
        transaction
            .rollback()
            .context("rollback sqlite v3-to-v4 migration failed")?;
        return Err(err).context("migrate sqlite store schema from v3 to v4 failed");
    }
    transaction
        .commit()
        .context("commit sqlite v3-to-v4 migration failed")
}

fn query_session(connection: &Connection, key: &StoreSessionKey) -> Result<Option<String>> {
    let channel = key.channel();
    connection
        .query_row(
            "SELECT agent_session_id
             FROM agent_sessions
             WHERE agent_name = ?1
               AND backend_type = ?2
               AND canonical_workspace = ?3
               AND isolation_scope = ?4
               AND channel_name IS ?5
               AND provider_type IS ?6
               AND provider_account_identity IS ?7
               AND channel_session_id IS ?8",
            params![
                key.agent.configured_name,
                key.agent.backend_type,
                key.agent.canonical_workspace,
                key.scope_name(),
                channel.map(ChannelIdentity::configured_name),
                channel.map(ChannelIdentity::provider_type),
                channel.map(ChannelIdentity::account_identity),
                key.session_id()
            ],
            |row| row.get(0),
        )
        .optional()
        .context("query agent session mapping failed")
}

fn insert_session(
    connection: &Connection,
    key: &StoreSessionKey,
    agent_session_id: &str,
    now: i64,
) -> Result<()> {
    let channel = key.channel();
    connection
        .execute(
            "INSERT INTO agent_sessions (
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
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
            params![
                key.agent.configured_name,
                key.agent.backend_type,
                key.agent.canonical_workspace,
                key.scope_name(),
                channel.map(ChannelIdentity::configured_name),
                channel.map(ChannelIdentity::provider_type),
                channel.map(ChannelIdentity::account_identity),
                key.session_id(),
                agent_session_id,
                now
            ],
        )
        .context("insert agent session mapping failed")?;
    Ok(())
}

fn delete_session(connection: &Connection, key: &StoreSessionKey, expected: &str) -> Result<()> {
    let channel = key.channel();
    let removed = connection
        .execute(
            "DELETE FROM agent_sessions
             WHERE agent_name = ?1
               AND backend_type = ?2
               AND canonical_workspace = ?3
               AND isolation_scope = ?4
               AND channel_name IS ?5
               AND provider_type IS ?6
               AND provider_account_identity IS ?7
               AND channel_session_id IS ?8
               AND agent_session_id = ?9",
            params![
                key.agent.configured_name,
                key.agent.backend_type,
                key.agent.canonical_workspace,
                key.scope_name(),
                channel.map(ChannelIdentity::configured_name),
                channel.map(ChannelIdentity::provider_type),
                channel.map(ChannelIdentity::account_identity),
                key.session_id(),
                expected
            ],
        )
        .context("remove agent session mapping failed")?;
    if removed != 1 {
        bail!("agent session mapping changed during removal");
    }
    Ok(())
}

fn validate_nonempty(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(())
}

fn now_millis() -> Result<i64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time is before unix epoch")?
        .as_millis();
    i64::try_from(millis).context("current timestamp does not fit in sqlite integer")
}

fn secure_sqlite_files(path: &Path) -> Result<()> {
    secure_file(path)?;
    for suffix in ["-wal", "-shm"] {
        let mut auxiliary = path.as_os_str().to_os_string();
        auxiliary.push(suffix);
        let auxiliary = PathBuf::from(auxiliary);
        if auxiliary.exists() {
            secure_file(&auxiliary)?;
        }
    }
    Ok(())
}
