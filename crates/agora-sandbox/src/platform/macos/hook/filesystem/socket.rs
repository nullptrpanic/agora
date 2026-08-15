use super::*;
use std::mem::{self, MaybeUninit};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};

type BindFn =
    unsafe extern "C" fn(libc::c_int, *const libc::sockaddr, libc::socklen_t) -> libc::c_int;

pub(in crate::platform::hook) struct UnixSocketAddress {
    address: libc::sockaddr_un,
    length: libc::socklen_t,
    temporary: Option<PathBuf>,
}

impl UnixSocketAddress {
    pub(in crate::platform::hook) fn new(path: &Path) -> Result<Self> {
        let bytes = path.as_os_str().as_bytes();
        let mut address = unsafe { MaybeUninit::<libc::sockaddr_un>::zeroed().assume_init() };
        if bytes.len() >= address.sun_path.len() {
            return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG).into());
        }
        let length = mem::offset_of!(libc::sockaddr_un, sun_path)
            .checked_add(bytes.len())
            .and_then(|length| length.checked_add(1))
            .context("Unix socket address length overflow")?;
        address.sun_len =
            u8::try_from(length).map_err(|_| io::Error::from_raw_os_error(libc::ENAMETOOLONG))?;
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                address.sun_path.as_mut_ptr().cast(),
                bytes.len(),
            );
        }
        Ok(Self {
            address,
            length: length as libc::socklen_t,
            temporary: None,
        })
    }

    fn for_overlay_path(path: &Path) -> Result<Self> {
        match Self::new(path) {
            Ok(address) => Ok(address),
            Err(error) if super::error_errno(&error) == libc::ENAMETOOLONG => {
                let address = Self::temporary()?;
                std::fs::hard_link(path, address.temporary_path()?)?;
                Ok(address)
            }
            Err(error) => Err(error),
        }
    }

    fn temporary() -> Result<Self> {
        let root = temporary_socket_root()?;
        let path = root.join(uuid::Uuid::new_v4().simple().to_string());
        let mut address = Self::new(&path)?;
        address.temporary = Some(path);
        Ok(address)
    }

    fn temporary_path(&self) -> Result<&Path> {
        self.temporary
            .as_deref()
            .context("temporary Unix socket address has no path")
    }

    pub(in crate::platform::hook) fn as_ptr(&self) -> *const libc::sockaddr {
        std::ptr::addr_of!(self.address).cast()
    }

    pub(in crate::platform::hook) fn len(&self) -> libc::socklen_t {
        self.length
    }
}

impl Drop for UnixSocketAddress {
    fn drop(&mut self) {
        let Some(path) = self.temporary.take() else {
            return;
        };
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
}

fn temporary_socket_root() -> Result<PathBuf> {
    let effective_uid = unsafe { libc::geteuid() };
    let root = Path::new("/tmp").join(format!("as-{effective_uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = root.symlink_metadata()?;
    if !metadata.is_dir() || metadata.uid() != effective_uid || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::from_raw_os_error(libc::EACCES).into());
    }
    Ok(root)
}

fn bind_overlay_socket<T>(
    path: &Path,
    bind: impl FnOnce(&UnixSocketAddress) -> Result<T>,
) -> Result<T> {
    match UnixSocketAddress::new(path) {
        Ok(address) => bind(&address),
        Err(error) if super::error_errno(&error) == libc::ENAMETOOLONG => {
            let address = UnixSocketAddress::temporary()?;
            let value = bind(&address)?;
            std::fs::hard_link(address.temporary_path()?, path)?;
            Ok(value)
        }
        Err(error) => Err(error),
    }
}

unsafe fn pathname_from_raw(
    address: *const libc::sockaddr,
    length: libc::socklen_t,
) -> Option<PathBuf> {
    let path_offset = mem::offset_of!(libc::sockaddr_un, sun_path);
    if address.is_null() || usize::try_from(length).ok()? <= path_offset {
        return None;
    }
    if unsafe { (*address).sa_family as libc::c_int } != libc::AF_UNIX {
        return None;
    }
    let available = usize::try_from(length)
        .ok()?
        .min(mem::size_of::<libc::sockaddr_un>())
        .saturating_sub(path_offset);
    let bytes =
        unsafe { std::slice::from_raw_parts(address.cast::<u8>().add(path_offset), available) };
    let path_length = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    if path_length == 0 {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(
        bytes[..path_length].to_vec(),
    )))
}

pub(in crate::platform::hook) unsafe fn is_pathname_address(
    address: *const libc::sockaddr,
    length: libc::socklen_t,
) -> bool {
    unsafe { pathname_from_raw(address, length) }.is_some()
}

pub(in crate::platform::hook) unsafe fn prepare_connect_address(
    address: *const libc::sockaddr,
    length: libc::socklen_t,
) -> Result<Option<UnixSocketAddress>> {
    let Some(path) = (unsafe { pathname_from_raw(address, length) }) else {
        return Ok(None);
    };
    let Some(_guard) = FilesystemHookGuard::enter() else {
        return Ok(None);
    };
    let Some(runtime) = FilesystemHookRuntime::global() else {
        return Ok(None);
    };
    let mapped = runtime.prepare_socket_connect(&path)?;
    if runtime.filesystem.is_internal(&mapped) {
        UnixSocketAddress::for_overlay_path(&mapped).map(Some)
    } else {
        UnixSocketAddress::new(&mapped).map(Some)
    }
}

impl FilesystemHookRuntime {
    fn logical_socket_path(&self, requested: &Path) -> Result<PathBuf> {
        if requested.is_absolute() {
            return self.logical_or_host(requested);
        }
        let base = lock(&self.current_directory).logical.clone();
        let candidate = self.logical_or_host(&base)?.join(requested);
        self.logical_or_host(&candidate)
    }

    fn prepare_socket_connect(&self, requested: &Path) -> Result<PathBuf> {
        let logical = self.logical_socket_path(requested)?;
        if let Some(native) = self.native_passthrough_path(&logical)? {
            return Ok(native);
        }
        if let Some(remote) = &self.remote
            && remote.route_result(&logical)?.is_some()
        {
            return Err(io::Error::from_raw_os_error(libc::ENOTSUP).into());
        }
        let plan = self.filesystem.prepare_authorized_metadata(
            &logical,
            true,
            &Credentials::effective(),
        )?;
        let (resolved, mapped, _, _) = plan.into_parts();
        self.logical_or_host(&resolved)?;
        Ok(mapped)
    }

    fn bind_socket<T>(&self, requested: &Path, bind: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
        let logical = self.logical_socket_path(requested)?;
        if let Some(native) = self.native_passthrough_path(&logical)? {
            return bind(&native);
        }
        if let Some(remote) = &self.remote
            && remote.route_result(&logical)?.is_some()
        {
            return Err(io::Error::from_raw_os_error(libc::ENOTSUP).into());
        }
        self.filesystem
            .bind_socket_authorized(&logical, &Credentials::effective(), bind)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn agora_sandbox_bind(
    socket: libc::c_int,
    address: *const libc::sockaddr,
    length: libc::socklen_t,
) -> libc::c_int {
    catch_filesystem_panic(-1, || {
        let Some(original) = original_bind() else {
            unsafe { set_errno(libc::ENOSYS) };
            return -1;
        };
        let Some(path) = (unsafe { pathname_from_raw(address, length) }) else {
            return unsafe { original(socket, address, length) };
        };
        let Some(_guard) = FilesystemHookGuard::enter() else {
            return unsafe { original(socket, address, length) };
        };
        let Some(runtime) = FilesystemHookRuntime::global() else {
            return unsafe { original(socket, address, length) };
        };
        match runtime.bind_socket(&path, |mapped| {
            let bind = |mapped: &UnixSocketAddress| {
                native_operation_result(unsafe { original(socket, mapped.as_ptr(), mapped.len()) })
            };
            if runtime.filesystem.is_internal(mapped) {
                bind_overlay_socket(mapped, bind)
            } else {
                bind(&UnixSocketAddress::new(mapped)?)
            }
        }) {
            Ok(()) => 0,
            Err(error) => unsafe { fail(&error, -1) },
        }
    })
}

fn original_bind() -> Option<BindFn> {
    function_from_interpose(&INTERPOSE_BIND)
}

dyld_interpose!(INTERPOSE_BIND, agora_sandbox_bind, libc::bind);

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileTypeExt;

    #[test]
    fn pathname_addresses_round_trip_and_unnamed_addresses_delegate() {
        let path = Path::new("/tmp/agora-socket-address");
        let address = UnixSocketAddress::new(path).unwrap();

        assert_eq!(
            unsafe { pathname_from_raw(address.as_ptr(), address.len()) },
            Some(path.to_path_buf())
        );
        assert!(unsafe { is_pathname_address(address.as_ptr(), address.len()) });

        let mut unnamed = unsafe { MaybeUninit::<libc::sockaddr_un>::zeroed().assume_init() };
        unnamed.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let length = mem::offset_of!(libc::sockaddr_un, sun_path) + 1;
        assert_eq!(
            unsafe {
                pathname_from_raw(
                    std::ptr::addr_of!(unnamed).cast(),
                    length as libc::socklen_t,
                )
            },
            None
        );
    }

    #[test]
    fn pathname_addresses_reject_mapped_paths_that_exceed_darwin_sun_path() {
        let path = PathBuf::from(OsString::from_vec(vec![b'a'; 104]));

        let error = UnixSocketAddress::new(&path).err().unwrap();

        assert_eq!(super::super::error_errno(&error), libc::ENAMETOOLONG);
    }

    #[test]
    fn overlay_addresses_use_temporary_hard_links_for_long_backing_paths() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("x".repeat(96));
        std::fs::create_dir(&parent).unwrap();
        let mapped = parent.join("service.sock");
        assert!(UnixSocketAddress::new(&mapped).is_err());

        let mut bind_path = None;
        let listener = bind_overlay_socket(&mapped, |address| {
            bind_path = unsafe { pathname_from_raw(address.as_ptr(), address.len()) };
            std::os::unix::net::UnixListener::bind(bind_path.as_ref().unwrap()).map_err(Into::into)
        })
        .unwrap();

        assert!(mapped.symlink_metadata().unwrap().file_type().is_socket());
        assert!(!bind_path.unwrap().exists());

        let address = UnixSocketAddress::for_overlay_path(&mapped).unwrap();
        let connect_path = unsafe { pathname_from_raw(address.as_ptr(), address.len()) }.unwrap();
        let client = std::os::unix::net::UnixStream::connect(&connect_path).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        assert!(connect_path.exists());

        drop(accepted);
        drop(client);
        drop(address);
        assert!(!connect_path.exists());
        drop(listener);
        std::fs::remove_file(&mapped).unwrap();
    }
}
