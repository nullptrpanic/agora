#[repr(C)]
pub(super) struct DarwinFtsEntry {
    pub(super) fts_cycle: *mut DarwinFtsEntry,
    pub(super) fts_parent: *mut DarwinFtsEntry,
    pub(super) fts_link: *mut DarwinFtsEntry,
    pub(super) fts_number: libc::c_long,
    pub(super) fts_pointer: *mut libc::c_void,
    pub(super) fts_accpath: *mut libc::c_char,
    pub(super) fts_path: *mut libc::c_char,
    pub(super) fts_errno: libc::c_int,
    pub(super) fts_symfd: libc::c_int,
    pub(super) fts_pathlen: libc::c_ushort,
    pub(super) fts_namelen: libc::c_ushort,
    pub(super) fts_ino: libc::ino_t,
    pub(super) fts_dev: libc::dev_t,
    pub(super) fts_nlink: libc::nlink_t,
    pub(super) fts_level: libc::c_short,
    pub(super) fts_info: libc::c_ushort,
    pub(super) fts_flags: libc::c_ushort,
    pub(super) fts_instr: libc::c_ushort,
    pub(super) fts_statp: *mut libc::stat,
    pub(super) fts_name: [libc::c_char; 1],
}

pub(super) type FtsCompareFn =
    unsafe extern "C" fn(*const *const DarwinFtsEntry, *const *const DarwinFtsEntry) -> libc::c_int;

unsafe extern "C" {
    #[link_name = "mach_task_self_"]
    pub(super) static darwin_mach_task_self: libc::mach_port_t;

    #[link_name = "mach_vm_read_overwrite"]
    pub(super) fn darwin_mach_vm_read_overwrite(
        task: libc::mach_port_t,
        address: libc::mach_vm_address_t,
        size: libc::mach_vm_size_t,
        destination: libc::mach_vm_address_t,
        copied: *mut libc::mach_vm_size_t,
    ) -> libc::kern_return_t;

    #[link_name = "close"]
    pub(super) fn darwin_close(descriptor: libc::c_int) -> libc::c_int;

    #[link_name = "close$NOCANCEL"]
    pub(super) fn darwin_close_nocancel(descriptor: libc::c_int) -> libc::c_int;

    #[link_name = "dlopen_preflight"]
    pub(super) fn darwin_dlopen_preflight(path: *const libc::c_char) -> bool;

    #[cfg_attr(target_arch = "x86_64", link_name = "fts_children$INODE64")]
    #[cfg_attr(not(target_arch = "x86_64"), link_name = "fts_children")]
    pub(super) fn darwin_fts_children(
        stream: *mut libc::c_void,
        options: libc::c_int,
    ) -> *mut DarwinFtsEntry;

    #[cfg_attr(target_arch = "x86_64", link_name = "fts_open$INODE64")]
    #[cfg_attr(not(target_arch = "x86_64"), link_name = "fts_open")]
    pub(super) fn darwin_fts_open(
        paths: *const *mut libc::c_char,
        options: libc::c_int,
        compare: Option<FtsCompareFn>,
    ) -> *mut libc::c_void;

    #[cfg_attr(target_arch = "x86_64", link_name = "fts_close$INODE64")]
    #[cfg_attr(not(target_arch = "x86_64"), link_name = "fts_close")]
    pub(super) fn darwin_fts_close(stream: *mut libc::c_void) -> libc::c_int;

    #[cfg_attr(target_arch = "x86_64", link_name = "fts_read$INODE64")]
    #[cfg_attr(not(target_arch = "x86_64"), link_name = "fts_read")]
    pub(super) fn darwin_fts_read(stream: *mut libc::c_void) -> *mut DarwinFtsEntry;

    #[cfg_attr(target_arch = "x86_64", link_name = "fts_set$INODE64")]
    #[cfg_attr(not(target_arch = "x86_64"), link_name = "fts_set")]
    pub(super) fn darwin_fts_set(
        stream: *mut libc::c_void,
        entry: *mut DarwinFtsEntry,
        instruction: libc::c_int,
    ) -> libc::c_int;

    #[link_name = "getattrlistbulk"]
    pub(super) fn darwin_getattrlistbulk(
        directory: libc::c_int,
        attributes: *mut libc::c_void,
        buffer: *mut libc::c_void,
        size: libc::size_t,
        options: u64,
    ) -> libc::c_int;

    #[link_name = "removefile"]
    pub(super) fn darwin_removefile(
        path: *const libc::c_char,
        state: *mut libc::c_void,
        flags: libc::c_uint,
    ) -> libc::c_int;

    #[link_name = "removefileat"]
    pub(super) fn darwin_removefileat(
        directory: libc::c_int,
        path: *const libc::c_char,
        state: *mut libc::c_void,
        flags: libc::c_uint,
    ) -> libc::c_int;
}

pub(super) use libc::readdir_r as darwin_readdir_r;
