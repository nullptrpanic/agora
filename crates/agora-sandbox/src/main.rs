#[cfg(target_os = "macos")]
use agora_core::lifecycle::{
    shutdown::{ShutdownGuard, ShutdownReason},
    signal::{Signal, SignalHandlers},
};
use agora_core::logger;
#[cfg(not(target_os = "macos"))]
use agora_sandbox::runner::Sandbox;
#[cfg(target_os = "macos")]
use agora_sandbox::session;
use agora_sandbox::{hook_library, runner::SandboxCommand};
use anyhow::{Context, Result};
use clap::{ColorChoice, Parser, Subcommand};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, ExitStatus};
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex, MutexGuard};

mod audit_log;
mod config;
mod key_migration;
#[cfg(feature = "web")]
mod web;

use audit_log::JsonCallback;

#[derive(Parser)]
#[command(
    name = "agora-sandbox",
    about = "Run a command with Agora sandbox network interception and auditing",
    color = ColorChoice::Auto
)]
struct Arguments {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Run an executable inside the configured sandbox
    Run {
        /// Sandbox JSON configuration file
        #[arg(short = 'c', long)]
        config: PathBuf,

        /// Executable command line; shell operators are not interpreted
        #[arg(short = 'e', long)]
        executable: String,
    },

    /// Interactively change the passphrase of an existing encrypted filesystem
    MigrateKey {
        /// Sandbox work directory; defaults to ~/.agora-sandbox
        #[arg(long)]
        workdir: Option<PathBuf>,
    },

    #[cfg(feature = "web")]
    /// Open an interactive sandbox terminal with a live audit timeline
    Web {
        /// Sandbox JSON configuration file
        #[arg(short = 'c', long)]
        config: PathBuf,

        /// Print the URL without opening the default browser
        #[arg(long)]
        no_open: bool,
    },

    #[cfg(target_os = "macos")]
    #[command(name = "__session-daemon", hide = true)]
    SessionDaemon {
        #[arg(long, hide = true)]
        config: PathBuf,
        #[arg(long, hide = true)]
        ready_fd: libc::c_int,
        #[arg(long, hide = true)]
        startup_lock_fd: libc::c_int,
    },
}

fn open_log(path: &Path) -> Result<File> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory {}", parent.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to open log file {}", path.display()))
}

async fn async_main(arguments: Arguments) -> Result<u8> {
    match arguments.command {
        CliCommand::MigrateKey { workdir } => {
            key_migration::run(workdir).await?;
            Ok(0)
        }
        CliCommand::Run { config, executable } => run(config, executable).await,
        #[cfg(feature = "web")]
        CliCommand::Web { config, no_open } => run_web(config, no_open).await,
        #[cfg(target_os = "macos")]
        CliCommand::SessionDaemon {
            config,
            ready_fd,
            startup_lock_fd,
        } => run_session_daemon(config, ready_fd, startup_lock_fd).await,
    }
}

#[cfg(feature = "web")]
async fn run_web(config_path: PathBuf, no_open: bool) -> Result<u8> {
    let (config_path, config) = load_config(config_path)?;
    web::run(web::WebOptions {
        config_path,
        log_path: config.log_file().to_path_buf(),
        open_browser: !no_open,
    })
    .await?;
    Ok(0)
}

async fn run(config_path: PathBuf, executable: String) -> Result<u8> {
    let (config_path, config) = load_config(config_path)?;
    let command = parse_command(&executable)?;
    let hook = hook_library::materialize(config.workdir())?;

    #[cfg(not(target_os = "macos"))]
    {
        let _ = config.session_identity(&hook)?;
        logger::init(open_log(config.log_file())?, logger::LevelFilter::Info)?;
        let outcome = Sandbox::new(config.into_runtime(hook), JsonCallback::new())
            .run(command)
            .await?;
        return Ok(exit_status_code(outcome.status()));
    }

    #[cfg(target_os = "macos")]
    {
        let identity = config.session_identity(&hook)?;
        let workdir = config.workdir().to_path_buf();

        let status = Arc::new(Mutex::new(None::<ExitStatus>));
        let reason = Arc::new(Mutex::new(None::<ShutdownReason>));
        let process_status = Arc::clone(&status);
        let shutdown_reason = Arc::clone(&reason);
        let guard = ShutdownGuard::get();
        let signals = shutdown_signals(&guard)?;
        let process = async move {
            let outcome = session::run(&config_path, &workdir, &identity, command).await?;
            *lock(&process_status) = Some(outcome.status());
            Ok(())
        };

        guard
            .run_with_shutdown(process, signals, move |reason| async move {
                *lock(&shutdown_reason) = Some(reason);
            })
            .await?;

        if let Some(status) = lock(&status).take() {
            return Ok(exit_status_code(status));
        }
        let signal = match lock(&reason).as_ref() {
            Some(ShutdownReason::Signal { signal }) => Some(*signal),
            _ => None,
        };
        Ok(signal.map(signal_exit_code).unwrap_or(1))
    }
}

#[cfg(target_os = "macos")]
async fn run_session_daemon(
    config_path: PathBuf,
    ready_fd: libc::c_int,
    startup_lock_fd: libc::c_int,
) -> Result<u8> {
    let startup =
        unsafe { session::DaemonStartup::from_raw_descriptors(ready_fd, startup_lock_fd) }?;
    let prepared = (|| {
        let (_, config) = load_config(config_path)?;
        let hook = hook_library::materialize(config.workdir())?;
        let identity = config.session_identity(&hook)?;
        logger::init(open_log(config.log_file())?, logger::LevelFilter::Info)?;
        Ok::<_, anyhow::Error>((config.into_runtime(hook), identity))
    })();
    let (config, identity) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = startup.failed(&error).await;
            return Err(error);
        }
    };
    session::serve(config, JsonCallback::new(), identity, startup).await?;
    Ok(0)
}

fn absolute_config_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(std::env::current_dir()
        .context("failed to resolve current directory")?
        .join(path))
}

fn load_config(path: PathBuf) -> Result<(PathBuf, config::RunConfig)> {
    let path = absolute_config_path(path)?;
    let config = config::RunConfig::load(&path)?;
    Ok((path, config))
}

fn parse_command(command: &str) -> Result<SandboxCommand> {
    let mut words = shell_words::split(command).context("failed to parse command line")?;
    if words.is_empty() {
        anyhow::bail!("command line must contain a program");
    }
    let program = words.remove(0);
    Ok(SandboxCommand::new(program).args(words))
}

#[cfg(target_os = "macos")]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn exit_status_code(status: ExitStatus) -> u8 {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1)
}

#[cfg(target_os = "macos")]
fn signal_exit_code(signal: i32) -> u8 {
    u8::try_from(128_i32.saturating_add(signal)).unwrap_or(u8::MAX)
}

#[cfg(target_os = "macos")]
fn shutdown_signals(guard: &Arc<ShutdownGuard>) -> Result<SignalHandlers<Arc<ShutdownGuard>>> {
    use tokio::signal::unix::SignalKind;

    let mut signals = SignalHandlers::new();
    signals.register(
        Signal::new(SignalKind::interrupt().as_raw_value()),
        Arc::clone(guard),
    )?;
    signals.register(
        Signal::new(SignalKind::terminate().as_raw_value()),
        Arc::clone(guard),
    )?;
    Ok(signals)
}

fn main() -> ExitCode {
    let arguments = Arguments::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to initialize Tokio runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(async_main(arguments)) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests;
