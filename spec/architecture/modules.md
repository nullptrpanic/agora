# Project Modules

Agora is a Rust workspace with four top-level crates.

## `agora-core`

Type: library crate.

Responsibility:

- Shared domain types that are stable enough to be used across binaries.
- Logger implementation shared by local binaries.
- Process lifecycle monitoring, structured shutdown reasons, and shutdown callbacks shared by binaries.
- Future common models such as task identifiers, run identifiers, run events, artifact references, and status enums.

Rules:

- Keep `agora-core` small.
- Do not place unstable adapter traits here too early.
- Do not depend on `agora-node`, `agora-server`, or `agora-sandbox`.
- Promote types into core only after two or more modules genuinely need them, or when they define an explicitly shared process boundary.
- Keep lifecycle signal listeners generic and instance-based; only shutdown callback registration and the weak `ShutdownGuard` singleton are process-global.
- Keep `signal` and `shutdown` independent and compose them only from their parent lifecycle module.

## `agora-node`

Type: library plus binary crate.

Responsibility:

- Local node daemon.
- Channel adapters.
- Backend agent implementations.
- Shared command-process IO used by command-based agents.
- Local SQLite state.
- Persistent channel-session-to-agent-session mappings.
- Invocation of real agent commands.
- Streaming run events back to channels.

Library responsibility:

- Expose node-local modules for integration tests and the node binary.
- Keep agent, channel, config, daemon, and store boundaries testable without moving them into `agora-core`.

Binary responsibility:

- Parse CLI flags.
- Initialize logging and async runtime.
- Load config and call library orchestration functions.

Current submodules:

- `agent`: backend execution implementations, backend-specific session resume and deletion, per-run cancellation ownership, and the optional shared `command` process-IO helper.
- `channel`: task intake and event output adapters.
- `daemon`: channel routing, recursive node-command registration and execution, unified process-local execution scheduling, FIFO-protected session reset coordination, opaque session mapping coordination, and agent orchestration without backend-specific execution knowledge.
- `i18n`: centralized user-facing node copy, currently provided by `zh_cn`; channels own presentation markup while daemon and channel modules reuse the same wording.
- `instance`: fixed `~/.agora` state paths, private Unix permissions, and the daemon-lifetime single-instance lock.
- `store`: SQLite-backed identity-qualified backend session mappings and conversation-scoped Agent controls.

Rules:

- Keep channel and agent traits here for now.
- Each agent implementation owns its backend-specific execution protocol and interprets opaque session ids supplied by its caller.
- `ExecutionScheduler` owns the single process-local entry for each queued or running task. Its ticket combines FIFO admission and an `AgentRunControl`, removes itself on drop, and is the shared boundary used by stop, reset, and shutdown.
- Keep the store independent of channel and agent implementations; it must persist neutral identifiers rather than backend protocol objects.
- Derive one shared or channel-session isolation boundary per Agent run. Use its minimal key for the process-local FIFO and its configuration-qualified key for persistence, publish Queued with the live number of preceding tasks, and allow different scopes to run concurrently subject to the normalized-workspace boundary.
- Keep `agent::command` limited to child-process startup, stdin, stdout, stderr, and exit status; it must not know agent protocols or channels.
- Keep `daemon::command::registry` independent of agents, channels, SQLite, and daemon runtime state. Each command module owns its tree definition and handler functions; the command executor invokes the registered function without a central command enum or dispatch match.
- Keep fixed user-facing copy in `i18n`; do not move channel markup, protocol fields, logs, internal errors, or agent-produced content into the copy catalog.
- Do not split agent or channel code into separate crates until reuse pressure is real.
- Prefer enum-based adapter aggregation over `Box<dyn Trait>`.
- Do not use `async_trait`; use `fn -> impl Future` where asynchronous traits are needed.
- Keep `src/lib.rs` thin. It should expose modules, not become a second application entrypoint.

## `agora-server`

Type: binary crate.

Responsibility:

- Server-side control plane.
- Task creation and assignment.
- HTTP polling endpoints for nodes.
- WebSocket run event ingestion.
- Future workspace, run, and artifact APIs.

Current status:

- Skeleton binary.
- No protocol implementation yet.

Rules:

- Server APIs should speak structured tasks and run events, not raw process or terminal output.
- Server should not know agent CLI internals.
- Server should not become the central reasoning agent.

## `agora-sandbox`

Type: library plus binary crate.

Responsibility:

- Typed local sandbox startup API.
- Per-run network interception controller and raw TCP proxy.
- Versioned, caller-owned asynchronous policy and event callback contract.
- Thin command-line delivery using `agora-sandbox run -c <config> -e '<command>'`.
- A binary-private `web` command that opens one fixed interactive sandbox terminal with a live
  browser audit timeline.
- Ephemeral per-workspace CLI session discovery, launch preparation, and final-lease runtime
  ownership; each foreground client retains its own terminal and process group.
- Rootless TLS termination with an explicit or workdir-local fixed CA.
- Process-tree-scoped filesystem overlay, encrypted storage, remote filesystem roots, and native
  operation interception on supported platforms.

Current status:

- macOS rootless TCP interception is implemented in intercept mode.
- The SDK prepares the root dynamic Mach-O executable matching its build target in a persistent
  `<workdir>/fs` cache only when injection restrictions require a copy, starts IPv4/IPv6
  loopback proxies, and injects the private hook dylib. `SandboxCommand` accepts standard
  `Stdio` configuration and defaults to inherited stdin, stdout, and stderr. `Sandbox::spawn`
  returns a live `SandboxChild` whose public stdin, stdout, and stderr fields follow the standard
  process API shape while the wrapper retains the sandbox runtime and process-group lifetime. Its
  `wait` method closes an owned piped stdin, shuts the runtime down once, and returns the cached exit
  status on later calls, matching the standard child lifecycle where the sandbox-specific runtime
  ownership permits it.
  `Sandbox::run` is the foreground convenience path that waits for that child and returns its exit
  status. The caller owns pipe draining, streaming, output limits, and execution timeout policy.
  Dropping a live child terminates its complete process group. Sandbox initialization and execution
  errors remain fail-closed rather than falling back to an unsandboxed command.
- `runner/runtime.rs` owns reusable filesystem, local and NFS Broker, network, execution, and audit
  controller lifetime plus command preparation. The public SDK facade creates one such runtime for
  one command. `session/{startup,protocol,server,client}.rs` lets overlapping compatible CLI
  invocations share one runtime through a bounded owner-only UDS while commands are spawned by
  their respective foreground clients. The hidden daemon holds `.fs.lock` and the startup-election
  lock until the final lease has completed shutdown; no idle or persistent daemon is retained.
- The network hook path interposes `connect` and simple `connectx`. It invokes the original
  `connectx` with an authenticated transparent CONNECT preface as initial data, without changing
  `O_NONBLOCK` or waiting for a proxy response. Covered interception failures are blocked instead
  of falling back to the original destination. Optional process-local CA trust is applied only to
  SSL evaluations made through macOS `SecTrust`; it does not modify Keychain state or cover
  independent TLS stacks.
- The proxy inspects bounded initial client bytes for HTTP Host or TLS SNI before opening upstream,
  then asynchronously asks the caller's `Callback` to allow, deny, or route the connection through
  an HTTP CONNECT proxy with optional Basic Auth. Callback timeout and proxy failure deny the
  covered connection without direct fallback. Later connection events use the same callback for
  audit.
- TLS `auto` uses a fixed PEM CA certificate/private-key pair. Explicit paths take precedence;
  otherwise the run reuses or generates the pair under the sandbox workdir. The host verifies
  upstream TLS with native roots, issues one-day leaves using Public Suffix List-aware wildcard
  identities, caches normalized identities for one hour, preserves the negotiated ALPN, and relays
  decrypted bytes. The CA private key never enters the child.
- The hook also interposes the supported `posix_spawn` and `exec` family so an explicit shell and
  its descendants remain injected recursively. Restricted dynamic Mach-O executables are mirrored
  by canonical source path under `<workdir>/fs`, processed for the sandbox build architecture,
  ad-hoc signed, and reused across runs when their recorded source identity still matches. Process launch
  attempts and intercepted file opens and closes are published through an authenticated audit
  controller, then delivered through the same callback as network events with the shared trace
  chain. File events retain the logical pre-overlay path and structured open mode from open through
  close. A run terminates residual members of its process group but retains prepared copies.
- The filesystem VFS owns overlay namespace, short publication locking, lock-serialized
  runtime-visible metadata updates with an append fast path for new whiteouts, direct non-journaled
  rename, per-backing namespace leases,
  and logical Unix mode authorization. Namespace changes deliberately do not provide
  abnormal-exit or power-loss recovery. In
  encrypted mode, an independent authenticated parent-side filesystem Broker owns each open content
  container and one shared anonymous plaintext vnode per ciphertext inode. Every independent open
  receives separate anonymous offset/flag state plus a separately opened descriptor for the inode's
  shared lock anchor. The Broker owns completed dirty ranges on that shared inode, batches them
  briefly, allows independent ordinary plaintext writes to overlap, and serializes ciphertext
  publication and the append/materialization/truncation operations that require exclusivity. Busy protocol
  acquisition returns immediately so inherited control-stream locks are released before a bounded
  client retry; closed fork-retention handles remain bounded. The libc hook selects
  real or effective credentials, virtualizes ordinary and positioned data I/O over the per-open
  state, preserves macOS guarded-descriptor and shared-mapping semantics, and adapts results without
  duplicating permission or encryption policy. Its narrow `control` module owns pre-authenticated
  inheritable fallback streams for internal execution, audit, and filesystem services; the service
  clients and controllers retain all protocol and operation ownership. Synchronous workspace and
  key-migration storage work runs on blocking workers behind the public asynchronous runner API.
  Plain mutating opens retain a per-file staging lease through native descriptor creation and
  metadata publication so concurrent CLI clients cannot reconcile a staged upper file away without
  extending the global VFS transaction; encrypted opens continue using that lease for the lifetime
  of their shared Broker vnode.
- The `nfs` module owns protocol-backed network filesystem roots. Its generic storage trait and
  authenticated per-run Broker are independent of the hook; SMB2/3 is the first backend under
  `nfs/backend/smb`. `nfs/backend/mod.rs` exposes only the protocol-neutral storage boundary to
  the rest of `nfs`; SMB storage, sessions, path mapping, and error adaptation remain private to
  that backend. The existing `SmbRemoteConfig` public API is re-exported without exposing its
  implementation module. Each configured NFS root has an independent backend session holder and a
  non-blocking startup connection probe. The controller exposes sanitized connection results as
  status events, while the runner sends structured records through the same logger as CLI audit;
  a failed probe does not stop the run.
  An NFS root is the highest-priority namespace layer ahead of overlay
  upper and lower state, while remote objects still bypass COW storage without creating a host
  mount. The parent owns backend credentials and sessions, while the hook receives only route ids,
  a socket path, and a per-run token and operates through anonymous regular-file descriptors and
  short-lived opaque empty directory anchors. Remote requests use replayable request IDs and
  explicit resource claims. Directory enumeration crosses the SMB, generic Broker, and hook
  boundaries incrementally through bounded query pages and a descriptor-backed stream. The generic
  Broker owns file, directory, and one absolute operation deadline; a timed-out backend operation
  resets only its configured root's session state. SMB writeback uses an opaque staged sibling,
  cooperative per-target lock, baseline identity check, and atomic replace; transport ambiguity
  reconnects the private backend session before resolution or retry. The local encrypted
  Broker and NFS Broker share only private IPC
  framing; their protocols, handles, and synchronization policies remain independent. A Broker
  failure is monitored alongside the other run services.
- The CLI session daemon renders one structured JSON Lines log record per network connection
  attempt, intercepted descendant process execution attempt, and intercepted file open or close.
  `log.file` selects the unified log destination and defaults to
  `<workdir>/runtime/logs/sandbox.log`, leaving every client's child stdout and stderr inherited
  without Agora records. Its callback always allows requests.
- The binary-private `web` module starts one fixed
  `agora-sandbox run -c <validated-config> -e /bin/bash` child by invoking the current executable
  on a pseudoterminal. It owns the embedded Runtime Trace assets, authenticated loopback WebSocket
  transport, bounded terminal replay, and normalized presentation of process, file-open/file-close,
  and network records from the configured compact JSON Lines log. One binary-private audit-log
  model is shared by the CLI logger and viewer parser, so their record schema cannot drift. The
  module receives only the already resolved config path and log path from the binary entrypoint; it
  is not exported by the sandbox library and no runner, hook, filesystem, or session module depends
  on it.
  The HTTP listener uses an operating-system-selected IPv4 loopback port. A high-entropy
  per-process token, exact `Host`, same-origin `Origin`, and one-controller lease protect terminal
  input from ordinary cross-origin access; this is not protection from another process already
  running as the same operating-system user. The compact sandbox log remains the durable audit
  source. Each terminal session starts following at the current log end, buffers incomplete JSON
  Lines records, follows truncation or replacement, and bounds only in-memory replay and
  presentation history. Network presentation is limited to the recorded domain or target plus
  destination IP and port; it does not infer URLs, bodies, file content, or execution results.
  Browser messages carry only PTY bytes, bounded terminal
  dimensions, Stop/Start, and presentation reset requests; they cannot select a shell, executable,
  config, log path, or arbitrary host launch vector. Browser assets, including pinned xterm.js
  distributions and licenses, are embedded in `agora-sandbox`, so the command requires neither a
  second binary, CDN, nor Node.js runtime. Each audit-log poll is published to the browser as one
  trace batch capped to the same bounded trace-history limit while PTY output remains immediate. A lagged browser subscription is atomically
  replaced before one bounded replay, so stale broadcast backlog cannot trigger repeated terminal
  resets. Historical replay never forwards xterm-generated device responses back into the live
  PTY. Live trace batches append only their changed rows; reconnects and filter changes rebuild the
  bounded timeline, and unchanged terminal dimensions are not sent back to the PTY.
- The interception CA is trusted by covered macOS `SecTrust` SSL evaluations and by common
  environment-aware clients through a CA-keyed trust bundle containing the interception CA and
  current native roots. TLS stacks that ignore both mechanisms require their own trust
  configuration. Strict egress enforcement remains unimplemented and is rejected during validation.

Rules:

- Keep sandbox concerns separate from channel and agent orchestration.
- Avoid making `agora-node` depend on sandbox internals until the sandbox API is clear.
- Keep callback delivery asynchronous, owned, and storage-free. The caller owns access policy and
  any queueing, retry, file, database, or remote audit delivery it needs. Event serialization is
  explicit through `Redact`; proxy passwords must never appear in serialized or debug output.
- Do not describe dylib interception as a complete security boundary. Strict mode requires an
  independent native egress-denial layer.
- Keep proxy endpoints and credentials private to one run, authenticate every request, and reject
  mismatched protocol versions or credentials.
- Keep the wire protocol and compile-time-selected platform hook as private `protocol` and
  `platform/*/hook` source modules inside `agora-sandbox`; do not expose them as workspace crates
  or public integration APIs.
- Keep backend credentials, endpoints, and protocol clients in the parent-side `nfs` module. New
  remote protocols implement the `nfs` storage trait; they must not add mounts, protocol code, or
  credentials to the injected hook.
- Keep executable preparation and its authenticated loopback control protocol private to the
  `execution` source module. Never modify the original executable. Keep persistent prepared copies
  under `<workdir>/fs` and validate them through the directory-local versioned `.metadata` state.
  Persistent state is removed only by explicitly deleting the work directory outside sandbox
  startup; there is no `agora-sandbox clean` command.
- Keep workspace setup, encrypted storage, and key migration synchronous internally. Dispatch them
  at the async runner boundary instead of marking blocking filesystem operations `async`.
- Build the `agora-sandbox` library as both `rlib` for SDK callers and `cdylib` for macOS injection.
- Keep `web` binary-private and one-way: `main.rs` may pass resolved inputs into it, but the library,
  runner, hook, filesystem, session, and root config modules must not depend on Web implementation
  types. Keep its server and PTY dependencies behind the default-enabled `web` feature so the
  no-default-features inner Hook build does not compile them. Keep terminal execution behind the
  fixed current-executable sandbox CLI boundary. Do not add a browser API that accepts a shell,
  executable, config, log path, or arbitrary host launch vector. Treat terminal output and audit
  records as potentially sensitive; do not persist them or the session token in cookies, local
  storage, or a second viewer-owned log.

## Dependency Direction

Expected dependency direction:

```text
agora-node    -> agora-core
agora-server  -> agora-core
agora-sandbox -> agora-core
agora-core    -> external crates only
```
