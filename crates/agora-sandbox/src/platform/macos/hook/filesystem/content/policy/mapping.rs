use super::super::super::{FilesystemHookRuntime, LocalByteRange};
use super::super::state::ManagedContent;
use anyhow::Result;

pub(crate) fn prepare_mapping(
    content: &ManagedContent,
    runtime: &FilesystemHookRuntime,
    descriptor: libc::c_int,
    range: LocalByteRange,
    protection: libc::c_int,
    flags: libc::c_int,
) -> Result<bool> {
    let native_flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if native_flags < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let logical_flags = content
        .backend
        .merge_status_flags(&content.state, native_flags)?;
    validate_mapping_access(logical_flags, protection, flags)?;
    content
        .backend
        .materialize(&content.state, runtime, Some(range))?;

    let writable = flags & libc::MAP_SHARED != 0 && content.state.writable;
    if writable {
        content
            .backend
            .potentially_dirty(&content.state, runtime, range)?;
    }
    Ok(writable)
}

pub(super) fn validate_mapping_access(
    descriptor_flags: libc::c_int,
    protection: libc::c_int,
    mapping_flags: libc::c_int,
) -> std::io::Result<()> {
    let access = descriptor_flags & libc::O_ACCMODE;
    if access == libc::O_WRONLY
        || (mapping_flags & libc::MAP_SHARED != 0
            && protection & libc::PROT_WRITE != 0
            && access == libc::O_RDONLY)
    {
        return Err(std::io::Error::from_raw_os_error(libc::EACCES));
    }
    Ok(())
}
