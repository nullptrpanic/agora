# Agora Sandbox

[English](README.md) | [简体中文](README.zh-CN.md) | [Workspace](../../README.md)

The hard local-agent problem is not hiding the whole machine; it is letting useful tools read the context they need without letting those tools rewrite host state. Agora Sandbox solves that trade-off for one macOS process tree with a private Overlay, transparent encrypted storage, remote files, and runtime evidence—all without privileged deployment.

> Agora Sandbox is a cooperative user-space boundary, not a kernel namespace or VM. Read [Security Model and Limitations](#security-model-and-limitations) before using it with hostile code.

## Highlights

- **Read the host; write only to the sandbox.** On managed paths, programs keep the familiar host lower view, while writes, renames, and deletions enter a private upper layer. The original host entries remain unchanged.
- **The private view still behaves like a real filesystem.** Ordinary and positioned I/O, independent descriptor offsets, file locks, append, and writable `MAP_SHARED` mappings retain native semantics instead of forcing applications onto a special storage API.
- **Encrypted data has no ordinary plaintext backing path.** In encrypted mode, business filenames and contents are authenticated ciphertext at rest; shared runtime plaintext lives in anonymous, unlinked vnodes, with large files materialized by range rather than decrypted at open.
- **Audit answers three direct questions: what ran, what opened, and where it connected.** `execve` and related launches, logical file opens/closes, and covered TCP connections carry process and trace context, plus HTTP Host or TLS SNI when available.
- **TLS visibility does not require a system-wide CA installation.** Auto mode terminates covered TLS in a parent-side proxy, verifies upstream certificates, and scopes child trust without changing the host Keychain. Audit remains metadata-only.
- **Remote workspaces need no host mount.** SMB2/3 roots appear at normal logical paths while credentials and protocol sessions remain in the parent; the child receives anonymous descriptors instead of mount authority or raw credentials.
- **Rootless changes the deployment model.** There is no root daemon, FUSE mount, kernel extension, reboot, or SIP change, so the boundary can be created for one local workflow instead of installed as machine infrastructure.
- **Runtime behavior is visible while it happens.** Runtime Trace places a real `/bin/bash` terminal beside a live, searchable process/file/network timeline backed by the JSON Lines audit log.

## Usage

### 1. Prerequisites

- macOS on Apple Silicon (`aarch64`) or Intel (`x86_64`).
- A stable Rust toolchain.
- Xcode Command Line Tools, including `clang`, `lipo`, and `codesign`.

### 2. Build

From the workspace root:

```bash
cargo build --release -p agora-sandbox
```

The build produces `target/release/agora-sandbox` and embeds the matching signed hook library in that binary. The default build includes SMB remote filesystem and Runtime Trace support.

### 3. Create a configuration

Create `sandbox.json`:

```json
{
  "workdir": "~/.agora-sandbox",
  "tls": "auto",
  "filesystem": {
    "bypass": [],
    "local": {
      "encrypt": "encrypted",
      "key": "replace-with-a-private-passphrase"
    },
    "nfs": []
  },
  "log": {
    "file": "runtime/logs/sandbox.log"
  }
}
```

The encryption key and any remote credentials are secrets stored in this JSON file. Do not commit it; restrict its permissions, for example:

```bash
chmod 600 sandbox.json
```

`workdir` defaults to `~/.agora-sandbox`. Relative configuration paths resolve from the configuration file or the work directory according to their field; the default log resolves to `<workdir>/runtime/logs/sandbox.log`.

Set `filesystem.local.encrypt` to `plain` and omit `key` if encryption is not required. Set `tls` to `off` to disable TLS termination.

### 4. Run a command

```bash
target/release/agora-sandbox run \
  --config sandbox.json \
  --executable "/bin/bash -lc 'printf sandboxed > /tmp/agora-demo.txt && cat /tmp/agora-demo.txt'"
```

The `--executable` value is split into an executable and arguments, but shell operators are not interpreted by the CLI itself. Use an explicit shell such as `/bin/bash -lc '...'` when pipes, redirects, variables, or compound commands are needed.

The command inherits the caller's standard input, output, error, environment, and working directory. Its exit status becomes the CLI exit status.

### 5. Open Runtime Trace

```bash
target/release/agora-sandbox web --config sandbox.json
```

This starts a fixed interactive `/bin/bash` on a pseudoterminal and opens the local Runtime Trace page. Use `--no-open` to print the loopback URL without opening a browser:

```bash
target/release/agora-sandbox web --config sandbox.json --no-open
```

The page can send terminal input, resize, stop, and restart that fixed shell. It cannot choose another executable, configuration, log path, or host command. The UI and its JavaScript dependencies are embedded in the Rust binary; no Node.js runtime or CDN is required.

### 6. Inspect the audit log

```bash
tail -f ~/.agora-sandbox/runtime/logs/sandbox.log
```

The log is JSON Lines and is separate from child stdout/stderr. Each compact record describes one intercepted process execution attempt, file open/close, or network connection attempt and includes the trace identity needed to relate descendant activity.

### 7. Change an encryption key

Stop every command using that work directory, then run the interactive migration:

```bash
target/release/agora-sandbox key migrate --workdir ~/.agora-sandbox
```

Migration takes the same exclusive workspace lock as normal execution and refuses to run while the workspace is active.

### 8. Add an SMB remote root

The configuration field is named `nfs` for Agora's protocol-neutral network-filesystem layer; the currently implemented backend is SMB2/3:

```json
{
  "filesystem": {
    "local": {
      "encrypt": "encrypted",
      "key": "replace-with-a-private-passphrase"
    },
    "nfs": [
      {
        "type": "smb",
        "dir": "/workspace",
        "server": "smb://files.example.com/share/project",
        "username": "user",
        "password": "replace-me"
      }
    ]
  }
}
```

`dir` is the absolute logical path visible inside the sandbox. A remote root has higher lookup priority than the local upper and host lower layers. It does not create a macOS mount: reads are fetched lazily into anonymous local snapshots, while changed snapshots are checked against the opened remote baseline and published through staged whole-file replacement. It therefore provides remote filesystem semantics, not synchronous per-write SMB traffic.

## Filesystem Model

### What “Overlay” means

An Overlay filesystem combines layers into one logical view:

```text
                    sandbox path: /project/report.txt
                                  |
                       lookup upper first
                                  |
                +-----------------+-----------------+
                |                                   |
        private upper layer                 host lower layer
      sandbox changes / whiteouts           original host data
                |                                   |
                +------------- merged view --------+
```

- **Read:** use a remote entry or private upper entry when present; otherwise fall through to the host lower entry.
- **First mutation:** create or copy the logical object into the upper layer and mutate that private version.
- **Delete:** record a whiteout in the upper layer so the lower object disappears only from the sandbox view.
- **Rename and metadata changes:** update the logical upper namespace without changing the lower host object.
- **Bypass:** `/dev` and explicitly configured absolute `filesystem.bypass` roots use native host behavior and are intentionally outside Overlay, encryption, and file audit.

The practical result is asymmetric access: a sandbox can work with familiar host paths, but covered mutations stay inside its work directory. In encrypted mode, business filenames and contents in that upper layer are ciphertext; plaintext lives in anonymous, unlinked vnodes rather than ordinary host-visible files. Another process running as the same macOS user can still delete or tamper with the physical work directory, so this is data-at-rest protection and view isolation—not protection from a hostile peer with the same OS identity.

### Encrypted I/O and large files

Encryption is block-based and authenticated. For an encrypted file larger than 1 MiB, a non-truncating cold open validates its header and creates a correctly sized sparse anonymous plaintext vnode without decrypting the whole body. Reads materialize only requested ranges with bounded readahead. Writes report completed byte ranges and re-encrypt only affected 4 KiB ciphertext blocks, using read-modify-write for partial boundary blocks.

Independent opens of the same ciphertext inode share one plaintext vnode, so completed writes and shared mappings become visible without repeatedly decrypting peer snapshots. Each open still keeps independent offset and status-flag state, and file locking is anchored across opens.

`mmap` uses the native macOS pager over that anonymous plaintext vnode:

- the requested mapping range is materialized before the native `mmap` call;
- page faults do not call the encryption Broker, so there is no custom fault-time pager;
- writable `MAP_SHARED` ranges are tracked and encrypted on synchronization boundaries such as `msync`, `munmap`, final close, exec, and normal runtime shutdown, with periodic change detection while the runtime remains alive;
- `MAP_PRIVATE` changes are never written back.

This avoids whole-file decryption on ordinary large-file opens while preserving native descriptor and mapping behavior. Mapping a very large range can still require materializing that requested range up front.

### Shared workspace sessions

Overlapping CLI commands with the same canonical work directory, build, and effective configuration join one ephemeral per-workspace session. They share Overlay state, encrypted plaintext vnodes, locks, network controllers, audit, and remote filesystem state, while each caller keeps its own terminal and process group. The helper exits after the final command releases its lease; no persistent machine-wide daemon is installed.

## Runtime Audit

All audit records carry a sandbox ID, run ID or root trace identity, and process context so descendant activity can be correlated instead of shown as unrelated log lines.

| Activity | What is captured |
| --- | --- |
| Process execution | `execve`, `execv`, `execvp`, `posix_spawn`, or `posix_spawnp`; executable, full argument vector, current directory, PID, PPID, and trace |
| Filesystem | Logical pre-Overlay path, open or close operation, read/write access, and create/truncate/append/exclusive flags |
| Network | TCP destination IP and port, PID and trace, plus target host, HTTP Host, TLS SNI, or normalized domain when observable |

The compact CLI log intentionally does not capture file contents, terminal input/output, HTTP bodies, or full URLs. The public Rust callback receives the richer versioned event model; on network connection attempts it may return `Allow`, `Deny`, or an HTTP `Proxy` decision.

## Network and TLS Model

The injected process tree redirects covered `connect` and simple `connectx` calls to authenticated IPv4/IPv6 loopback proxies. The proxy inspects a bounded initial prefix to derive HTTP Host or TLS SNI, asks the callback for a decision, and either opens the original destination or uses the selected HTTP CONNECT proxy.

With `tls: "auto"`, the parent verifies upstream TLS using native roots, issues short-lived child-facing certificates from a workdir-local or explicitly configured CA, and relays decrypted application bytes. Trust is scoped to injected `SecTrust` clients and common environment-aware tools; Agora does not install the CA into the login or system Keychain, and the private key is never passed to the child.

## Rust Library Usage

The library exposes typed configuration, process-style command setup, asynchronous callbacks, `spawn`, and the foreground `run` convenience method:

```rust,no_run
use agora_sandbox::{
    callback::{Decision, Event},
    hook_library,
    runner::{Sandbox, SandboxCommand, SandboxConfig},
};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let workdir = PathBuf::from("/absolute/path/to/.agora-sandbox");
    let hook = hook_library::materialize(&workdir)?;
    let key = std::env::var("AGORA_SANDBOX_KEY")?;
    let config = SandboxConfig::new(hook)
        .with_workdir(&workdir)
        .with_encrypted_workspace(key.as_bytes());

    let sandbox = Sandbox::new(config, |event: Event| async move {
        eprintln!("{event:?}");
        Decision::Allow
    });
    let outcome = sandbox
        .run(SandboxCommand::new("/usr/bin/env").arg("true"))
        .await?;

    println!("sandbox={} run={} status={}", outcome.sandbox_id(), outcome.run_id(), outcome.status());
    Ok(())
}
```

`SandboxCommand` also supports `args`, `env`, `current_dir`, and standard `Stdio` configuration. `spawn` returns a live `SandboxChild`; the caller owns pipe draining, execution timeouts, and output limits.

## Security Model and Limitations

- **Cooperative boundary.** Coverage applies to processes that successfully load the Agora hook. This is not a kernel-enforced namespace, container, or VM boundary.
- **macOS only.** The runtime hook and executable preparation support native Apple Silicon and Intel macOS targets.
- **No strict egress enforcement yet.** Covered TCP interception errors stop that connection, but a process may have an unsupported path or networking stack. Do not treat intercept mode as complete network confinement.
- **Partial API surface.** Direct syscalls and some filesystem families remain outside the hook surface; several unsupported mutations return `ENOTSUP`. Native passthrough roots deliberately bypass Overlay and audit.
- **Host identity remains real.** The sandbox does not virtualize users, groups, ACLs, entitlements, TCC, or Keychain. Descendants can use the current user's host Keychain.
- **System-mediated launches are outside the tree.** Direct application binaries can be prepared, but `/usr/bin/open` is unsupported because LaunchServices would launch outside the injected process-tree lifecycle.
- **TLS client compatibility varies.** Clients that ignore both macOS `SecTrust` and configured CA environment variables require their own trust configuration.
- **Normal-shutdown durability.** Normal synchronization and shutdown flush changed encrypted data. A controller or host crash can lose updates that were not durably synchronized, and Overlay namespace mutations do not provide power-loss rollback or journal recovery.
- **Remote writeback is snapshot-based.** Modified SMB files require a complete logical baseline and whole-file publication; concurrent remote modification is rejected rather than merged.

Execution preparation and sandbox initialization errors stop the command instead of silently running it without the configured boundary.

## Development

Run focused checks from the workspace root:

```bash
cargo test -p agora-sandbox --all-targets --jobs 16
cargo clippy -p agora-sandbox --all-targets --all-features --jobs 16 -- -D warnings
```
