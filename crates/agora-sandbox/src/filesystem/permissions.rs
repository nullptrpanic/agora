use super::FileAttributes;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AccessRequest {
    pub(crate) read: bool,
    pub(crate) write: bool,
    pub(crate) execute: bool,
}

impl AccessRequest {
    pub(crate) const READ: Self = Self::new(true, false, false);
    pub(crate) const WRITE: Self = Self::new(false, true, false);
    pub(crate) const EXECUTE: Self = Self::new(false, false, true);
    pub(crate) const READ_WRITE: Self = Self::new(true, true, false);
    pub(crate) const WRITE_EXECUTE: Self = Self::new(false, true, true);

    const fn new(read: bool, write: bool, execute: bool) -> Self {
        Self {
            read,
            write,
            execute,
        }
    }

    pub(crate) fn from_open_flags(flags: libc::c_int) -> Self {
        let mut request = match flags & libc::O_ACCMODE {
            libc::O_WRONLY => Self::WRITE,
            libc::O_RDWR => Self::READ_WRITE,
            _ => Self::READ,
        };
        if flags & libc::O_TRUNC != 0 {
            request.write = true;
        }
        request
    }

    pub(crate) fn from_access_mode(mode: libc::c_int) -> std::io::Result<Self> {
        if mode & !(libc::R_OK | libc::W_OK | libc::X_OK) != 0 {
            return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
        }
        Ok(Self::new(
            mode & libc::R_OK != 0,
            mode & libc::W_OK != 0,
            mode & libc::X_OK != 0,
        ))
    }
}

pub(crate) struct Credentials {
    uid: libc::uid_t,
    gid: libc::gid_t,
    groups: Vec<libc::gid_t>,
}

impl Credentials {
    pub(crate) fn real() -> Self {
        Self::current(unsafe { libc::getuid() }, unsafe { libc::getgid() })
    }

    pub(crate) fn effective() -> Self {
        Self::current(unsafe { libc::geteuid() }, unsafe { libc::getegid() })
    }

    fn current(uid: libc::uid_t, gid: libc::gid_t) -> Self {
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        let mut groups = if count > 0 {
            vec![0; count as usize]
        } else {
            Vec::new()
        };
        if count > 0 {
            let actual = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
            if actual >= 0 {
                groups.truncate(actual as usize);
            } else {
                groups.clear();
            }
        }
        Self { uid, gid, groups }
    }

    #[cfg(test)]
    pub(crate) fn for_test(uid: libc::uid_t, gid: libc::gid_t, groups: &[libc::gid_t]) -> Self {
        Self {
            uid,
            gid,
            groups: groups.to_vec(),
        }
    }

    pub(crate) fn allows(&self, attributes: &FileAttributes, request: AccessRequest) -> bool {
        if request == AccessRequest::default() {
            return true;
        }
        if self.uid == 0 {
            return !request.execute || attributes.mode & 0o111 != 0;
        }
        let shift = if self.uid == attributes.uid {
            6
        } else if self.gid == attributes.gid || self.groups.contains(&attributes.gid) {
            3
        } else {
            0
        };
        let allowed = (attributes.mode >> shift) & 0o7;
        (!request.read || allowed & 0o4 != 0)
            && (!request.write || allowed & 0o2 != 0)
            && (!request.execute || allowed & 0o1 != 0)
    }

    pub(crate) fn can_chmod(&self, attributes: &FileAttributes) -> bool {
        self.uid == 0 || self.uid == attributes.uid
    }
}

#[cfg(test)]
mod tests {
    use super::{AccessRequest, Credentials};
    use crate::filesystem::FileAttributes;

    fn file_attributes(mode: u32, uid: u32, gid: u32) -> FileAttributes {
        FileAttributes {
            mode,
            uid,
            gid,
            atime: 0,
            atime_nsec: 0,
            mtime: 0,
            mtime_nsec: 0,
        }
    }

    #[test]
    fn credentials_apply_owner_group_other_and_root_rules() {
        let attributes = file_attributes(u32::from(libc::S_IFREG) | 0o640, 501, 20);

        let owner = Credentials::for_test(501, 99, &[]);
        assert!(owner.allows(&attributes, AccessRequest::READ_WRITE));

        let primary_group = Credentials::for_test(502, 20, &[]);
        assert!(primary_group.allows(&attributes, AccessRequest::READ));
        assert!(!primary_group.allows(&attributes, AccessRequest::WRITE));

        let supplementary_group = Credentials::for_test(502, 99, &[20]);
        assert!(supplementary_group.allows(&attributes, AccessRequest::READ));
        assert!(!supplementary_group.allows(&attributes, AccessRequest::WRITE));

        let other = Credentials::for_test(502, 99, &[]);
        assert!(!other.allows(&attributes, AccessRequest::READ));

        let root = Credentials::for_test(0, 0, &[]);
        assert!(root.allows(&attributes, AccessRequest::READ_WRITE));
        assert!(!root.allows(&attributes, AccessRequest::EXECUTE));
        assert!(root.allows(
            &file_attributes(u32::from(libc::S_IFREG) | 0o001, 501, 20),
            AccessRequest::EXECUTE,
        ));
    }

    #[test]
    fn access_requests_derive_from_open_and_access_modes() {
        assert_eq!(
            AccessRequest::from_open_flags(libc::O_RDONLY),
            AccessRequest::READ,
        );
        assert_eq!(
            AccessRequest::from_open_flags(libc::O_WRONLY),
            AccessRequest::WRITE,
        );
        assert_eq!(
            AccessRequest::from_open_flags(libc::O_RDWR),
            AccessRequest::READ_WRITE,
        );
        assert_eq!(
            AccessRequest::from_open_flags(libc::O_RDONLY | libc::O_TRUNC),
            AccessRequest::READ_WRITE,
        );
        assert_eq!(
            AccessRequest::from_access_mode(libc::F_OK).unwrap(),
            AccessRequest::default(),
        );
        assert_eq!(
            AccessRequest::from_access_mode(libc::R_OK | libc::X_OK).unwrap(),
            AccessRequest {
                read: true,
                write: false,
                execute: true,
            },
        );
    }

    #[test]
    fn access_requests_reject_unknown_mode_bits() {
        let error = AccessRequest::from_access_mode(libc::R_OK | 0x100).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn chmod_requires_owner_or_root() {
        let attributes = file_attributes(u32::from(libc::S_IFREG) | 0o644, 501, 20);
        assert!(Credentials::for_test(501, 99, &[]).can_chmod(&attributes));
        assert!(Credentials::for_test(0, 0, &[]).can_chmod(&attributes));
        assert!(!Credentials::for_test(502, 20, &[]).can_chmod(&attributes));
    }
}
