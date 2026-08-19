use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::FileExt;
use std::sync::{Mutex, MutexGuard};

const STATE_SIZE: u64 = 24;
const MAGIC_OFFSET: u64 = 8;
const FLAGS_OFFSET: u64 = 12;
const OFFSET_OFFSET: u64 = 16;
const STATE_MAGIC: u32 = 0x4147_4f52;

pub(crate) const LOCAL_STATUS_FLAGS_MASK: libc::c_int =
    libc::O_ACCMODE | libc::O_APPEND | libc::O_NONBLOCK | libc::O_SYNC | libc::O_DSYNC;

pub(crate) struct LocalOpenState {
    file: File,
    process_lock: Mutex<()>,
}

pub(crate) struct LocalOpenStateGuard<'a> {
    state: &'a LocalOpenState,
    _process_lock: MutexGuard<'a, ()>,
}

impl LocalOpenState {
    pub(crate) fn create(flags: libc::c_int) -> io::Result<Self> {
        let state = Self {
            file: tempfile::tempfile()?,
            process_lock: Mutex::new(()),
        };
        state.file.set_len(STATE_SIZE)?;
        write_all_at(&state.file, &STATE_MAGIC.to_ne_bytes(), MAGIC_OFFSET)?;
        write_all_at(&state.file, &flags.to_ne_bytes(), FLAGS_OFFSET)?;
        write_all_at(&state.file, &0_i64.to_ne_bytes(), OFFSET_OFFSET)?;
        Ok(state)
    }

    pub(crate) fn from_descriptor(descriptor: OwnedFd) -> io::Result<Self> {
        let state = Self {
            file: File::from(descriptor),
            process_lock: Mutex::new(()),
        };
        if state.file.metadata()?.len() != STATE_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid local open state length",
            ));
        }
        let mut magic = [0_u8; 4];
        read_exact_at(&state.file, &mut magic, MAGIC_OFFSET)?;
        if u32::from_ne_bytes(magic) != STATE_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid local open state marker",
            ));
        }
        Ok(state)
    }

    pub(super) fn try_clone_file(&self) -> io::Result<File> {
        self.file.try_clone()
    }

    pub(crate) fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    pub(crate) fn set_close_on_exec(&self, close_on_exec: bool) -> io::Result<()> {
        let flags = unsafe { libc::fcntl(self.as_raw_fd(), libc::F_GETFD) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let flags = if close_on_exec {
            flags | libc::FD_CLOEXEC
        } else {
            flags & !libc::FD_CLOEXEC
        };
        if unsafe { libc::fcntl(self.as_raw_fd(), libc::F_SETFD, flags) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) fn lock(&self) -> io::Result<LocalOpenStateGuard<'_>> {
        let process_lock = self
            .process_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_lock(self.as_raw_fd(), libc::F_WRLCK, libc::F_SETLKW)?;
        Ok(LocalOpenStateGuard {
            state: self,
            _process_lock: process_lock,
        })
    }
}

impl LocalOpenStateGuard<'_> {
    pub(crate) fn offset(&self) -> io::Result<libc::off_t> {
        let mut bytes = [0_u8; 8];
        read_exact_at(&self.state.file, &mut bytes, OFFSET_OFFSET)?;
        Ok(i64::from_ne_bytes(bytes))
    }

    pub(crate) fn set_offset(&self, offset: libc::off_t) -> io::Result<()> {
        write_all_at(&self.state.file, &offset.to_ne_bytes(), OFFSET_OFFSET)
    }

    pub(crate) fn flags(&self) -> io::Result<libc::c_int> {
        let mut bytes = [0_u8; 4];
        read_exact_at(&self.state.file, &mut bytes, FLAGS_OFFSET)?;
        Ok(i32::from_ne_bytes(bytes))
    }

    pub(crate) fn set_flags(&self, flags: libc::c_int) -> io::Result<()> {
        write_all_at(&self.state.file, &flags.to_ne_bytes(), FLAGS_OFFSET)
    }
}

impl Drop for LocalOpenStateGuard<'_> {
    fn drop(&mut self) {
        let _ = set_lock(self.state.as_raw_fd(), libc::F_UNLCK, libc::F_SETLK);
    }
}

fn set_lock(descriptor: RawFd, kind: libc::c_short, command: libc::c_int) -> io::Result<()> {
    let mut lock = libc::flock {
        l_start: 0,
        l_len: 1,
        l_pid: 0,
        l_type: kind,
        l_whence: libc::SEEK_SET as libc::c_short,
    };
    loop {
        if unsafe { libc::fcntl(descriptor, command, &raw mut lock) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buffer.is_empty() {
        match file.read_at(buffer, offset)? {
            0 => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            count => {
                buffer = &mut buffer[count..];
                offset += count as u64;
            }
        }
    }
    Ok(())
}

fn write_all_at(file: &File, mut buffer: &[u8], mut offset: u64) -> io::Result<()> {
    while !buffer.is_empty() {
        match file.write_at(buffer, offset)? {
            0 => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            count => {
                buffer = &buffer[count..];
                offset += count as u64;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn state_round_trips_flags_offset_and_descriptor_policy() {
        let state = LocalOpenState::create(libc::O_RDWR | libc::O_APPEND).unwrap();
        {
            let guard = state.lock().unwrap();
            assert_eq!(guard.flags().unwrap(), libc::O_RDWR | libc::O_APPEND);
            assert_eq!(guard.offset().unwrap(), 0);
            guard.set_flags(libc::O_RDONLY).unwrap();
            guard.set_offset(37).unwrap();
        }
        state.set_close_on_exec(false).unwrap();
        assert_eq!(
            unsafe { libc::fcntl(state.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let restored =
            LocalOpenState::from_descriptor(state.try_clone_file().unwrap().into()).unwrap();
        let guard = restored.lock().unwrap();
        assert_eq!(guard.flags().unwrap(), libc::O_RDONLY);
        assert_eq!(guard.offset().unwrap(), 37);
    }

    #[test]
    fn state_lock_serializes_threads_in_the_same_process() {
        let state = Arc::new(LocalOpenState::create(libc::O_RDWR).unwrap());
        let guard = state.lock().unwrap();
        let (acquired, receiver) = mpsc::channel();
        let peer = Arc::clone(&state);
        let thread = std::thread::spawn(move || {
            let _guard = peer.lock().unwrap();
            acquired.send(()).unwrap();
        });

        assert!(receiver.recv_timeout(Duration::from_millis(50)).is_err());
        drop(guard);
        receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        thread.join().unwrap();
    }
}
