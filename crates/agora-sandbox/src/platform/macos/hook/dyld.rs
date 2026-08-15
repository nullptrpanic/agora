use std::mem;

#[repr(C)]
pub(super) struct DyldInterpose {
    pub(super) replacement: *const libc::c_void,
    pub(super) replacee: *const libc::c_void,
}

unsafe impl Sync for DyldInterpose {}

pub(super) fn function_from_interpose<T>(interpose: &DyldInterpose) -> Option<T>
where
    T: Copy,
{
    if interpose.replacee.is_null() {
        None
    } else {
        Some(unsafe { mem::transmute_copy(&interpose.replacee) })
    }
}

macro_rules! dyld_interpose {
    ($name:ident, $replacement:path, $replacee:path) => {
        #[used]
        #[unsafe(link_section = "__DATA,__interpose")]
        static $name: $crate::platform::hook::dyld::DyldInterpose =
            $crate::platform::hook::dyld::DyldInterpose {
                replacement: $replacement as *const () as *const libc::c_void,
                replacee: $replacee as *const () as *const libc::c_void,
            };
    };
}

pub(super) use dyld_interpose;
