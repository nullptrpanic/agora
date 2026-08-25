use anyhow::{Context, Result};
use std::env;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::oneshot;

mod assets;
mod audit;
mod protocol;
mod server;
mod terminal;

#[cfg(test)]
mod tests;

pub(super) struct WebOptions {
    pub(super) config_path: PathBuf,
    pub(super) log_path: PathBuf,
    pub(super) open_browser: bool,
}

pub(super) async fn run(options: WebOptions) -> Result<()> {
    let sandbox_binary =
        env::current_exe().context("failed to resolve the agora-sandbox binary")?;
    run_with(
        options,
        sandbox_binary,
        |url| async move { open_default_browser(&url).await },
        async {
            if let Err(error) = tokio::signal::ctrl_c().await {
                eprintln!("warning: failed to wait for Ctrl-C: {error}");
            }
        },
    )
    .await
}

async fn run_with<Open, OpenFuture, Shutdown>(
    options: WebOptions,
    sandbox_binary: PathBuf,
    opener: Open,
    shutdown: Shutdown,
) -> Result<()>
where
    Open: FnOnce(String) -> OpenFuture,
    OpenFuture: Future<Output = io::Result<()>>,
    Shutdown: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("failed to bind the trace viewer loopback listener")?;
    let address = listener
        .local_addr()
        .context("failed to inspect the trace viewer listener")?;
    let origin = format!("http://{address}");
    let token = protocol::AccessToken::generate();
    let launch_url = format!("{origin}/#token={}", token.as_str());
    let hub = server::EventHub::new();
    let manager = Arc::new(server::SessionManager::new(
        terminal::TerminalSpec {
            sandbox_binary,
            config_path: options.config_path,
        },
        options.log_path,
        hub.clone(),
    ));
    let state = server::AppState::new(
        server::AccessGuard {
            expected_host: address.to_string(),
            expected_origin: origin,
        },
        token,
        manager.clone(),
        hub,
    );
    let server_state = state.clone();
    let (server_shutdown_tx, server_shutdown_rx) = oneshot::channel();
    let mut server_task = tokio::spawn(server::serve(listener, server_state, async move {
        let _ = server_shutdown_rx.await;
    }));

    println!("Agora Runtime Trace: {launch_url}");
    println!("Press Ctrl-C to stop the viewer.");
    if options.open_browser
        && let Err(error) = opener(launch_url).await
    {
        eprintln!("warning: could not open the default browser: {error}");
    }

    tokio::pin!(shutdown);
    let early_server_result = tokio::select! {
        _ = &mut shutdown => None,
        result = &mut server_task => Some(result),
    };

    state.shutdown_clients();
    let terminal_shutdown = manager.shutdown();
    let _ = server_shutdown_tx.send(());
    match early_server_result {
        Some(result) => result.context("trace viewer server task failed")??,
        None => server_task
            .await
            .context("trace viewer server task failed")??,
    }
    terminal_shutdown.context("failed to stop the trace viewer terminal")?;
    Ok(())
}

async fn open_default_browser(url: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("/usr/bin/open");
        command.arg(url);
        command
    };
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("rundll32.exe");
        command.args(["url.dll,FileProtocolHandler", url]);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "automatic browser opening is unsupported on this platform",
    ));

    let status = command.status().await?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "browser opener exited with {status}"
        )))
    }
}
