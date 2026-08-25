# Agora

[English](README.md) | [简体中文](README.zh-CN.md)

Agora provides the two building blocks that useful local agents need: a reliable conversational entry point, and a rootless execution boundary that can expose real host context without surrendering host state. It currently ships them as two independently usable components:

- **Agora Node** turns Lark or Telegram into an observable control surface for local Codex and custom agents—not a blind remote shell.
- **Agora Sandbox** lets commands work with familiar host paths while covered mutations remain private, encrypted, and traceable.

> Agora is under active development (`0.1.0`). Node and Sandbox are usable independently today; automatic Node-to-Sandbox execution is not wired yet.

## What Makes Agora Different

Agora is designed around the gap between a demo that can run commands and an agent runtime that people can actually trust and operate:

- **Chat becomes the operating surface, not a notification pipe.** Users submit work, inspect commands and progress, stop or reset runs, and receive the final answer without leaving Lark or Telegram.
- **Agents can use host context without rewriting host data.** The sandbox presents familiar lower-layer paths, while covered writes, renames, and deletions stay in a private Overlay.
- **Sandbox-created plaintext does not need a host path.** In encrypted mode, business filenames and contents are stored as ciphertext; runtime plaintext lives in anonymous files instead of ordinary host-visible backing files.
- **Every important action leaves evidence.** Process execution—including `execve`—file opens and closes, and network connections share trace context, answering what ran, what it opened, and where it connected.
- **Deployment stays local and unprivileged.** No mount, root daemon, kernel extension, reboot, or macOS system-protection change is required.

## Workspace

| Crate | Role | Status |
| --- | --- | --- |
| [`agora-node`](crates/agora-node/README.md) | Local daemon, chat channels, agent execution, scheduling, and session state | Lark, Telegram, Codex, and custom agents are active |
| [`agora-sandbox`](crates/agora-sandbox/README.md) | Rootless command sandbox, filesystem overlay and encryption, remote files, TLS interception, and runtime audit | Implemented for macOS |
| `agora-core` | Shared logging, lifecycle, and stable domain utilities | Internal shared library |
| `agora-server` | Future server-side control plane | Skeleton; no protocol implementation yet |

## Architecture at a Glance

```text
Lark / Telegram
       |
       v
  Agora Node  ------>  Codex / custom agent
       |                       |
       `---- session state ----'

Command / SDK
       |
       v
 Agora Sandbox  ---> private filesystem view
       |          |-> process, file, and network audit
       |          `-> optional SMB remote roots
       `------------> rootless network and TLS interception
```

Node and Sandbox deliberately have separate ownership boundaries. A channel does not know how an agent executes, an agent does not depend on a channel, and the sandbox does not depend on either. This keeps each component independently testable and leaves room for a future explicit integration boundary.

## Quick Start

Build the complete workspace with the stable Rust toolchain:

```bash
cargo build --workspace --release
```

Then choose the component you want to run:

- [Run Agora Node](crates/agora-node/README.md#usage)
- [Run Agora Sandbox](crates/agora-sandbox/README.md#usage)

Agora Sandbox's runtime is macOS-specific and its build uses the Xcode Command Line Tools (`clang`, `lipo`, and `codesign`). See the component README for configuration and platform limitations.

## Development

Common workspace checks are:

```bash
cargo fmt --all -- --check
cargo test --workspace --all-targets --jobs 16
cargo clippy --workspace --all-targets --all-features --jobs 16 -- -D warnings
```
