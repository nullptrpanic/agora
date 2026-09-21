# Process Lifecycle

`agora-core` provides shared process signal dispatch and shutdown notification. Binaries such as `agora-node` choose the concrete operating-system signals and use `ShutdownGuard` to supervise their main process future.

## Responsibilities

The lifecycle layer should:

- Allow callers to register arbitrary operating-system signal numbers and handlers.
- Allow any Agora module to register a process-global shutdown callback.
- Normalize normal completion, signals, intentional requests, and failures into a structured reason.
- Allow a caller-owned asynchronous shutdown step to finish before stopping the supervised process future and releasing the shutdown guard.
- Run every registered shutdown callback when the last `ShutdownGuard` reference is dropped.

It should not send channel messages, persist state, or decide module-specific cleanup behavior.

## Module Boundaries

The lifecycle implementation is split under `agora-core/src/lifecycle/`:

- `signal`: defines `Signal`, `SignalHandler`, and generic `SignalHandlers<H>`. Registration is instance-based, accepts raw signal numbers, and does not hard-code SIGINT, SIGTERM, or shutdown behavior.
- `shutdown`: defines the process-global `on_shutdown` registry, `ShutdownReason`, and the `ShutdownGuard` singleton. It does not import or listen through the signal module.
- `mod`: composes both independent modules. It implements `SignalHandler` for `Arc<ShutdownGuard>` and provides `ShutdownGuard::run`.

The two child modules do not depend on each other. Only their parent composition layer maps a `Signal` into `ShutdownReason::Signal`.

## Public Boundary

Shutdown callbacks are synchronous because Rust cannot await asynchronous work from `Drop`:

```rust
agora_core::lifecycle::shutdown::on_shutdown(|reason| {
    // Module-owned bounded cleanup or notification.
    Ok(())
})?;
```

Code that must perform bounded asynchronous work before its process future is dropped uses the supervised shutdown hook instead:

```rust
guard
    .run_with_shutdown(process, signals, move |reason| async move {
        component.shutdown(reason).await;
    })
    .await?;
```

The hook runs after the first shutdown reason is selected but while the supervised process future and its spawned work are still alive. The caller owns timeout handling and module-specific behavior. The existing `on_shutdown` callbacks still run later, when the last guard reference is dropped.

Intentional shutdown is requested separately:

```rust
agora_core::lifecycle::shutdown::request_shutdown("operator requested shutdown");
```

Signal handlers are registered on an instance. The caller chooses each concrete signal:

```rust
use agora_core::lifecycle::signal::{Signal, SignalHandler, SignalHandlers};

struct Handler;

impl SignalHandler for Handler {
    fn handle(&self, signal: Signal) {
        // Signal-specific handling.
    }
}

let mut signals = SignalHandlers::new();
signals.register(Signal::new(raw_signal_number), Handler)?;
```

`ShutdownGuard::get` returns an `Arc<ShutdownGuard>`. Calls return the same allocation while that singleton is alive. The global slot retains only a `Weak` reference so the last strong reference can run `Drop`.

`Arc<ShutdownGuard>` implements `SignalHandler` in the parent lifecycle module. A binary can therefore subscribe the singleton to selected signals and supervise its process:

```rust
let guard = ShutdownGuard::get();
signals.register(signal, Arc::clone(&guard))?;
guard.run(process, signals).await?;
```

## Shutdown Reasons

`ShutdownReason` distinguishes:

- `Signal { signal }`: the raw operating-system signal number.
- `Requested`: an intentional in-process request with a caller-provided reason.
- `Normal`: the supervised process future returned successfully, or the guard was dropped without another reason.
- `Failed`: the supervised process future or signal listener returned an error.

The first accepted shutdown reason wins. Later requests do not replace it.

## Callback Semantics

- Callbacks must be registered before shutdown begins.
- Each callback receives the same cloned `ShutdownReason`.
- Callbacks run once in registration order when the last guard reference is dropped.
- A callback error or panic is logged and does not prevent later callbacks from running.
- Callback logic must be synchronous and bounded.
- Heterogeneous shutdown closures require type erasure; this callback registry is the lifecycle implementation's only use of `Box`.

The optional asynchronous hook passed to `run_with_shutdown` is separate from the global callback registry. It runs once before the supervised process future is dropped and can await bounded network or process cleanup without making `ShutdownGuard::Drop` asynchronous.

Rust does not run `Drop` after `SIGKILL`, `std::process::abort`, or `std::process::exit`. Callers must release the guard before explicitly terminating the process.
