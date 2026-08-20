use super::super::super::{FilesystemHookRuntime, LocalByteRange, OpenFile, fail, mapping};
use super::super::backend::{ContentReadMode, ContentWriteMode, ContentWritePosition};
use super::super::state::ManagedContent;
use crate::filesystem::broker::LocalOpenStateGuard;
use anyhow::Result;

const READ_AHEAD_MIN_BYTES: u64 = 16 * 1024;
pub(crate) const READ_AHEAD_MAX_BYTES: u64 = 256 * 1024;
const READ_AHEAD_MULTIPLIER: u64 = 4;

#[derive(Clone, Copy)]
pub(crate) enum ContentIoOffset {
    Sequential,
    Positioned(libc::off_t),
}

pub(in super::super) struct ReadOperations<'a> {
    pub(in super::super) requested_length:
        &'a mut dyn FnMut() -> std::result::Result<usize, libc::c_int>,
    pub(in super::super) copy_from_payload: &'a mut dyn FnMut(libc::c_int, usize) -> libc::ssize_t,
    pub(in super::super) positioned: &'a mut dyn FnMut(libc::off_t) -> libc::ssize_t,
    pub(in super::super) native: &'a mut dyn FnMut() -> libc::ssize_t,
}

pub(in super::super) struct WriteOperations<'a> {
    pub(in super::super) requested_length:
        &'a mut dyn FnMut() -> std::result::Result<usize, libc::c_int>,
    pub(in super::super) copy_to_payload: &'a mut dyn FnMut(libc::c_int, usize) -> libc::ssize_t,
    pub(in super::super) positioned: &'a mut dyn FnMut(libc::off_t) -> libc::ssize_t,
    pub(in super::super) native: &'a mut dyn FnMut() -> libc::ssize_t,
}

#[derive(Clone, Copy)]
struct WriteRequest {
    offset: ContentIoOffset,
    reservation_length: Option<usize>,
}

pub(crate) unsafe fn managed_seek_io(
    descriptor: libc::c_int,
    requested_offset: libc::off_t,
    whence: libc::c_int,
    mut native: impl FnMut() -> libc::off_t,
) -> Option<libc::off_t> {
    let runtime = FilesystemHookRuntime::global()?;
    let _operation = runtime.operations.acquire(
        mapping::OperationRequest::new()
            .descriptor_registry_shared()
            .descriptor_shared(descriptor),
    );
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
    let _operation = runtime.operations.acquire(
        mapping::OperationRequest::new()
            .descriptor_registry_shared()
            .descriptor_shared(descriptor),
    );
    let open = runtime.tracked_open(descriptor)?;
    let content = open.managed();
    let _mutation = content.mutation_guard();
    let logical = content
        .backend
        .logical_open_state()
        .map(|state| state.lock())
        .transpose()
        .map_err(anyhow::Error::from);
    let mut operations = ReadOperations {
        requested_length: &mut requested_length,
        copy_from_payload: &mut payload,
        positioned: &mut positioned,
        native: &mut native,
    };
    Some(
        match logical.and_then(|logical| {
            read_with_policy(
                content,
                runtime,
                descriptor,
                requested_offset,
                logical.as_ref(),
                &mut operations,
            )
        }) {
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
    let _operation = runtime.operations.acquire(
        mapping::OperationRequest::new()
            .descriptor_registry_shared()
            .descriptor_shared(descriptor),
    );
    let open = runtime.tracked_open(descriptor)?;
    let content = open.managed();
    let _mutation = content.mutation_guard();
    let logical = content
        .backend
        .logical_open_state()
        .map(|state| state.lock())
        .transpose()
        .map_err(anyhow::Error::from);
    let mut operations = WriteOperations {
        requested_length: &mut requested_length,
        copy_to_payload: &mut payload,
        positioned: &mut positioned,
        native: &mut native,
    };
    Some(
        match logical.and_then(|logical| {
            write_with_policy(
                content,
                runtime,
                descriptor,
                &open,
                WriteRequest {
                    offset: requested_offset,
                    reservation_length,
                },
                logical.as_ref(),
                &mut operations,
            )
        }) {
            Ok(result) => result,
            Err(error) => unsafe { fail(&error, -1) },
        },
    )
}

fn read_with_policy(
    content: &ManagedContent,
    runtime: &FilesystemHookRuntime,
    descriptor: libc::c_int,
    requested_offset: ContentIoOffset,
    logical: Option<&LocalOpenStateGuard<'_>>,
    operations: &mut ReadOperations<'_>,
) -> Result<libc::ssize_t> {
    let mode = content.backend.read_mode();
    if mode == ContentReadMode::Native {
        return Ok(match requested_offset {
            ContentIoOffset::Sequential => (operations.native)(),
            ContentIoOffset::Positioned(offset) => (operations.positioned)(offset),
        });
    }

    let flags = status_flags(descriptor, logical)?;
    if flags & libc::O_ACCMODE == libc::O_WRONLY {
        return Err(errno_error(libc::EBADF));
    }
    let offset = resolve_offset(descriptor, requested_offset, logical)?;
    if matches!(mode, ContentReadMode::Positioned { materialize: true }) {
        let length = (operations.requested_length)().map_err(errno_error)?;
        if length != 0 {
            let start = u64::try_from(offset).map_err(|_| errno_error(libc::EINVAL))?;
            let requested = u64::try_from(length).unwrap_or(u64::MAX);
            let end = start.saturating_add(read_materialization_length(requested));
            let range =
                LocalByteRange::new(start, end).expect("non-empty read materialization range");
            content
                .backend
                .materialize(&content.state, runtime, Some(range))?;
        }
    }
    let result = match mode {
        ContentReadMode::Positioned { .. } => (operations.positioned)(offset),
        ContentReadMode::Direct => content.backend.direct_read(
            &content.state,
            runtime,
            u64::try_from(offset).map_err(|_| errno_error(libc::EINVAL))?,
            operations,
        )?,
        ContentReadMode::Native => unreachable!("native read returned above"),
    };
    if result > 0 && matches!(requested_offset, ContentIoOffset::Sequential) {
        set_offset_after_io(descriptor, logical, offset as u64, result as u64)?;
    }
    Ok(result)
}

fn write_with_policy(
    content: &ManagedContent,
    runtime: &FilesystemHookRuntime,
    descriptor: libc::c_int,
    open: &OpenFile,
    request: WriteRequest,
    logical: Option<&LocalOpenStateGuard<'_>>,
    operations: &mut WriteOperations<'_>,
) -> Result<libc::ssize_t> {
    if let ContentWriteMode::Native { track_snapshot } = content.backend.write_mode() {
        return native_write_with_policy(
            content,
            runtime,
            descriptor,
            open,
            request.offset,
            track_snapshot,
            operations,
        );
    }
    if !content.state.writable {
        return Err(errno_error(libc::EBADF));
    }
    let flags = status_flags(descriptor, logical)?;
    if flags & libc::O_ACCMODE == libc::O_RDONLY {
        return Err(errno_error(libc::EBADF));
    }
    let position = match request.offset {
        ContentIoOffset::Sequential
            if request.reservation_length != Some(0) && flags & libc::O_APPEND != 0 =>
        {
            ContentWritePosition::Append
        }
        ContentIoOffset::Sequential => ContentWritePosition::At(
            u64::try_from(current_offset(descriptor, logical)?)
                .map_err(|_| errno_error(libc::EINVAL))?,
        ),
        ContentIoOffset::Positioned(offset) => {
            ContentWritePosition::At(u64::try_from(offset).map_err(|_| errno_error(libc::EINVAL))?)
        }
    };
    let completed = content.backend.write_explicit(
        &content.state,
        runtime,
        descriptor,
        position,
        request.reservation_length,
        operations,
    )?;
    if completed.result <= 0 {
        return Ok(completed.result);
    }
    let start = completed.start.ok_or_else(|| errno_error(libc::EIO))?;
    let range = LocalByteRange::new(start, start.saturating_add(completed.result as u64))?;
    if !completed.published {
        content.state.record_write(range);
    }
    if matches!(request.offset, ContentIoOffset::Sequential) {
        set_offset_after_io(descriptor, logical, start, completed.result as u64)?;
    }
    if flags & (libc::O_SYNC | libc::O_DSYNC) != 0
        && let Err(error) = content
            .backend
            .sync(&content.state, runtime, descriptor, open, true)
    {
        if completed.recoverable {
            content.state.record_write(range);
        }
        return Err(error);
    }
    Ok(completed.result)
}

fn native_write_with_policy(
    content: &ManagedContent,
    runtime: &FilesystemHookRuntime,
    descriptor: libc::c_int,
    open: &OpenFile,
    requested_offset: ContentIoOffset,
    track_snapshot: bool,
    operations: &mut WriteOperations<'_>,
) -> Result<libc::ssize_t> {
    let before = (track_snapshot && matches!(requested_offset, ContentIoOffset::Sequential))
        .then(|| native_offset(descriptor).ok())
        .flatten();
    let result = match requested_offset {
        ContentIoOffset::Sequential => (operations.native)(),
        ContentIoOffset::Positioned(offset) => (operations.positioned)(offset),
    };
    if result <= 0 || !track_snapshot {
        return Ok(result);
    }
    let range = match requested_offset {
        ContentIoOffset::Sequential => sequential_write_range(descriptor, before, result),
        ContentIoOffset::Positioned(offset) => positional_write_range(offset, result),
    };
    if let Some(range) = range {
        content.state.record_write(range);
    }
    let flags = status_flags(descriptor, None)?;
    if flags & (libc::O_SYNC | libc::O_DSYNC) != 0 {
        content
            .backend
            .sync(&content.state, runtime, descriptor, open, true)?;
    }
    Ok(result)
}

fn status_flags(
    descriptor: libc::c_int,
    logical: Option<&LocalOpenStateGuard<'_>>,
) -> Result<libc::c_int> {
    if let Some(logical) = logical {
        return logical.flags().map_err(Into::into);
    }
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(flags)
    }
}

fn resolve_offset(
    descriptor: libc::c_int,
    requested: ContentIoOffset,
    logical: Option<&LocalOpenStateGuard<'_>>,
) -> Result<libc::off_t> {
    let offset = match requested {
        ContentIoOffset::Sequential => current_offset(descriptor, logical),
        ContentIoOffset::Positioned(offset) => Ok(offset),
    }?;
    if offset < 0 {
        Err(errno_error(libc::EINVAL))
    } else {
        Ok(offset)
    }
}

fn current_offset(
    descriptor: libc::c_int,
    logical: Option<&LocalOpenStateGuard<'_>>,
) -> Result<libc::off_t> {
    match logical {
        Some(logical) => logical.offset().map_err(Into::into),
        None => native_offset(descriptor),
    }
}

fn native_offset(descriptor: libc::c_int) -> Result<libc::off_t> {
    let offset = unsafe { libc::lseek(descriptor, 0, libc::SEEK_CUR) };
    if offset < 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(offset)
    }
}

fn set_offset_after_io(
    descriptor: libc::c_int,
    logical: Option<&LocalOpenStateGuard<'_>>,
    start: u64,
    length: u64,
) -> Result<()> {
    let next = start
        .checked_add(length)
        .and_then(|next| libc::off_t::try_from(next).ok())
        .ok_or_else(|| errno_error(libc::EOVERFLOW))?;
    match logical {
        Some(logical) => logical.set_offset(next).map_err(Into::into),
        None if unsafe { libc::lseek(descriptor, next, libc::SEEK_SET) } < 0 => {
            Err(std::io::Error::last_os_error().into())
        }
        None => Ok(()),
    }
}

pub(super) fn sequential_write_range(
    descriptor: libc::c_int,
    before: Option<libc::off_t>,
    written: libc::ssize_t,
) -> Option<LocalByteRange> {
    let written = u64::try_from(written).ok()?;
    let before = before.and_then(|offset| u64::try_from(offset).ok());
    let after = native_offset(descriptor)
        .ok()
        .and_then(|offset| u64::try_from(offset).ok());
    let after_start = after.and_then(|after| after.checked_sub(written));
    let before_end = before.and_then(|before| before.checked_add(written));
    let start = match (before, after_start) {
        (Some(before), Some(after)) => before.min(after),
        (Some(before), None) => before,
        (None, Some(after)) => after,
        (None, None) => return None,
    };
    let end = match (before_end, after) {
        (Some(before), Some(after)) => before.max(after),
        (Some(before), None) => before,
        (None, Some(after)) => after,
        (None, None) => return None,
    };
    LocalByteRange::new(start, end).ok()
}

fn positional_write_range(offset: libc::off_t, written: libc::ssize_t) -> Option<LocalByteRange> {
    let start = u64::try_from(offset).ok()?;
    LocalByteRange::new(start, start.checked_add(u64::try_from(written).ok()?)?).ok()
}

pub(crate) fn errno_error(errno: libc::c_int) -> anyhow::Error {
    std::io::Error::from_raw_os_error(errno).into()
}

pub(crate) fn positional_reservation_range(
    start: u64,
    length: Option<usize>,
) -> Option<super::super::super::LocalByteRange> {
    match length {
        Some(0) => None,
        Some(length) => {
            super::super::super::LocalByteRange::new(start, start.saturating_add(length as u64))
                .ok()
        }
        None => super::super::super::LocalByteRange::new(start, u64::MAX).ok(),
    }
}

pub(crate) fn read_materialization_length(requested: u64) -> u64 {
    requested.max(
        requested
            .saturating_mul(READ_AHEAD_MULTIPLIER)
            .clamp(READ_AHEAD_MIN_BYTES, READ_AHEAD_MAX_BYTES),
    )
}
