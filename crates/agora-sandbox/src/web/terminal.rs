use anyhow::{Context, Result, anyhow};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const EVENT_CAPACITY: usize = 128;
const INPUT_CAPACITY: usize = 16;
pub(super) const MAX_INPUT_BYTES: usize = 64 * 1024;
const STOP_GRACE: Duration = Duration::from_secs(2);
const FORCE_KILL_WAIT: Duration = Duration::from_secs(1);
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const SANDBOX_SHELL: &str = "/bin/bash";

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
}

pub(super) struct TerminalSession {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    input: std_mpsc::SyncSender<Vec<u8>>,
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
        let descriptor = pair
            .master
            .as_raw_fd()
            .context("terminal has no native descriptor")?;
        let mut reader =
            File::from(unsafe { BorrowedFd::borrow_raw(descriptor) }.try_clone_to_owned()?);
        let writer = reader.try_clone()?;
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let mut command = CommandBuilder::new(&spec.sandbox_binary);
        command.args([
            "run".as_ref(),
            "-c".as_ref(),
            spec.config_path.as_os_str(),
            "-e".as_ref(),
            SANDBOX_SHELL.as_ref(),
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
        let master = Arc::new(Mutex::new(pair.master));
        let exited = Arc::new(AtomicBool::new(false));
        let stop_requested = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel(EVENT_CAPACITY);
        let (input, input_receiver) = std_mpsc::sync_channel(INPUT_CAPACITY);
        let input_sender = sender.clone();
        let input_exited = exited.clone();
        let input_stopped = stop_requested.clone();
        thread::Builder::new()
            .name("agora-trace-terminal-input".to_string())
            .spawn(move || {
                if let Err(error) =
                    write_input(writer, input_receiver, &input_exited, &input_stopped)
                {
                    let _ = input_sender.blocking_send(TerminalEvent::Error(format!(
                        "terminal input failed: {error}"
                    )));
                }
            })
            .context("failed to start viewer terminal input worker")?;

        let output_sender = sender.clone();
        let output_exited = exited.clone();
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
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            if output_exited.load(Ordering::Acquire) {
                                break;
                            }
                            if let Err(error) = wait_ready(&reader, libc::POLLIN) {
                                let _ = output_sender
                                    .blocking_send(TerminalEvent::Error(error.to_string()));
                                break;
                            }
                        }
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
                input,
                process_id,
                exited,
                stop_requested,
            },
            receiver,
        ))
    }

    pub(super) fn input(&self, bytes: &[u8]) -> io::Result<()> {
        if self.is_exited() || self.stop_requested.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "terminal is stopping",
            ));
        }
        if bytes.len() > MAX_INPUT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal input exceeds frame limit",
            ));
        }
        if bytes.is_empty() {
            return Ok(());
        }
        self.input
            .try_send(bytes.to_vec())
            .map_err(|error| match error {
                std_mpsc::TrySendError::Full(_) => {
                    io::Error::new(io::ErrorKind::WouldBlock, "terminal input queue is full")
                }
                std_mpsc::TrySendError::Disconnected(_) => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "terminal input worker stopped")
                }
            })
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
        signal_process(self.process_id, libc::SIGTERM)?;

        let exited = self.exited.clone();
        let process_id = self.process_id;
        thread::Builder::new()
            .name("agora-trace-terminal-stop".to_string())
            .spawn(move || {
                thread::sleep(STOP_GRACE);
                if !exited.load(Ordering::Acquire) {
                    let _ = signal_process(process_id, libc::SIGKILL);
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
        signal_process(self.process_id, libc::SIGKILL)?;
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

fn wait_ready(file: &File, events: libc::c_short) -> io::Result<()> {
    let mut descriptor = libc::pollfd {
        fd: file.as_raw_fd(),
        events,
        revents: 0,
    };
    if unsafe { libc::poll(&mut descriptor, 1, EXIT_POLL_INTERVAL.as_millis() as i32) } < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(())
}

fn write_input(
    mut writer: File,
    receiver: std_mpsc::Receiver<Vec<u8>>,
    exited: &AtomicBool,
    stopped: &AtomicBool,
) -> io::Result<()> {
    while !exited.load(Ordering::Acquire) && !stopped.load(Ordering::Acquire) {
        let bytes = match receiver.recv_timeout(EXIT_POLL_INTERVAL) {
            Ok(bytes) => bytes,
            Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let mut remaining = bytes.as_slice();
        while !remaining.is_empty() {
            if exited.load(Ordering::Acquire) || stopped.load(Ordering::Acquire) {
                return Ok(());
            }
            match writer.write(remaining) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(written) => remaining = &remaining[written..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    wait_ready(&writer, libc::POLLOUT)?
                }
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

fn signal_process(process_id: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    if unsafe { libc::kill(process_id, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
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
    use std::path::Path;
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
        }
    }

    #[tokio::test]
    async fn input_backpressure_does_not_block_terminal_control() {
        let root = tempfile::tempdir().unwrap();
        let sandbox = root.path().join("fake-sandbox");
        fs::write(
            &sandbox,
            "#!/bin/sh\nstty raw -echo\nprintf READY\nexec sleep 10\n",
        )
        .unwrap();
        fs::set_permissions(&sandbox, fs::Permissions::from_mode(0o700)).unwrap();
        let (session, mut events) =
            TerminalSession::spawn(spec(root.path()), TerminalSize::default()).unwrap();
        output_until(&mut events, "READY").await;
        let session = std::sync::Arc::new(session);
        let writer = session.clone();
        let (done, result) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let bytes = vec![b'x'; 64 * 1024];
            for _ in 0..64 {
                if let Err(error) = writer.input(&bytes) {
                    done.send(error.kind()).unwrap();
                    return;
                }
            }
            panic!("terminal input must have a bounded queue");
        });
        let backpressure = result.recv_timeout(Duration::from_secs(1));
        session.stop_and_wait().unwrap();
        worker.join().unwrap();
        assert_eq!(backpressure.unwrap(), std::io::ErrorKind::WouldBlock);
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
    async fn stop_force_kills_a_child_that_ignores_graceful_signals() {
        let root = tempfile::tempdir().unwrap();
        let sandbox = root.path().join("fake-sandbox");
        fs::write(
            &sandbox,
            r#"#!/bin/sh
trap '' TERM HUP
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
        session.request_stop().unwrap();
        let exited = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match events.recv().await.unwrap() {
                    TerminalEvent::Exited { signal, .. } => break signal,
                    TerminalEvent::Error(message) => panic!("terminal error: {message}"),
                    TerminalEvent::Output(_) => {}
                }
            }
        })
        .await;
        if exited.is_err() {
            unsafe {
                libc::kill(-session.process_id, libc::SIGKILL);
            }
            tokio::time::timeout(Duration::from_secs(1), async {
                while !session.is_exited() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }

        let signal = exited.expect("stop did not force-kill the unresponsive terminal child");
        let expected_signal =
            unsafe { std::ffi::CStr::from_ptr(libc::strsignal(libc::SIGKILL)) }.to_string_lossy();
        assert_eq!(signal.as_deref(), Some(expected_signal.as_ref()));
        assert!(started.elapsed() >= super::STOP_GRACE);
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
