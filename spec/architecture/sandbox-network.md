# Sandbox Network

## Scope

The first sandbox network implementation provides rootless TCP interception, policy decisions, and
audit events on macOS.
It is available through the `agora-sandbox` SDK and the `agora-sandbox` command-line binary.

This implementation is an observation and routing layer, not yet a complete containment boundary:

- The runner installs no native macOS process policy. Host Keychain access is unchanged and is
  outside the network and filesystem hooks; Keychain mutations by descendants affect the current
  user's host Keychain.

- `NetworkEnforcement::Intercept` is supported. A covered TCP call fails closed when hook
  configuration, proxy redirection, or CONNECT-preface construction cannot be completed.
  The callback may also deny a covered request before the proxy opens the upstream destination.
- `NetworkEnforcement::Strict` is rejected until a native macOS policy independently denies direct
  external egress while allowing the Agora control and proxy paths.
- `TlsMode::Off` relays TLS as raw TCP and reads only plaintext ClientHello metadata such as SNI.
- `TlsMode::Auto` terminates connections with a valid TLS ClientHello and relays HTTP and other
  non-TLS TCP protocols unchanged. It uses one fixed PEM CA certificate and matching private key
  per run. Explicit CA paths take precedence. Without explicit paths, the run uses
  `<workdir>/ca/ca.crt` and `<workdir>/ca/ca.key`, where workdir is the sandbox configuration's
  persistent work directory and defaults to `~/.agora-sandbox`. Existing files are reused when both
  are present; if either file is absent, a matching pair replaces both selected paths.
- TLS termination verifies the upstream server with native roots and issues one-day DNS or IP leaf
  certificates from the configured CA. DNS names are normalized with a bundled Public Suffix List:
  registrable domains and public suffixes remain exact, while deeper names replace only their
  leftmost label with `*` (`www.baidu.com` becomes `*.baidu.com`, and `bar.foo.co.uk` becomes
  `*.foo.co.uk`). IP addresses remain exact. A 2048-entry LRU caches leaves by normalized identity
  for one hour, so sibling names covered by one wildcard reuse the same certificate. The proxy
  negotiates the upstream ALPN first, then presents the same ALPN downstream. Handshake and
  verification failures are blocked without raw fallback.
- The interception CA is injected into covered macOS `SecTrust` SSL evaluations without modifying
  a user or system Keychain. This transport is internal to TLS interception; there is no public
  independent trust-anchor option.

The term "sandbox traffic" therefore means TCP calls made through covered APIs by a process that
the runner can prepare and that successfully loads the hook. It does not mean all host or kernel
traffic.

## Public API

`agora-sandbox` exposes:

- `SandboxConfig`: network policy, hook-library path, an optional fixed TLS interception CA
  certificate/private-key pair, and the persistent sandbox work directory.
- `SandboxCommand`: program, arguments, environment, and working-directory configuration.
- `Sandbox<C: Callback>`: one sandbox run with a caller-provided asynchronous callback.
- `SandboxOutcome`: child exit status plus generated sandbox and run identifiers.
- `hook_library::materialize`: publish the embedded hook below a selected work directory and return
  its verified path for the existing `SandboxConfig::new` API.
- `generate_tls_ca`: generate or replace a PEM signing CA certificate and matching PKCS#8 private
  key for TLS interception.
- `NetworkConfig`: enforcement mode, TLS mode, domain-inspection timeout, callback timeout,
  upstream connect timeout, and the per-run maximum number of active proxied connections. Domain
  inspection defaults to 500 ms, callback execution defaults to five seconds, the connection limit
  defaults to 256, and zero values are rejected.
- Versioned network, process, and file callback events, `Decision`, proxy route types, and explicit
  `Redact` views under `callback`.

The CLI is a thin adapter with three user-visible subcommands. `run` accepts one configuration file
and one executable command line:

```bash
agora-sandbox run -c sandbox.json \
  -e './target/debug/my-client --endpoint https://example.com'
```

```json
{
  "workdir": "~/.agora-sandbox",
  "tls": "auto",
  "filesystem": {
    "bypass": ["/opt/agora-tools"],
    "local": {
      "encrypt": "encrypted",
      "key": "REPLACE_WITH_A_HIGH_ENTROPY_RANDOM_SECRET"
    },
    "nfs": [
      {
        "type": "smb",
        "dir": "/smb",
        "server": "smb://127.0.0.1:10445/workspace",
        "username": "openclaw",
        "password": "secret"
      }
    ]
  },
  "log": {
    "file": "logs/sandbox.log"
  }
}
```

The configuration rejects unknown fields. Omitted settings retain the previous runtime defaults:
`workdir` is `~/.agora-sandbox`, `tls` is `off`, the local filesystem is `plain`, configured bypass
roots and NFS roots are empty, `/dev` remains an implicit native-passthrough root, and structured
logs append to `<workdir>/runtime/logs/sandbox.log`. A relative `workdir` is
resolved from the configuration file's directory, while a relative `log.file` is resolved from the
resulting workdir; a leading `~` uses `HOME` in either field. The removed `audit` object is rejected
as an unknown field. The configuration must be a regular non-symlink file. Ownership and permission
policy is left to the caller and the operating system; callers storing filesystem keys or remote
credentials should protect the file accordingly.
`tls` accepts `off` or `auto`. The CLI supports `filesystem.local.encrypt` values `plain` and
`encrypted`; plain mode rejects a key, while encrypted mode requires a non-empty
`filesystem.local.key`. `filesystem.bypass` accepts additional absolute host directory roots. Each
configured root and its resolved existing-ancestor alias bypass COW, encryption, tracking, and
filesystem audit; roots overlapping the workdir or an NFS logical root are rejected. An empty JSON
object therefore selects all defaults.

`web` reuses that same configuration loader and resolved `log.file` path:

```bash
agora-sandbox web -c sandbox.json [--no-open]
```

It invokes the current `agora-sandbox` executable with `run`, the validated absolute configuration
path, and the fixed `/bin/bash` command inside a pseudoterminal. The browser can start or stop that
fixed terminal but cannot select another executable, config, log path, listener, or host command.
`--no-open` prints the authenticated loopback URL without launching the default browser. The third
user-visible subcommand, `key migrate`, remains the explicit workdir-based encrypted-key migration
operation described by the filesystem architecture.

Overlapping CLI `run` invocations whose canonical workdir, executable build,
materialized Hook, and default-expanded runtime configuration have the same
semantic SHA-256 identity join one ephemeral per-workspace session. The first
client starts a hidden owner daemon; later clients may run different commands
through the same filesystem, local and NFS Brokers, proxy, execution controller,
audit controller, credentials, `sandbox_id`, and callback-visible `run_id`.
Each client still resolves its executable against its own PATH and current
directory, retains its own ordinary environment, arguments, process group,
terminal descriptors, exit status, and root trace ID, and starts the prepared
child locally. A mismatched build or effective configuration is rejected. The
daemon stops after the final client exits and completes controller shutdown and
encrypted writeback before releasing the final client and `.fs.lock`. The
library-level `Sandbox::run` API remains a one-command runtime and does not join
CLI sessions.

Configured NFS roots are probed asynchronously after their Broker starts, so remote readiness never
delays child startup. The logical root itself is synthetic, but its parent must already be visible as
a directory in the local overlay when the probe starts; a missing or non-directory parent reports
the route as unavailable without attempting the remote connection. An SMB probe then validates a
configured share subpath by statting it and requiring a directory before reporting success; a share
URL without a subpath needs only a successful share connection. The CLI writes one sanitized
structured `nfs` record per root through its JSON logger when each probe completes. The record uses
the configured `log.file` destination or `<workdir>/runtime/logs/sandbox.log` and never enters child
stdout. A failed probe does not stop the run, and later access retries the backend connection.

The CLI contains the hook and automatically materializes it below
`<workdir>/runtime/hook/<md5>/libagora_sandbox.dylib` before constructing `SandboxConfig`. It has no
`--hook-library` option, sidecar lookup, or runtime Cargo dependency; failure to materialize the
verified hook is a startup error. CLI auto mode uses the workdir-relative default CA paths and
generates the pair when needed. SDK callers may supply explicit CA paths; the PEM certificate must
be a signing CA and its public key must match the private key.
The library-level `generate_tls_ca` function creates missing parent directories and writes a new
ten-year signing CA and matching PKCS#8 private key in PEM format. Certificate and key destinations
must differ. Existing destination files are replaced, and both generated files use mode `0600` on
Unix. User-supplied output directories follow normal filesystem path resolution, including symbolic
links; auto-managed CA storage below the workdir retains the controller-managed directory checks
described below. There is no separate CLI certificate-generation subcommand.
The CA private key remains in the host proxy and is never transported to the child. Auto mode
writes a CA-keyed trust bundle containing the interception CA and current native roots in the
owner-only per-run temporary directory, then points `SSL_CERT_FILE`,
`CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE`, `NODE_EXTRA_CA_CERTS`, and `GIT_SSL_CAINFO` at that bundle.
Covered descendant launches restore these protected values even if the caller clears or replaces
its environment. The child inherits stdin, stdout, and stderr. The
`-e` value is tokenized into a program and arguments but is not
implicitly run through a system shell; pipes, redirections, substitutions, and other shell
operators require an explicit shell command. The CLI audit adapter writes one structured `audit`
field through the JSON logger for each validated network connection attempt, intercepted descendant
process execution attempt, and intercepted file open or close attempt. The logger appends JSON Lines
to `log.file`, defaulting to `<workdir>/runtime/logs/sandbox.log`, and creates missing parent
directories using normal filesystem path resolution, including symbolic links. The final log file
itself must not be a symbolic link, and the opened log must be a regular file owned by the effective
user. New logs are mode `0600`; an existing regular log retains its permissions. The same file
contains the unified audit, NFS-status, and lifecycle log stream. Network
records contain `access_time`, `trace_id`, `pid`, the actual intercepted TCP destination IP and port,
the observed domain, and optional `target_host` and `target_port` fields when an HTTP CONNECT
authority supplies a logical target. Process records contain `access_time`,
`trace_id`, PID, PPID, current and requested executables, arguments, current directory, and launch
operation. The root command is started directly by the runner and therefore does not emit a
`process.exec.attempt` record. File records contain `access_time`, `trace_id`, PID, operation,
logical path, and structured open mode. Child stdout remains exclusively available for child output,
and child stderr remains inherited for child diagnostics. Agora records do not enter either stream
after logger initialization. Errors that occur before logger initialization are written directly to
the controller's stderr. The CLI returns the child's exit code. SIGINT and SIGTERM
terminate the active run through the shared process lifecycle. Strict network enforcement remains
unavailable and is not exposed as a CLI option. Filesystem persistence and destructive-reset
behavior are specified in [Sandbox Filesystem And Executable Preparation](sandbox.md).

## Embedded Hook Build And Materialization

`agora-sandbox` remains one public Cargo package. Its ordinary Cargo build produces an rlib and a
standard cdylib; the latter is a build and coverage artifact, not a runtime sidecar dependency. It
lets injected coverage processes share Cargo's crate identity with their test binaries. The
coverage-only initializer verifies that its own symbol belongs to a dylib before activating, so an
rlib linked into the test executable cannot initialize the same process a second time.
`build.rs` performs one guarded inner `cargo rustc --crate-type cdylib` build in a shared nested
target directory beneath the active Cargo target directory for the bytes embedded into consumers.
Different outer build-script instances reuse that dependency cache while each stages its reported
dylib into its own `OUT_DIR`. The inner build uses the same manifest, Cargo
target, and profile, compiles the existing C shim, and excludes the materializer so embedded bytes
cannot recurse. A private build marker prevents the inner build script from launching Cargo again.

The outer build resolves the dylib from Cargo's JSON artifact output, verifies that it contains only
the requested target architecture, and checks the linker's existing ad-hoc signature with
`codesign --verify --strict` without signing or mutating the artifact. It then computes a full-file
MD5, stages the dylib in the outer `OUT_DIR`, and includes those exact bytes and their checksum in
consumers that call `hook_library::materialize`. A packaged standalone executable does not require
or install a dylib beside itself, even though Cargo's target directory contains its normal cdylib
build artifact.

At runtime, `hook_library::materialize(workdir)` publishes the bytes under the checksum-addressed
path while holding `<workdir>/runtime/hook/.lock`. Controller directories and the lock reject
symlinks, non-directory or non-file substitutions, and foreign ownership. A matching regular file
is reused; a same-user regular file with the wrong size, unreadable content, or mismatched content
is replaced through a same-directory, synced temporary file and an atomic rename. The runtime cache
and lock are independent of `<workdir>/fs` and its overlay locks.
SDK callers may instead continue to pass any explicit hook path directly to `SandboxConfig::new`.

## Runtime Flow

The SDK creates one runtime for one command. The CLI creates or joins one
runtime for each set of overlapping compatible commands:

```text
CLI startup
  -> resolve the configured or default workdir
  -> materialize and verify the embedded hook in <workdir>/runtime/hook/<md5>
  -> derive the effective configuration and executable-build identities
  -> connect to the owner-only per-workspace session socket
  -> if absent, elect and start one hidden runtime daemon while holding
     <workdir>/runtime/session-start.lock
  -> ask the daemon to prepare the caller-resolved executable
  -> start the prepared command locally with direct terminal I/O

CLI session daemon / SDK Sandbox::run
  -> validate intercept and TLS policy
  -> validate explicit fixed PEM interception CA paths when configured
  -> use the configured persistent workdir and load or generate its default CA when explicit paths are absent
  -> create an authenticated loopback audit controller for process and file events
  -> in encrypted filesystem mode, create an independent authenticated local content Broker
     that owns one anonymous plaintext vnode per ciphertext inode, returns separate open-state and
     lock-anchor descriptors for each independent open, and updates block-encrypted upper files
  -> create an authenticated loopback execution controller backed by <workdir>/fs
     whose incomplete handshakes expire after one second and whose active connections are capped at 64
  -> resolve the root executable and prepare a persistent native copy only when injection
     restrictions require it
  -> validate the CA/key pair, load native upstream roots, and bind loopback proxy listeners
  -> publish a CA-keyed trust bundle, transport the interception CA DER bytes and bundle path to
     the child as internal runtime configuration, and retain the private key only in the host
  -> inject libagora_sandbox.dylib and immutable per-run control configuration
  -> hook initialization authenticates one inheritable execution stream, one inheritable audit
     stream, and any configured local or remote filesystem Broker streams before ordinary hooks
     become active
  -> for the SDK, start the original or prepared child as a new process-group leader
  -> for the CLI, return protected launch data to the client, which starts that
     command as its own process-group leader

intercepted child posix_spawn/exec
  -> resolve the requested executable
  -> append one trace id and publish process.exec.attempt through the authenticated audit controller
  -> request preparation from the authenticated execution controller
  -> reuse a valid persistent copy or mirror and ad-hoc sign a native-architecture copy beneath
     <workdir>/fs while preserving the executable's canonical absolute path
  -> rebuild the protected control environment and DYLD_INSERT_LIBRARIES from the loaded snapshot
  -> pass the authenticated control descriptors and their anonymous cross-process lock to the new
     image; ordinary requests still prefer fresh connections and use the inherited streams only
     when a nested kernel sandbox denies a new internal connection
  -> invoke the original launch operation with the original or prepared executable

intercepted child open/openat/fopen and close/fclose
  -> resolve and retain the logical path before overlay mapping
  -> derive structured access, create, truncate, append, and exclusive mode fields
  -> publish filesystem.open through the authenticated audit controller before native open
  -> associate successful descriptors with their logical file context
  -> publish filesystem.close before native close and remove the association after success

intercepted child SecTrust TLS evaluation
  -> detect whether the trust object uses an SSL policy
  -> preserve non-SSL trust objects and application-provided custom anchors
  -> defer asynchronous signals while preparing the trust object and add all configured CAs
     alongside built-in anchors for otherwise-default SSL trust
  -> restore the caller's signal mask after preparation
  -> invoke the original synchronous or asynchronous SecTrust evaluator

intercepted child connect/connectx
  -> identify original destination and process
  -> defer asynchronous signals while recursion state is active and encode the original
     destination, process identity, and trace chain in an authenticated HTTP CONNECT preface
  -> release recursion state and restore the caller's signal mask
  -> invoke the original macOS connectx with the preface as initial data
  -> kernel connects the original socket to the matching loopback proxy and orders the preface
     before later application writes
  -> proxy consumes the preface without returning an HTTP response
  -> proxy buffers at most 64 KiB of initial client bytes for up to 500 ms
  -> proxy derives an optional HTTP Host or TLS ClientHello SNI and ALPN
  -> callback receives network.connect.attempt and asynchronously returns Allow, Deny, or Proxy
  -> Deny, callback timeout, or callback task failure closes the local connection without opening
     the upstream destination
  -> Allow opens the original upstream destination asynchronously
  -> Proxy connects to the selected HTTP proxy, performs a standard CONNECT handshake with optional
     Basic Auth, and never falls back to a direct connection
  -> TLS off, or auto with non-TLS input, forwards buffered bytes and relays raw TCP
  -> TLS auto with a valid ClientHello verifies upstream TLS using native roots
  -> normalize the DNS identity with Public Suffix List-aware wildcarding, issue or reuse its
     cached one-day leaf certificate, complete downstream TLS with the selected upstream ALPN,
     then relay decrypted application bytes bidirectionally
  -> any TLS certificate, verification, or handshake failure closes the
     connection without raw fallback
  -> callback receives established or failed and closed events
```

The hook never changes `O_NONBLOCK`, waits for a proxy response, or interposes readiness and I/O
APIs. A nonblocking socket therefore returns the result of the original `connectx`, normally
`-1/EINPROGRESS`, and remains compatible with `poll`, `select`, and `kqueue`. A blocking socket uses
the operating system's normal blocking behavior while connecting to the local proxy.

This rootless proxy boundary cannot make readiness report the remote destination's state. Writable
readiness and `SO_ERROR` describe the connection to the local proxy; a later upstream failure is
observed by the application through subsequent I/O. Audit events still report the actual upstream
attempt and result.

Normal SDK shutdown terminates residual processes in the run's process group. If the future owning
an active SDK command is cancelled or dropped before the normal wait completes, its command guard
sends `SIGKILL` to the whole process group and restores any foreground-terminal handoff; Tokio's
child-only kill-on-drop remains a secondary fallback. A CLI client owns and
terminates only its command's process group; exiting one client does not stop commands belonging to
other leases. After the final CLI lease ends, the session daemon drains active relays for up to one
second and stops the proxy listeners, execution controller, audit controller, local encrypted
Broker, and optional NFS Broker before acknowledging the final release. The Broker shutdown paths
close inherited persistent UDS endpoints before waiting for their
connection tasks, so an idle descendant descriptor cannot keep the session daemon or Tokio blocking
pool alive after the final lease. Prepared executables, directory metadata, and CA material remain
under the configured work directory for reuse; per-run CA-keyed trust artifacts are removed with
their temporary runtime directory. The runtime owner monitors the network listeners, execution controller,
audit controller, local encrypted Broker, and optional NFS Broker while any command is active. An
unexpected service exit notifies every CLI client, which terminates its own process group, instead
of allowing descendants to continue after their interception path has failed. A descendant
that deliberately creates a new session or process group can leave this lifecycle boundary;
preventing that requires the future native sandbox.

The IPv4 and IPv6 listeners share one per-run connection limit. Connections accepted after the
limit is reached are immediately closed before protocol parsing, upstream connection, or audit
publication. This bounds proxy tasks, sockets, and relay memory even when the child opens connections
aggressively.

The private execution-preparation controller applies an independent limit of 64 active loopback
connections. Connections above that limit are closed immediately, and a client that does not finish
its length-prefixed request frame within one second is disconnected. These bounds prevent an
injected or malfunctioning descendant from retaining unbounded controller tasks or file descriptors;
the audit controller independently allows 1024 active connections and applies the same one-second
request timeout.
The per-run token remains the authentication boundary for requests that complete the handshake.

## Hook Coverage

The private `hook` module in the `agora-sandbox` dylib uses the Mach-O `__interpose` section. It
currently covers:

- `connect` for IPv4 and IPv6 stream sockets.
- The simple `connectx` form without source binding, flags, or caller-provided initial data.
- `posix_spawn`, `posix_spawnp`, `execve`, `execv`, and `execvp` for recursive executable
  preparation and hook injection.
- `open`, `openat`, `fopen`, `close`, and `fclose` for overlay mapping and file lifecycle audit,
  together with the path metadata, mutation, current-directory, and directory-enumeration APIs
  described in [Sandbox Filesystem And Executable Preparation](sandbox.md).
- `SecTrustCreateWithCertificates`, `SecTrustEvaluateWithError`,
  `SecTrustEvaluateAsyncWithError`, and the deprecated `SecTrustEvaluate` and
  `SecTrustEvaluateAsync` entry points for optional process-local SSL trust-anchor injection.

Network descriptor inspection and duplication APIs such as `getpeername`, `dup`, and `dup2` are not
interposed. Consequently, `getpeername` on an intercepted socket reports the loopback proxy rather
than the original destination, and duplicated file descriptors do not acquire a separate lifecycle
audit association.

The hook snapshots immutable control configuration when the dylib loads and reads PID and PPID for
each intercepted connection. The runner reads configured trust certificates once and transports
their DER bytes as comma-separated protected base64 runtime configuration; descendants do not
reopen caller-provided paths. The TLS interception private key is retained only by the host proxy.
Process launch interception rebuilds protected sandbox variables from that snapshot. A descendant
cannot accidentally escape by using `clearenv()` or `env -i`; ordinary environment entries still
follow the caller's requested environment. A descendant created by `fork()` without `exec()`
receives its own process identity and PID-prefixed connection id instead of inheriting the parent's
audit identity.

Trust injection is deliberately narrow. It applies only to SSL policies evaluated through macOS
`SecTrust`, keeps built-in roots enabled, and does not replace an application's explicit custom
anchors. It does not modify the login or system Keychain. Auto mode additionally configures common
environment-aware clients through a private CA bundle, including curl, Requests, Node.js, and Git.
Clients using BoringSSL, rustls, a private OpenSSL context, or another verifier that ignores both
`SecTrust` and the configured trust environment still require a client-specific trust mechanism.
Configured `SecTrust` preparation is fail-closed for SSL evaluation, while an absent trust-anchor
configuration preserves the original API behavior.

When stdin is the runner's foreground terminal, the root child process group becomes the terminal
foreground group for the duration of the run and the runner restores the original group before
returning. This lets an interactive command such as `/bin/bash` receive terminal input and job
control signals instead of being suspended by the kernel.

Non-stream and non-IP sockets bypass the hook. Calls to the run's own proxy addresses bypass it to
avoid recursion. A thread-local guard rejects unexpected recursive covered calls. Catchable
asynchronous signals are deferred while that guard and CONNECT-preface preparation state are live,
then restored before the original potentially blocking `connectx`; a signal-driven non-local
transfer therefore cannot poison later network calls, and native blocking behavior remains
interruptible. The hook does not use flat namespace mode; original functions are obtained from the
interposition table's replacee pointers.

Covered interception is fail-closed. Missing or invalid run configuration, unavailable internal
`connectx`, CONNECT-preface encoding failure, unexpected hook recursion, and complex `connectx`
calls with source binding, flags, or caller-provided initial data return an error without connecting
to the original destination. Unsupported complex `connectx` forms currently return `EACCES`.

The hook has no best-effort network or audit fallback path. A failure that occurs before the proxy
accepts a valid CONNECT preface therefore produces no callback event. The execution controller uses
an authenticated loopback connection only to prepare descendants, while the independent audit
controller receives process and file events. Audit-controller unavailability fails the covered
launch or file operation closed.

The executable preparation path follows the architecture used to compile `agora-sandbox`. A copied
executable mirrors its canonical source path beneath `<workdir>/fs`; for example, `/usr/bin/curl`
becomes `<workdir>/fs/usr/bin/curl`. Same-named executables from different directories therefore do
not collide. An
`aarch64` build selects an `arm64` slice; when only `arm64e` is available, it extracts that slice and
rewrites the Mach-O subtype to `arm64`. An `x86_64` build selects the `x86_64` slice. Universal
binaries are thinned to that selected architecture before the original signature is replaced with
an ad-hoc signature. Executables without dyld restrictions and scripts remain at their canonical
original paths. Shebang scripts are launched through their declared interpreter, and that interpreter
is independently prepared when required. Prepared copies are reused across runs only when the
executable exists and its directory metadata entry matches the source device, inode, size,
timestamps, and mode. Executable preparation does not hash the complete source. The original
executable is never modified.

After Hook initialization, filesystem APIs and explicit runtime `dlopen` or `dlopen_preflight`
calls derived from a relocated executable path re-enter the logical VFS. Runtime loading supports
lower and plain-upper native files; encrypted-upper and NFS inputs remain unsupported because dyld
requires a stable native plaintext path. Remaining coverage gaps exist outside that boundary:

- statically linked code or direct system calls that bypass interposed APIs;
- process launch APIs outside the covered spawn and exec family;
- executables that cannot run correctly after copying or ad-hoc signing because they depend on
  their original code identity, dependencies loaded before Hook initialization, or an Apple-private,
  developer, team, application-group, Keychain-group, rootless, or other identity-bound entitlement;
  executable preparation removes those invalid identity-bound entitlements from only the copied
  image while preserving ordinary entitlements and the original source file;
- executables without a slice compatible with the sandbox build target;
- processes that do not load the hook successfully;
- TLS clients that use neither the interposed `SecTrust` APIs nor the injected trust-bundle
  environment.

These startup-level gaps cannot be blocked or reported by the hook because no hook code runs.
Strict egress enforcement still requires a broader independent native sandbox layer.

## Proxy Protocol

The `protocol` module is private to `agora-sandbox` and shared by its host and hook code. Intercepted
connections begin with an HTTP/1.1 CONNECT preface of at most 16 KiB. Protocol version 7 carries a
per-run random bearer token, connection id, original destination, process identity, hook operation,
and the single comma-separated trace chain in `Agora-Trace-Id`. Sandbox and run ids stay in the host
controller context and are not echoed through the child-controlled wire protocol. Domain data is
also derived by the host proxy from relayed bytes.

The proxy consumes the CONNECT preface and sends no HTTP status line, so protocol bytes are never
exposed to the application. This transparent behavior is the only supported protocol path; direct
standard-proxy clients are not part of the SDK.

The host rejects invalid credentials or protocol versions before inspecting application data,
opening an upstream connection, or publishing events.

The token prevents accidental cross-run traffic and requests from processes that do not know the
run environment; it is not a tamper-proof identity mechanism against the sandboxed process itself.
The hook and application share one address space, and the application can inspect its environment
or speak the proxy protocol directly. Callback events must therefore be treated as operational
telemetry rather than cryptographic evidence. Trusted process identity and tamper-resistant audit
require a future host-native boundary that derives identity independently of child-supplied data.

## Callback Contract

`Callback::on_event` receives an owned, versioned `Event` and asynchronously returns `Decision`.
Event schema version 9 contains `Event::Network(NetworkEvent)`, `Event::Process(ProcessEvent)`, and
`Event::File(FileEvent)`.
For `network.connect.attempt`, `Decision::Allow` routes directly, `Decision::Deny` blocks the
connection, and `Decision::Proxy` selects a typed proxy route. The only implemented route is
`Proxy::Http`, containing an address and optional Basic Auth username and password. The address is
an authority in `<host>:<port>` form; DNS names and bracketed IPv6 addresses are accepted. Decisions
returned for process, file, or network notification events are ignored.

The network path publishes:

- `network.connect.attempt`
- `network.connect.denied`
- `network.connect.established`
- `network.connect.failed`
- `network.connection.closed`

Events include sandbox and run ids, connection id and sequence where applicable, PID, PPID,
executable, the XFF-style `trace_id` string, destination, protocol-derived domain, applied decision,
result, and close metrics. The attempt event has no applied decision because its callback result is
the decision. A denied event contains that result; successful connection events contain the applied
`Allow` or `Proxy` decision.
The loopback peer of the local proxy is intentionally not exposed as the original connection source.
The event schema records `http_host`, `tls_sni`, normalized `domain`, and a `domain_source` of
either `http_host` or `tls_sni`. `destination_ip` and `destination_port` always describe the actual
intercepted TCP endpoint. When a valid HTTP CONNECT request contains a DNS authority with an explicit
port matching its normalized Host observation, `target_host` and `target_port` separately describe
that logical target. Other HTTP requests and TLS SNI observations leave both target fields empty;
the controller never infers port 443 from a hostname. Established, failed, and closed events for
observed TLS include the TLS policy, an outcome of `terminated`, `passthrough`, or `failed`, and the
negotiated ALPN when known.

The process path currently publishes `process.exec.attempt` for intercepted descendant `posix_spawn`,
`posix_spawnp`, `execve`, `execv`, and `execvp` operations. A process event contains sandbox and run
ids, the trace chain, current PID/PPID/executable, requested executable, arguments, current directory,
operation, and result. The hook records at most 256 arguments and replaces the omitted argument tail
with `[truncated]`. Process event delivery is independent of execution preparation protocol version 6,
which carries authentication plus either a persistent-control `ping` or an executable path. A
valid initial ping converts that connection into a multi-request inherited control stream; an
ordinary preparation request remains a one-request connection. The root command is outside this
process-event path because the runner launches it before any hooked descendant launch occurs.

The filesystem path publishes `filesystem.open` and `filesystem.close` through local audit protocol
version 4. A valid initial `ping` authenticates the inheritable multi-request control stream without
publishing an event and exempts that stream from the ordinary 30-second idle timeout. The first event
on a fresh ordinary connection receives an authentication and validation response before its callback
runs. Later events on that authenticated connection are written without waiting for a response; the
bounded kernel socket buffer supplies backpressure while the controller invokes callbacks in the
connection task. Each event request includes a random request ID, and that ID becomes the callback
event ID. A transport retry reuses the encoded request, but the controller deliberately keeps no
deduplication cache, so an ambiguous retry may invoke the callback more than once with the same event
ID. Events include the logical path before overlay mapping, structured open mode, process identity,
and the same trace chain used by process and network events. Successful opens register their native
descriptor so close events retain the original path and mode. A reported transport or initial
validation failure fails the intercepted operation closed; a successful asynchronous write does not
guarantee callback completion if the controller terminates with buffered events. Orderly controller
shutdown stops accepting connections and gives existing connection tasks 250 ms in total to drain
before aborting the remainder; an abnormal termination or a larger backlog can still lose accepted
tail events. Callback decisions for these audit-only events are ignored.

Domain observation is deliberately narrow:

- The proxy records only the first domain observed on each TCP connection.
- Plain HTTP uses the first complete HTTP/1 request `Host` header.
- HTTPS uses SNI from a TLS ClientHello, including a ClientHello split across TCP reads.
- Inspection is bounded to the first 64 KiB of initial client bytes and 500 ms by default. Buffered
  bytes are forwarded unchanged after an `Allow` or `Proxy` decision.
- DNS lookups are not hooked or correlated with destination addresses.
- Generic TCP, cleartext HTTP/2, IP-only HTTP hosts, TLS without visible SNI, and ECH-hidden names
  produce an attempt event with no domain.

The proxy waits for inspection to complete, reach its byte limit, or time out before publishing the
attempt event. This guarantees that an HTTP Host or TLS SNI policy decision happens before an
upstream connection. Server-first and otherwise silent protocols wait for the bounded inspection
timeout and are then evaluated with no domain.

Callback delivery is asynchronous and deliberately has no built-in persistence, application-level
queue, batching, or remote transport. Network callbacks run in connection tasks; process and file
callbacks run in audit connection tasks after the bounded transport and retry behavior described
above.
Decisions are enforced only for `network.connect.attempt`; decisions returned for process, file,
and network notification events are ignored. An attempt
callback that exceeds `callback_timeout` is denied fail-closed. The embedding application owns
policy lookup, serialization, and storage. The SDK also provides `NoopCallback`, which always returns
`Decision::Allow`.

`NetworkEvent` and `Decision` intentionally do not implement direct serialization. Callers must
serialize `value.redacted()` through the `Redact` trait. The redacted view retains the HTTP proxy
address, Basic Auth username, and a `[redacted]` password marker without exposing the original
password. `BasicAuth` also redacts its password in `Debug` output.

For `Decision::Proxy`, the controller resolves and connects the configured HTTP proxy address, sends
`CONNECT <original-destination>`, and adds `Proxy-Authorization: Basic ...` when credentials are
configured. Only a complete 2xx response establishes the route. Proxy DNS failure, TCP failure,
timeout, malformed or oversized response, `407`, and every other non-2xx response fail closed. The
controller does not retry the original destination directly. Bytes received after the CONNECT
response head are preserved as the first server-to-client tunnel bytes. SOCKS proxies are not
implemented.

The binary's callback adapter is intentionally narrower than the SDK event contract. It emits one
compact record immediately for each `network.connect.attempt`, `process.exec.attempt`,
`filesystem.open`, and `filesystem.close`, and always returns `Decision::Allow`. A domain that cannot
be derived from HTTP Host or TLS SNI is serialized as `null`. All record types include the same
`trace_id`, allowing process launches, file activity, and network attempts to be correlated.

## Root Privileges

The implemented interception, executable preparation, and callback path requires no root
privileges:

- the root and recursively launched dynamic Mach-O executables matching the sandbox build target
  are copied only when injection restrictions require it, mirrored under persistent
  `<workdir>/fs`, normalized with the system `lipo` tool when needed, and ad-hoc signed with the
  system `codesign` tool;
- dylib injection is configured on each prepared child process;
- both proxy listeners bind ephemeral loopback ports;
- direct upstream and selected HTTP proxy TCP connections use the current user's permissions;
- optional TLS trust is scoped to the injected process tree and requires no Keychain write or trust
  prompt; CA-keyed trust bundles and Java trust stores are removed with the per-run temporary
  directory, while the automatically managed CA pair remains under `<workdir>/ca` for reuse.
- no native process profile is installed; descendants retain the current user's normal host
  Keychain access;
- fixed-CA leaf issuance, upstream verification, and TLS relay run in the unprivileged host proxy;
  the CA key file requires only the current user's read permission.

Future strict egress enforcement and any chroot-based filesystem mode are separate capabilities and
may require stronger privileges or platform-specific entitlements. They must not weaken or be
silently conflated with this rootless callback boundary.
