# Engineering Conventions

This project follows a small, direct Rust style.

## Crate Shape

- `agora-core` is the shared library crate.
- `agora-node` has a thin library target plus a binary target. The library exists so agent, channel, config, and daemon boundaries can be tested from `tests/` without moving unstable APIs into `agora-core`.
- `agora-server` is a binary crate.
- `agora-sandbox` has a typed SDK, injectable `cdylib`, and thin binary target. Its hook, execution
  preparation, and wire protocols are private source modules in the same crate.
- Do not add a library target to other non-core crates unless there is a concrete reuse need.

## Async Traits

Do not use `async_trait` for project-owned traits.

Preferred style:

```rust
pub trait Server {
    fn serve(&self) -> impl Future<Output = anyhow::Result<()>>;
}
```

Use enum aggregation for multiple implementations when possible.

## Adapter Boundaries

Keep adapters small:

- Channel adapters translate external task and event transports.
- Agent implementations own backend-specific execution, output decoding, and session state.
- The shared command helper only manages child-process IO, exit status, and command process-group lifecycle; agents may reuse it or implement another execution mechanism.
- Daemon code coordinates channel tasks and agent runs.
- Lifecycle code in `agora-core` keeps generic instance-based signal handling separate from process-global shutdown callbacks and composes them from the parent module.
- Sandbox SDK code owns local isolation lifecycle and audit contracts. The hook, executable
  preparation controller, and wire protocols remain private implementation details.

Do not let a channel spawn commands directly. Do not let an agent implementation know the external channel type. Do not put backend protocol or session behavior in the shared command helper.

## Errors

Use `anyhow::Result` at binary orchestration boundaries.

Use narrower standard errors such as `io::Result` where the module is a thin wrapper around IO behavior and callers can make useful decisions from `io::ErrorKind`.

## Logging

Use `agora_core::logger`.

The public logging path is:

```rust
agora_core::logger::init(std::io::stdout(), agora_core::logger::LevelFilter::Info)?;
agora_core::logger::info!("message");
agora_core::logger::debug!("message");
```

The logger outputs JSON lines. Repeated `init` calls are treated as successful no-ops after the first logger has been installed.

## Tests

Use focused tests for public behavior.

For `agora-node`, put behavior tests under `crates/agora-node/tests/` and import the thin library API. Keep test-only fakes in test files where practical.

For pure binary crates, prefer tests that run the binary and assert externally observable behavior. Do not add library targets to those crates only to make tests easier.

Keep black-box integration tests that use only public APIs under each crate's top-level `tests/`
directory. Keep white-box unit tests adjacent to the module they test, using
`#[cfg(test)] mod tests;` with `src/<module>/tests.rs` or `src/<module>/tests/`; do not attach
white-box tests from top-level `tests/` with `#[path]`.

For `agora-sandbox`, keep public SDK behavior tests under `crates/agora-sandbox/tests/`. Keep hook
and execution unit tests under adjacent source-module test paths, and include real injected-child
tests for both the macOS network and recursive process interposition paths.

For shared library behavior, test through public APIs unless private implementation details are specifically being stabilized.

## Documentation Consistency

When code changes affect architecture, module responsibility, public APIs, runtime assumptions, protocol behavior, or security boundaries, update `spec/` in the same change.

If code and spec disagree, either update code to match the spec or update the spec to match the intended behavior.
