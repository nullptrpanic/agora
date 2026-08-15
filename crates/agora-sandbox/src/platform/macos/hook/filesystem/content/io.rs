use super::super::{FilesystemHookRuntime, fail};
pub(crate) use super::backend::ContentIoOffset;
use super::backend::{ReadOperations, WriteOperations};

const READ_AHEAD_MIN_BYTES: u64 = 16 * 1024;
pub(crate) const READ_AHEAD_MAX_BYTES: u64 = 256 * 1024;
const READ_AHEAD_MULTIPLIER: u64 = 4;

pub(crate) unsafe fn managed_seek_io(
    descriptor: libc::c_int,
    requested_offset: libc::off_t,
    whence: libc::c_int,
    mut native: impl FnMut() -> libc::off_t,
) -> Option<libc::off_t> {
    let runtime = FilesystemHookRuntime::global()?;
    let open = runtime.tracked_open(descriptor)?;
    let content = open.managed();
    let _mutation = content.mutation_guard();
    Some(
        match content.backend.seek(
            &content.state,
            runtime,
            descriptor,
            requested_offset,
            whence,
            &mut native,
        ) {
            Ok(result) => result,
            Err(error) => unsafe { fail(&error, -1) },
        },
    )
}

pub(crate) unsafe fn managed_read_io<Length, Payload, Positioned, Native>(
    descriptor: libc::c_int,
    requested_offset: ContentIoOffset,
    mut requested_length: Length,
    mut payload: Payload,
    mut positioned: Positioned,
    mut native: Native,
) -> Option<libc::ssize_t>
where
    Length: FnMut() -> std::result::Result<usize, libc::c_int>,
    Payload: FnMut(libc::c_int, usize) -> libc::ssize_t,
    Positioned: FnMut(libc::off_t) -> libc::ssize_t,
    Native: FnMut() -> libc::ssize_t,
{
    let runtime = FilesystemHookRuntime::global()?;
    let open = runtime.tracked_open(descriptor)?;
    let content = open.managed();
    let _mutation = content.mutation_guard();
    let mut operations = ReadOperations {
        requested_length: &mut requested_length,
        copy_from_payload: &mut payload,
        positioned: &mut positioned,
        native: &mut native,
    };
    Some(
        match content.backend.read(
            &content.state,
            runtime,
            descriptor,
            requested_offset,
            &mut operations,
        ) {
            Ok(result) => result,
            Err(error) => unsafe { fail(&error, -1) },
        },
    )
}

pub(crate) unsafe fn managed_write_io<Length, Payload, Positioned, Native>(
    descriptor: libc::c_int,
    requested_offset: ContentIoOffset,
    reservation_length: Option<usize>,
    mut requested_length: Length,
    mut payload: Payload,
    mut positioned: Positioned,
    mut native: Native,
) -> Option<libc::ssize_t>
where
    Length: FnMut() -> std::result::Result<usize, libc::c_int>,
    Payload: FnMut(libc::c_int, usize) -> libc::ssize_t,
    Positioned: FnMut(libc::off_t) -> libc::ssize_t,
    Native: FnMut() -> libc::ssize_t,
{
    let runtime = FilesystemHookRuntime::global()?;
    let open = runtime.tracked_open(descriptor)?;
    let content = open.managed();
    let _mutation = content.mutation_guard();
    let mut operations = WriteOperations {
        requested_length: &mut requested_length,
        copy_to_payload: &mut payload,
        positioned: &mut positioned,
        native: &mut native,
    };
    Some(
        match content.backend.write(
            &content.state,
            runtime,
            descriptor,
            requested_offset,
            reservation_length,
            &mut operations,
        ) {
            Ok(result) => result,
            Err(error) => unsafe { fail(&error, -1) },
        },
    )
}

pub(super) fn errno_error(errno: libc::c_int) -> anyhow::Error {
    std::io::Error::from_raw_os_error(errno).into()
}

pub(super) fn positional_reservation_range(
    start: u64,
    length: Option<usize>,
) -> Option<super::super::LocalByteRange> {
    match length {
        Some(0) => None,
        Some(length) => {
            super::super::LocalByteRange::new(start, start.saturating_add(length as u64)).ok()
        }
        None => super::super::LocalByteRange::new(start, u64::MAX).ok(),
    }
}

pub(crate) fn read_materialization_length(requested: u64) -> u64 {
    requested.max(
        requested
            .saturating_mul(READ_AHEAD_MULTIPLIER)
            .clamp(READ_AHEAD_MIN_BYTES, READ_AHEAD_MAX_BYTES),
    )
}
