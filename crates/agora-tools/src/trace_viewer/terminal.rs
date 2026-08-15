use anyhow::{Context, Result, anyhow};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const EVENT_CAPACITY: usize = 128;
const STOP_GRACE: Duration = Duration::from_secs(2);
const FORCE_KILL_WAIT: Duration = Duration::from_secs(1);
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TerminalSize {
    pub(super) cols: u16,
    pub(super) rows: u16,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

impl From<TerminalSize> for PtySize {
    fn from(size: TerminalSize) -> Self {
        Self {
            rows: size.rows,
            cols: size.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

#[derive(Debug)]
pub(super) enum TerminalEvent {
    Output(Vec<u8>),
    Exited {
        exit_code: Option<i32>,
        signal: Option<String>,
    },
    Error(String),
}

#[derive(Clone, Debug)]
pub(super) struct TerminalSpec {
    pub(super) sandbox_binary: PathBuf,
    pub(super) config_path: PathBuf,
    pub(super) shell: PathBuf,
}

pub(super) struct TerminalSession {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    killer: Arc<Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
    process_id: libc::pid_t,
    exited: Arc<AtomicBool>,
    stop_requested: Arc<AtomicBool>,
}

impl TerminalSession {
    pub(super) fn spawn(
        spec: TerminalSpec,
        size: TerminalSize,
    ) -> Result<(Self, mpsc::Receiver<TerminalEvent>)> {
        let pair = native_pty_system()
            .openpty(size.into())
            .context("failed to open viewer pseudoterminal")?;
        let mut command = CommandBuilder::new(&spec.sandbox_binary);
        command.args([
            "run".as_ref(),
            "-c".as_ref(),
            spec.config_path.as_os_str(),
            "-e".as_ref(),
            spec.shell.as_os_str(),
        ]);
        command.env("TERM", "xterm-256color");
        let mut child = pair
            .slave
            .spawn_command(command)
            .context("failed to start agora-sandbox in the viewer terminal")?;
        drop(pair.slave);
        let process_id = child
            .process_id()
            .and_then(|pid| libc::pid_t::try_from(pid).ok())
            .context("viewer terminal child did not expose a process id")?;
        let killer = Arc::new(Mutex::new(child.clone_killer()));
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone viewer terminal reader")?;
        let writer = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .context("failed to open viewer terminal writer")?,
        ));
        let master = Arc::new(Mutex::new(pair.master));
        let exited = Arc::new(AtomicBool::new(false));
        let stop_requested = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel(EVENT_CAPACITY);

        let output_sender = sender.clone();
        thread::Builder::new()
            .name("agora-trace-terminal-output".to_string())
            .spawn(move || {
                let mut buffer = [0_u8; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            if output_sender
                                .blocking_send(TerminalEvent::Output(buffer[..read].to_vec()))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                        Err(error) => {
                            let _ = output_sender.blocking_send(TerminalEvent::Error(format!(
                                "terminal output failed: {error}"
                            )));
                            break;
                        }
                    }
                }
            })
            .context("failed to start viewer terminal output worker")?;

        let monitor_exited = exited.clone();
        thread::Builder::new()
            .name("agora-trace-terminal-monitor".to_string())
            .spawn(move || {
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            monitor_exited.store(true, Ordering::Release);
                            let signal = status.signal().map(str::to_string);
                            let exit_code = signal.is_none().then(|| status.exit_code() as i32);
                            let _ =
                                sender.blocking_send(TerminalEvent::Exited { exit_code, signal });
                            break;
                        }
                        Ok(None) => thread::sleep(Duration::from_millis(25)),
                        Err(error) => {
                            monitor_exited.store(true, Ordering::Release);
                            let _ = sender.blocking_send(TerminalEvent::Error(format!(
                                "terminal process monitoring failed: {error}"
                            )));
                            break;
                        }
                    }
                }
            })
            .context("failed to start viewer terminal monitor")?;

        Ok((
            Self {
                master,
                writer,
                killer,
                process_id,
                exited,
                stop_requested,
            },
            receiver,
        ))
    }

    pub(super) fn input(&self, bytes: &[u8]) -> io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("viewer terminal writer lock is poisoned"))?;
        writer.write_all(bytes)?;
        writer.flush()
    }

    pub(super) fn resize(&self, size: TerminalSize) -> io::Result<()> {
        self.master
            .lock()
            .map_err(|_| io::Error::other("viewer terminal master lock is poisoned"))?
            .resize(size.into())
            .map_err(io::Error::other)
    }

    pub(super) fn request_stop(&self) -> io::Result<()> {
        if self.exited.load(Ordering::Acquire) || self.stop_requested.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let result = unsafe { libc::kill(self.process_id, libc::SIGTERM) };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }

        let exited = self.exited.clone();
        let killer = self.killer.clone();
        thread::Builder::new()
            .name("agora-trace-terminal-stop".to_string())
            .spawn(move || {
                thread::sleep(STOP_GRACE);
                if !exited.load(Ordering::Acquire)
                    && let Ok(mut killer) = killer.lock()
                {
                    let _ = killer.kill();
                }
            })
            .map_err(|error| io::Error::other(anyhow!(error)))?;
        Ok(())
    }

    pub(super) fn stop_and_wait(&self) -> io::Result<()> {
        self.request_stop()?;
        if self.wait_for_exit(STOP_GRACE + EXIT_POLL_INTERVAL) {
            return Ok(());
        }
        self.killer
            .lock()
            .map_err(|_| io::Error::other("viewer terminal killer lock is poisoned"))?
            .kill()
            .map_err(io::Error::other)?;
        if self.wait_for_exit(FORCE_KILL_WAIT) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "viewer terminal did not exit after force kill",
            ))
        }
    }

    fn wait_for_exit(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.exited.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(EXIT_POLL_INTERVAL);
        }
        self.exited.load(Ordering::Acquire)
    }

    pub(super) fn is_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.stop_and_wait();
    }
}

#[cfg(test)]
mod tests {
    use super::{TerminalEvent, TerminalSession, TerminalSize, TerminalSpec};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn fake_sandbox(path: &Path) {
        fs::write(
            path,
            r#"#!/bin/sh
printf 'ARGV:%s\r\n' "$*"
printf 'TERM:%s\r\n' "$TERM"
exec /bin/bash --noprofile --norc
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    async fn output_until(receiver: &mut mpsc::Receiver<TerminalEvent>, expected: &str) -> String {
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !String::from_utf8_lossy(&output).contains(expected) {
                match receiver.recv().await.unwrap() {
                    TerminalEvent::Output(bytes) => output.extend(bytes),
                    TerminalEvent::Exited { .. } => panic!("terminal exited before {expected}"),
                    TerminalEvent::Error(message) => panic!("terminal error: {message}"),
                }
            }
        })
        .await
        .unwrap();
        String::from_utf8_lossy(&output).into_owned()
    }

    fn spec(root: &Path) -> TerminalSpec {
        TerminalSpec {
            sandbox_binary: root.join("fake-sandbox"),
            config_path: root.join("sandbox.json"),
            shell: PathBuf::from("/bin/bash"),
        }
    }

    #[tokio::test]
    async fn spawns_only_the_fixed_sandbox_command_and_round_trips_input() {
        let root = tempfile::tempdir().unwrap();
        fake_sandbox(&root.path().join("fake-sandbox"));
        fs::write(root.path().join("sandbox.json"), "{}").unwrap();
        let (session, mut events) = TerminalSession::spawn(
            spec(root.path()),
            TerminalSize {
                cols: 100,
                rows: 30,
            },
        )
        .unwrap();

        let startup = output_until(&mut events, "TERM:xterm-256color").await;
        assert!(startup.contains(&format!(
            "ARGV:run -c {} -e /bin/bash",
            root.path().join("sandbox.json").display()
        )));

        session.input(b"printf 'ROUNDTRIP:%s\\n' ok\r").unwrap();
        let output = output_until(&mut events, "ROUNDTRIP:ok").await;
        assert!(output.contains("ROUNDTRIP:ok"));
        session.request_stop().unwrap();
    }

    #[tokio::test]
    async fn resize_updates_the_child_terminal_dimensions() {
        let root = tempfile::tempdir().unwrap();
        fake_sandbox(&root.path().join("fake-sandbox"));
        fs::write(root.path().join("sandbox.json"), "{}").unwrap();
        let (session, mut events) =
            TerminalSession::spawn(spec(root.path()), TerminalSize::default()).unwrap();
        output_until(&mut events, "TERM:xterm-256color").await;

        session
            .resize(TerminalSize {
                cols: 120,
                rows: 40,
            })
            .unwrap();
        session.input(b"stty size\r").unwrap();

        let output = output_until(&mut events, "40 120").await;
        assert!(output.contains("40 120"));
        session.request_stop().unwrap();
    }

    #[tokio::test]
    async fn ctrl_c_interrupts_the_foreground_terminal_job() {
        let root = tempfile::tempdir().unwrap();
        fake_sandbox(&root.path().join("fake-sandbox"));
        fs::write(root.path().join("sandbox.json"), "{}").unwrap();
        let (session, mut events) =
            TerminalSession::spawn(spec(root.path()), TerminalSize::default()).unwrap();
        output_until(&mut events, "TERM:xterm-256color").await;

        session.input(b"sleep 10\r").unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        session.input(&[3]).unwrap();
        session.input(b"printf 'INTERRUPTED\\n'\r").unwrap();

        let output = output_until(&mut events, "INTERRUPTED").await;
        assert!(output.contains("INTERRUPTED"));
        session.request_stop().unwrap();
    }

    #[tokio::test]
    async fn shutdown_waits_for_a_sigterm_ignoring_child_to_be_force_killed() {
        let root = tempfile::tempdir().unwrap();
        let sandbox = root.path().join("fake-sandbox");
        fs::write(
            &sandbox,
            r#"#!/bin/sh
trap '' TERM
printf 'STUBBORN_READY\r\n'
while :; do sleep 1; done
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&sandbox).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&sandbox, permissions).unwrap();
        fs::write(root.path().join("sandbox.json"), "{}").unwrap();
        let (session, mut events) =
            TerminalSession::spawn(spec(root.path()), TerminalSize::default()).unwrap();
        output_until(&mut events, "STUBBORN_READY").await;

        let started = std::time::Instant::now();
        session.stop_and_wait().unwrap();

        assert!(session.is_exited());
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
