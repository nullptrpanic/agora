# Sandbox Executable Passthrough and Entitlement Allowlist

## Problem Statement

On macOS, running CLI tools such as `lark-cli` inside `agora-sandbox` fails when they read secrets from the macOS Keychain via `/usr/bin/security find-generic-password -w` (`exit 5`: `keychain access blocked`).

Root-cause analysis shows two independent layers:

1. **Restricted entitlements survive ad-hoc re-signing (`amfid Code=-424` / `SIGKILL`, exit 137)**
   - `/usr/bin/security` is SIP-protected (`SF_RESTRICTED`) and carries four Apple entitlements:
     - `com.apple.authorization.smartcard.override`
     - `com.apple.keystore.keybag.create`
     - `com.apple.private.CoreAuthentication.SPI`
     - `com.apple.private.platformsso.security`
   - `ExecutableStore::requires_original_signing_identity` previously used a blocklist that missed `com.apple.keystore.*`, `com.apple.authorization.*`, and dozens of other restricted Apple entitlements present across `/usr/bin/*` and `/bin/*`.
   - Ad-hoc re-signing an overlay copy while retaining any restricted entitlement causes macOS `amfid` to kill the process on launch (`Error Domain=AppleMobileFileIntegrityError Code=-424`).
2. **Keychain per-app ACLs require the original Apple signing identity**
   - Even after stripping every entitlement from an ad-hoc copy of `/usr/bin/security`, Keychain metadata queries succeed while secret reads (`-w`) fail or prompt (`exit 128`) because Keychain item ACLs bind to Apple's signature for `com.apple.security`.
   - `filesystem.bypass` (`native_passthrough_roots`) intentionally scopes only filesystem I/O hooks and does not bypass executable preparation or `DYLD_INSERT_LIBRARIES` injection.

---

## Design

### 1. Strict Ad-Hoc Entitlement Allowlist and Cache Invalidation

In `crates/agora-sandbox/src/execution/store.rs`:

- Replace the blocklist in `ExecutableStore` with a strict allowlist (`is_adhoc_safe_entitlement`) that retains only standard, non-restricted macOS entitlements safe under ad-hoc signatures:
  - `com.apple.security.cs.*` (Hardened Runtime exceptions such as `allow-jit`, `allow-unsigned-executable-memory`, `allow-dyld-environment-variables`, `disable-library-validation`, `disable-executable-page-protection`, `debugger`)
  - `com.apple.security.get-task-allow`
  - Standard non-restricted App Sandbox capabilities:
    - `com.apple.security.app-sandbox`
    - `com.apple.security.automation.apple-events`
    - `com.apple.security.hypervisor`
    - `com.apple.security.print`
    - `com.apple.security.scripting-targets`
    - `com.apple.security.smartcard`
    - `com.apple.security.assets.*`
    - `com.apple.security.device.*`
    - `com.apple.security.files.*`
    - `com.apple.security.network.*`
    - `com.apple.security.personal-information.*`
    - `com.apple.security.temporary-exception.*`
- Every other entitlement (including `com.apple.private.*`, `com.apple.developer.*`, `com.apple.rootless.*`, `com.apple.keystore.*`, `com.apple.authorization.*`, `com.apple.system-task-ports.*`, `com.apple.security.application-groups`, `com.apple.security.iokit-user-client-class`, `com.apple.security.hardened-process*`, `com.apple.security.exception.*`, `platform-application`, `keychain-access-groups`, `application-identifier`, etc.) is stripped.
- When all entitlements are stripped, `sign_executable_copy` invokes `codesign --force --sign - --timestamp=none --preserve-metadata=identifier` directly without generating an empty plist or passing `--entitlements`.
- Bump `PREPARED_FILE_CACHE_VERSION` in `crates/agora-sandbox/src/filesystem/overlay.rs` from `2` to `3` so previously cached `prepare-v2` copies with stale entitlements are rebuilt automatically.

### 2. Configurable Execution Passthrough Modes and Explicit Allowlist

To avoid forcing users to hardcode every Apple-signed helper path (while still supporting explicit exact-path overrides), execution policy is exposed through two orthogonal fields under `execution`:

```json
{
  "execution": {
    "mode": "strict",
    "bypass": []
  }
}
```

SDK equivalents on `SandboxConfig`:

- `SandboxConfig::with_execution_mode(ExecutionMode)` / `SandboxConfig::execution_mode()`
- `SandboxConfig::with_native_executable_passthrough(path)` / `SandboxConfig::native_executable_passthroughs()`

#### Modes (`ExecutionMode`)

| Mode | JSON value | Behavior |
| --- | --- | --- |
| `ExecutionMode::Strict` (default) | `"strict"` | Prepares and ad-hoc re-signs all SIP / dyld-restricted executables so they remain inside the hook boundary. Only host lower executables matching `execution.bypass` run natively without re-signing or injection. |
| `ExecutionMode::Restricted` | `"restricted"` | Automatically passes through host lower executables that would require copying/re-signing **and** carry restricted (non-allowlisted) entitlements—such as `/usr/bin/security`, `defaults`, `codesign`, `osascript`—plus any explicit `execution.bypass` entries. SIP/Hardened binaries without restricted entitlements (`/bin/sh`, `/bin/bash`, `/usr/bin/curl`, `/usr/bin/git`, `/usr/bin/python3`, etc.) remain copied, re-signed, and hooked inside the sandbox. |
| `ExecutionMode::Bypass` | `"bypass"` | Passes through all host lower executables that would otherwise require copying and ad-hoc re-signing (SIP-protected or `CS_DYLD_RESTRICTED`), plus any `execution.bypass` entries. Directly injectable binaries (unrestricted CLIs, shebang scripts, and sandbox COW binaries) remain hooked. |

#### Validation Rules

- `execution.bypass` entries must be normalized absolute file paths (not `/`), must not point to an existing directory, must not be `/usr/bin/open`, and must not overlap `<workdir>` or any configured `filesystem.nfs` logical root (comparing both normalized paths and canonical existing-ancestor aliases).
- Both `execution.mode` and the sorted, deduplicated `execution.bypass` list participate in the CLI `session_identity` SHA-256 digest so sessions with different execution policies cannot be shared.

### 3. ExecutionController-Authoritative Adjudication

Instead of propagating the bypass list through child environment variables and matching paths inside the hook, `ExecutableStore` / `ExecutionController` is the single authority for both top-level launches (`Sandbox::run`, `Sandbox::spawn`, `session::run`) and descendant `posix_spawn` / `execve` calls:

1. `ExecutableStore::prepare` resolves the target through `OverlayStore::visible_path` first:
   - Overlay `whiteout` entries still fail with `ENOENT` and never fall through to the host binary.
   - Overlay `cow` entries (`is_internal(&source) == true`) **never** qualify for passthrough, preventing sandbox-written code from escaping the hook by overwriting a bypassed path.
   - `/usr/bin/open` continues to fail with `ENOTSUP`.
   - Shebang scripts continue through normal script/interpreter preparation.
   - Host lower executables matching `execution.bypass` (by canonical resolved path) or the active `ExecutionMode` return `PreparedExecutableTarget::Passthrough(source)` without copying into `<workdir>/fs` or running `lipo`/`codesign`.
2. `execution/protocol.rs` bumps `EXECUTION_PROTOCOL_VERSION` from `6` to `7` and adds `PrepareResponse::Passthrough(PathBuf)` (status `3`).
3. `session/protocol.rs` bumps `PROTOCOL_VERSION` from `1` to `2` and adds `passthrough: bool` to `WirePreparedLaunch`.

### 4. Process Hook & Top-Level Launch Hygiene for Passthrough Processes

When a launch target is `Passthrough`:

- **Environment scrubbing**
  - Descendant hook launches (`ChildEnvironment::passthrough`) remove `DYLD_INSERT_LIBRARIES`, all `CHILD_RUNTIME_ENVIRONMENT` keys (`AGORA_SANDBOX_*`, `AGORA_SANDBOX_PENDING_PROCESS_EVENT`, `AGORA_SANDBOX_TRACE_ID`, and the 6 `TLS_CLIENT_TRUST_ENVIRONMENT` keys), and strip Agora-injected trust-store options from `JAVA_TOOL_OPTIONS` while preserving user-supplied JVM options.
  - Top-level passthrough launches (`ProtectedEnvironment::passthrough`) remove `DYLD_INSERT_LIBRARIES` and all `AGORA_SANDBOX_*` keys and skip `DYLD_INSERT_LIBRARIES` / `JAVA_TOOL_OPTIONS` injection.
- **File descriptor isolation**
  - Authenticated inherited control stream descriptors (`lock`, execution, audit, local Broker, remote Broker) and local encrypted auxiliary (`state`/`lock`) descriptors are **never** inherited by unhooked passthrough children:
    - `posix_spawn` / `posix_spawnp` (`SpawnFileActions::prepare_passthrough`) omits `addinherit_np` and appends `posix_spawn_file_actions_addclose` for every control/auxiliary descriptor not already closed or replaced by caller file actions (failing closed with `ENOTSUP` if caller file actions predate hook initialization and cannot be inspected).
    - `execve` / `execv` / `execvp` (`PassthroughCloexecGuard`) temporarily sets `FD_CLOEXEC` on control/auxiliary descriptors across native `execve` and restores original flags if `execve` fails.
  - In encrypted workspace mode, passthrough launches never call `local.retain()` and fail closed with `ENOTSUP` if any Broker-managed encrypted content descriptor would be inherited by the unhooked child; pipes, TTYs, `/dev/null`, plain files, and `CLOEXEC` encrypted descriptors remain allowed.
  - `execvp` does not fall back to `/bin/sh` with a passthrough environment if native `execve` returns `ENOEXEC`.
- **Audit preservation**
  - Because an unhooked passthrough child never executes the hook initializer (`publish_pending_event`), the caller publishes `process.exec.attempt` fail-closed before calling native `posix_spawn` or `execve` (using caller process identity and child trace context, consistent with failed launches), and does not re-publish if the native call returns an error.

---

## Verification

- **Unit tests**
  - `execution/store/tests.rs`: allowlist policy coverage (`com.apple.keystore.keybag.create`, `com.apple.authorization.smartcard.override`, `com.apple.system-task-ports.read`, `platform-application`, etc.), empty-entitlements signing, preparing and running `/usr/bin/security help` without `SIGKILL`, `ExecutionMode::{Strict, Restricted, Bypass}` behavior, and COW/whiteout precedence.
  - `execution/tests.rs`: `PrepareResponse::Passthrough` protocol round-trip and `ExecutionController` policy integration.
  - `config/tests.rs` & `runner/tests.rs`: `execution.mode` and `execution.bypass` validation (relative path, `/`, directory, `/usr/bin/open`, workdir/SMB overlap) and `session_identity` sensitivity.
  - `session/tests.rs`: `WirePreparedLaunch` passthrough round-trip.
  - `platform/macos/hook/process/tests.rs`: `ChildEnvironment::passthrough` scrubbing (including `JAVA_TOOL_OPTIONS`), `PrepareResponse::Passthrough` handling, and `PassthroughCloexecGuard` flag restoration.
- **macOS integration tests (`tests/runner.rs`)**
  - `execution_mode_restricted_passes_through_security_and_audits_exec`: verifies `/usr/bin/security` runs natively with `ExecutionMode::Restricted` (no hardcoded path, no `<workdir>/fs/usr/bin/security` copy, `/bin/sh` stays copied/hooked, `process.exec.attempt` recorded).
  - `native_executable_passthrough_scrubs_env_and_control_fds_across_spawn_exec_and_top_level`: verifies `DYLD_INSERT_LIBRARIES`, `AGORA_SANDBOX_*`, TLS env vars, managed `JAVA_TOOL_OPTIONS`, and control sockets are absent in passthrough children across `posix_spawn`, `execve`, and top-level `Sandbox::run`.
  - `encrypted_mode_rejects_inheritable_encrypted_descriptor_in_passthrough_child`: verifies encrypted-FD inheritance into passthrough `posix_spawn` and `execve` fails closed with `ENOTSUP`, while pipe redirection succeeds.
