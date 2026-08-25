//! Unix socket framing and descriptor transfer shared by sandbox brokers.

#[cfg(target_os = "macos")]
use std::cell::UnsafeCell;
#[cfg(all(target_os = "macos", any(agora_sandbox_hook_build, test, coverage)))]
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
#[cfg(target_os = "macos")]
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;

pub(crate) const MAX_FRAME_SIZE: usize = 1024 * 1024;
const MAX_DESCRIPTORS: usize = 8;
const FRAME_MARKER: u8 = 0;
#[cfg(target_os = "macos")]
const INHERITED_CONTROL_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(target_os = "macos")]
const INHERITED_CONTROL_LOCK_RETRY_DELAY: Duration = Duration::from_millis(1);

#[cfg(target_os = "macos")]
pub(crate) struct InheritedControlLock {
    descriptor: OwnedFd,
}

#[cfg(target_os = "macos")]
impl InheritedControlLock {
    #[cfg(any(agora_sandbox_hook_build, test, coverage))]
    pub(crate) fn anonymous() -> io::Result<Arc<Self>> {
        let file = tempfile::tempfile()?;
        Self::from_file(file)
    }

    #[cfg(test)]
    pub(crate) unsafe fn from_raw_descriptor(descriptor: RawFd) -> io::Result<Arc<Self>> {
        if descriptor < 0 || unsafe { libc::fcntl(descriptor, libc::F_GETFD) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Self::from_owned_descriptor(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }

    #[cfg(any(agora_sandbox_hook_build, test, coverage))]
    fn from_file(file: File) -> io::Result<Arc<Self>> {
        Self::from_owned_descriptor(file.into())
    }

    #[cfg(any(agora_sandbox_hook_build, test, coverage))]
    pub(crate) fn from_owned_descriptor(descriptor: OwnedFd) -> io::Result<Arc<Self>> {
        make_inheritable(descriptor.as_raw_fd())?;
        Ok(Arc::new(Self { descriptor }))
    }

    pub(crate) fn descriptor(&self) -> RawFd {
        self.descriptor.as_raw_fd()
    }

    fn lock_until(
        &self,
        slot: i64,
        deadline: Instant,
    ) -> io::Result<InheritedControlLockGuard<'_>> {
        loop {
            match set_record_lock(self.descriptor(), slot, libc::F_WRLCK, libc::F_SETLK) {
                Ok(()) => return Ok(InheritedControlLockGuard { lock: self, slot }),
                Err(error) if is_record_lock_contention(&error) => {
                    wait_for_inherited_control_lock(deadline)?;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(target_os = "macos")]
struct InheritedControlLockGuard<'a> {
    lock: &'a InheritedControlLock,
    slot: i64,
}

#[cfg(target_os = "macos")]
impl Drop for InheritedControlLockGuard<'_> {
    fn drop(&mut self) {
        let _ = set_record_lock(
            self.lock.descriptor(),
            self.slot,
            libc::F_UNLCK,
            libc::F_SETLK,
        );
    }
}

#[cfg(target_os = "macos")]
pub(crate) struct InheritedControlStream<S> {
    stream: UnsafeCell<S>,
    mutex: UnsafeCell<libc::pthread_mutex_t>,
    lock: Arc<InheritedControlLock>,
    slot: i64,
}

#[cfg(target_os = "macos")]
unsafe impl<S: Send> Send for InheritedControlStream<S> {}
#[cfg(target_os = "macos")]
unsafe impl<S: Send> Sync for InheritedControlStream<S> {}

#[cfg(target_os = "macos")]
impl<S> InheritedControlStream<S>
where
    S: AsRawFd,
{
    #[cfg(any(agora_sandbox_hook_build, test, coverage))]
    pub(crate) fn new(
        stream: S,
        lock: Arc<InheritedControlLock>,
        slot: i64,
    ) -> io::Result<Arc<Self>> {
        make_inheritable(stream.as_raw_fd())?;
        Ok(Arc::new(Self {
            stream: UnsafeCell::new(stream),
            mutex: UnsafeCell::new(libc::PTHREAD_MUTEX_INITIALIZER),
            lock,
            slot,
        }))
    }

    pub(crate) fn descriptor(&self) -> RawFd {
        unsafe { &*self.stream.get() }.as_raw_fd()
    }

    pub(crate) fn transact<T>(&self, operation: impl FnOnce(&mut S) -> T) -> io::Result<T> {
        self.transact_for(INHERITED_CONTROL_LOCK_TIMEOUT, operation)
    }

    fn transact_for<T>(
        &self,
        timeout: Duration,
        operation: impl FnOnce(&mut S) -> T,
    ) -> io::Result<T> {
        let _signals = crate::platform::hook::SignalMaskGuard::block()?;
        let deadline = Instant::now() + timeout;
        let _mutex = RawMutexGuard::lock_until(self.mutex.get(), deadline)?;
        let _process = self.lock.lock_until(self.slot, deadline)?;
        Ok(operation(unsafe { &mut *self.stream.get() }))
    }

    #[cfg(any(agora_sandbox_hook_build, test, coverage))]
    pub(crate) unsafe fn reset_after_fork(&self) {
        unsafe {
            std::ptr::write(self.mutex.get(), libc::PTHREAD_MUTEX_INITIALIZER);
        }
    }
}

#[cfg(target_os = "macos")]
impl<S> std::fmt::Debug for InheritedControlStream<S>
where
    S: AsRawFd,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InheritedControlStream")
            .field("descriptor", &self.descriptor())
            .field("slot", &self.slot)
            .finish_non_exhaustive()
    }
}

#[cfg(target_os = "macos")]
impl<S> Drop for InheritedControlStream<S> {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_mutex_destroy(self.mutex.get());
        }
    }
}

#[cfg(target_os = "macos")]
struct RawMutexGuard {
    mutex: *mut libc::pthread_mutex_t,
}

#[cfg(target_os = "macos")]
impl RawMutexGuard {
    fn lock_until(mutex: *mut libc::pthread_mutex_t, deadline: Instant) -> io::Result<Self> {
        loop {
            let result = unsafe { libc::pthread_mutex_trylock(mutex) };
            match result {
                0 => return Ok(Self { mutex }),
                libc::EBUSY => wait_for_inherited_control_lock(deadline)?,
                error => return Err(io::Error::from_raw_os_error(error)),
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for RawMutexGuard {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_mutex_unlock(self.mutex);
        }
    }
}

#[cfg(target_os = "macos")]
#[cfg(any(agora_sandbox_hook_build, test, coverage))]
fn make_inheritable(descriptor: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn set_record_lock(
    descriptor: RawFd,
    slot: i64,
    lock_type: libc::c_short,
    command: libc::c_int,
) -> io::Result<()> {
    let mut lock = libc::flock {
        l_start: slot,
        l_len: 1,
        l_pid: 0,
        l_type: lock_type,
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

#[cfg(target_os = "macos")]
fn is_record_lock_contention(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::EACCES | libc::EAGAIN))
}

#[cfg(target_os = "macos")]
fn wait_for_inherited_control_lock(deadline: Instant) -> io::Result<()> {
    let now = Instant::now();
    if now >= deadline {
        return Err(io::Error::from_raw_os_error(libc::ETIMEDOUT));
    }
    std::thread::sleep(INHERITED_CONTROL_LOCK_RETRY_DELAY.min(deadline - now));
    Ok(())
}

pub(crate) fn send<T: Serialize>(
    stream: &mut UnixStream,
    message: &T,
    descriptor: Option<RawFd>,
) -> io::Result<()> {
    let descriptors = descriptor.as_slice();
    send_with_descriptors(stream, message, descriptors)
}

pub(crate) fn send_with_descriptors<T: Serialize>(
    stream: &mut UnixStream,
    message: &T,
    descriptors: &[RawFd],
) -> io::Result<()> {
    configure_no_sigpipe(stream.as_raw_fd())?;
    let payload = serde_json::to_vec(message).map_err(invalid_data)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote filesystem frame is too large",
        ));
    }
    send_marker(stream, descriptors)?;
    stream.write_all(
        &u32::try_from(payload.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame is too large"))?
            .to_be_bytes(),
    )?;
    stream.write_all(&payload)
}

pub(crate) fn receive<T: DeserializeOwned>(
    stream: &mut UnixStream,
) -> io::Result<(T, Option<OwnedFd>)> {
    let (message, mut descriptors) = receive_with_descriptors(stream)?;
    if descriptors.len() > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid descriptor control message",
        ));
    }
    Ok((message, descriptors.pop()))
}

pub(crate) fn receive_with_descriptors<T: DeserializeOwned>(
    stream: &mut UnixStream,
) -> io::Result<(T, Vec<OwnedFd>)> {
    let descriptors = receive_marker(stream)?;
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote filesystem frame is too large",
        ));
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload)?;
    let message = serde_json::from_slice(&payload).map_err(invalid_data)?;
    Ok((message, descriptors))
}

fn send_marker(stream: &UnixStream, descriptors: &[RawFd]) -> io::Result<()> {
    if descriptors.is_empty() {
        return (&*stream).write_all(&[FRAME_MARKER]);
    }
    if descriptors.len() > MAX_DESCRIPTORS || descriptors.iter().any(|descriptor| *descriptor < 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid descriptor control message",
        ));
    }
    let mut marker = [FRAME_MARKER];
    let mut iov = libc::iovec {
        iov_base: marker.as_mut_ptr().cast(),
        iov_len: marker.len(),
    };
    let descriptor_bytes = descriptors
        .len()
        .checked_mul(size_of::<RawFd>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "too many descriptors"))?;
    let control_length = unsafe { libc::CMSG_SPACE(descriptor_bytes as u32) as usize };
    let mut control = vec![0_u8; control_length];
    let mut header = unsafe { zeroed::<libc::msghdr>() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = control.len() as _;
    unsafe {
        let message = libc::CMSG_FIRSTHDR(&header);
        if message.is_null() {
            return Err(io::Error::other(
                "failed to create descriptor control message",
            ));
        }
        (*message).cmsg_level = libc::SOL_SOCKET;
        (*message).cmsg_type = libc::SCM_RIGHTS;
        (*message).cmsg_len = libc::CMSG_LEN(descriptor_bytes as u32) as _;
        std::ptr::copy_nonoverlapping(
            descriptors.as_ptr(),
            libc::CMSG_DATA(message).cast::<RawFd>(),
            descriptors.len(),
        );
        header.msg_controllen = (*message).cmsg_len as _;
    }
    let flags = send_flags();
    let written = unsafe { libc::sendmsg(stream.as_raw_fd(), &header, flags) };
    if written == 1 {
        Ok(())
    } else if written < 0 {
        Err(io::Error::last_os_error())
    } else {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "failed to send remote filesystem frame marker",
        ))
    }
}

fn receive_marker(stream: &UnixStream) -> io::Result<Vec<OwnedFd>> {
    let mut marker = [0_u8; 1];
    let mut iov = libc::iovec {
        iov_base: marker.as_mut_ptr().cast(),
        iov_len: marker.len(),
    };
    let control_length =
        unsafe { libc::CMSG_SPACE((MAX_DESCRIPTORS * size_of::<RawFd>()) as u32) as usize };
    let mut control = vec![0_u8; control_length];
    let mut header = unsafe { zeroed::<libc::msghdr>() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = control.len() as _;
    let received = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut header, libc::MSG_WAITALL) };
    if received == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "remote filesystem transport closed",
        ));
    }
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    if received != 1 || marker[0] != FRAME_MARKER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid remote filesystem frame marker",
        ));
    }
    let reported_control_length = header.msg_controllen as usize;
    let control_start = header.msg_control as usize;
    let control_end = control_start.saturating_add(reported_control_length.min(control.len()));
    let mut descriptors = Vec::new();
    let mut invalid_control =
        header.msg_flags & libc::MSG_CTRUNC != 0 || reported_control_length > control.len();
    header.msg_controllen = reported_control_length.min(control.len()) as _;
    unsafe {
        let mut message = libc::CMSG_FIRSTHDR(&header);
        while !message.is_null() {
            let message_start = message as usize;
            let base = libc::CMSG_LEN(0) as usize;
            let length = (*message).cmsg_len as usize;
            let data_start = libc::CMSG_DATA(message) as usize;
            if length < base
                || message_start < control_start
                || data_start < message_start
                || data_start > control_end
            {
                invalid_control = true;
                break;
            }
            let Some(declared_end) = message_start.checked_add(length) else {
                invalid_control = true;
                break;
            };
            let available_end = declared_end.min(control_end);
            if declared_end > control_end {
                invalid_control = true;
            }
            if (*message).cmsg_level == libc::SOL_SOCKET && (*message).cmsg_type == libc::SCM_RIGHTS
            {
                let payload = available_end.saturating_sub(data_start);
                if payload % size_of::<RawFd>() != 0 {
                    invalid_control = true;
                }
                for index in 0..(payload / size_of::<RawFd>()) {
                    let raw = std::ptr::read_unaligned(
                        libc::CMSG_DATA(message).cast::<RawFd>().add(index),
                    );
                    if raw < 0 {
                        invalid_control = true;
                    } else {
                        descriptors.push(OwnedFd::from_raw_fd(raw));
                    }
                }
            } else {
                invalid_control = true;
            }
            if declared_end > control_end {
                break;
            }
            message = libc::CMSG_NXTHDR(&header, message);
        }
    }
    if invalid_control || descriptors.len() > MAX_DESCRIPTORS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid descriptor control message",
        ));
    }
    for descriptor in &descriptors {
        let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) };
        if flags < 0
            || unsafe {
                libc::fcntl(
                    descriptor.as_raw_fd(),
                    libc::F_SETFD,
                    flags | libc::FD_CLOEXEC,
                )
            } < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(descriptors)
}

#[cfg(target_os = "macos")]
fn configure_no_sigpipe(descriptor: RawFd) -> io::Result<()> {
    let enabled: libc::c_int = 1;
    let result = unsafe {
        libc::setsockopt(
            descriptor,
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&enabled as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn configure_no_sigpipe(_descriptor: RawFd) -> io::Result<()> {
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const fn send_flags() -> libc::c_int {
    libc::MSG_NOSIGNAL
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
const fn send_flags() -> libc::c_int {
    0
}

pub(crate) fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

fn invalid_data(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests;
