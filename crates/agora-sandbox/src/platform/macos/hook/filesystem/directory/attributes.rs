//! Darwin bulk records use the requested bitmap order and four-byte alignment,
//! not a fixed FTS struct. Unsupported fields are omitted (or zero-filled for
//! PACK_INVAL_ATTRS) and never advertised in the returned bitmap.
use super::super::*;

pub(super) const ATTR_CMN_ERROR: u32 = 0x2000_0000;
pub(super) struct Metadata {
    pub(super) status: libc::stat,
    pub(super) filesystem_id: [u8; 8],
}
const REQUIRED: u32 = libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME;
// ERROR precedes NAME even though its bit number is greater.
const COMMON: &[(u32, usize)] = &[
    (ATTR_CMN_ERROR, 4),
    (libc::ATTR_CMN_NAME, 8),
    (libc::ATTR_CMN_DEVID, 4),
    (libc::ATTR_CMN_FSID, 8),
    (libc::ATTR_CMN_OBJTYPE, 4),
    (libc::ATTR_CMN_OBJTAG, 4),
    (libc::ATTR_CMN_OBJID, 8),
    (libc::ATTR_CMN_OBJPERMANENTID, 8),
    (libc::ATTR_CMN_PAROBJID, 8),
    (libc::ATTR_CMN_SCRIPT, 4),
    (libc::ATTR_CMN_CRTIME, 16),
    (libc::ATTR_CMN_MODTIME, 16),
    (libc::ATTR_CMN_CHGTIME, 16),
    (libc::ATTR_CMN_ACCTIME, 16),
    (libc::ATTR_CMN_BKUPTIME, 16),
    (libc::ATTR_CMN_FNDRINFO, 32),
    (libc::ATTR_CMN_OWNERID, 4),
    (libc::ATTR_CMN_GRPID, 4),
    (libc::ATTR_CMN_ACCESSMASK, 4),
    (libc::ATTR_CMN_FLAGS, 4),
    (libc::ATTR_CMN_GEN_COUNT, 4),
    (libc::ATTR_CMN_DOCUMENT_ID, 4),
    (libc::ATTR_CMN_USERACCESS, 4),
    (libc::ATTR_CMN_EXTENDED_SECURITY, 8),
    (libc::ATTR_CMN_UUID, 16),
    (libc::ATTR_CMN_GRPUUID, 16),
    (libc::ATTR_CMN_FILEID, 8),
    (libc::ATTR_CMN_PARENTID, 8),
    (libc::ATTR_CMN_FULLPATH, 8),
    (libc::ATTR_CMN_ADDEDTIME, 16),
    (libc::ATTR_CMN_DATA_PROTECT_FLAGS, 4),
];
const DIRECTORY: &[(u32, usize)] = &[
    (libc::ATTR_DIR_LINKCOUNT, 4),
    (libc::ATTR_DIR_ENTRYCOUNT, 4),
    (libc::ATTR_DIR_MOUNTSTATUS, 4),
    (libc::ATTR_DIR_ALLOCSIZE, 8),
    (libc::ATTR_DIR_IOBLOCKSIZE, 4),
    (libc::ATTR_DIR_DATALENGTH, 8),
];
const FILE: &[(u32, usize)] = &[
    (libc::ATTR_FILE_LINKCOUNT, 4),
    (libc::ATTR_FILE_TOTALSIZE, 8),
    (libc::ATTR_FILE_ALLOCSIZE, 8),
    (libc::ATTR_FILE_IOBLOCKSIZE, 4),
    (libc::ATTR_FILE_DEVTYPE, 4),
    (libc::ATTR_FILE_FORKCOUNT, 4),
    (libc::ATTR_FILE_FORKLIST, 8),
    (libc::ATTR_FILE_DATALENGTH, 8),
    (libc::ATTR_FILE_DATAALLOCSIZE, 8),
    (libc::ATTR_FILE_RSRCLENGTH, 8),
    (libc::ATTR_FILE_RSRCALLOCSIZE, 8),
];
const EXTENDED: &[(u32, usize)] = &[
    (libc::ATTR_CMNEXT_RELPATH, 8),
    (libc::ATTR_CMNEXT_PRIVATESIZE, 8),
    (libc::ATTR_CMNEXT_LINKID, 8),
    (libc::ATTR_CMNEXT_NOFIRMLINKPATH, 8),
    (libc::ATTR_CMNEXT_REALDEVID, 4),
    (libc::ATTR_CMNEXT_REALFSID, 8),
    (libc::ATTR_CMNEXT_CLONEID, 8),
    (libc::ATTR_CMNEXT_EXT_FLAGS, 8),
    (libc::ATTR_CMNEXT_RECURSIVE_GENCOUNT, 8),
];

pub(super) fn supported(attributes: &libc::attrlist, options: u64) -> bool {
    let known = |fields: &[(u32, usize)]| fields.iter().fold(0, |bits, (bit, _)| bits | bit);
    let options_mask = libc::FSOPT_NOFOLLOW
        | libc::FSOPT_PACK_INVAL_ATTRS
        | libc::FSOPT_ATTR_CMN_EXTENDED
        | libc::FSOPT_RETURN_REALDEV;
    attributes.bitmapcount == libc::ATTR_BIT_MAP_COUNT
        && attributes.commonattr & REQUIRED == REQUIRED
        && attributes.volattr == 0
        && attributes.dirattr & !known(DIRECTORY) == 0
        && attributes.fileattr & !known(FILE) == 0
        && attributes.forkattr & !known(EXTENDED) == 0
        && (attributes.forkattr == 0 || options & u64::from(libc::FSOPT_ATTR_CMN_EXTENDED) != 0)
        && options & !u64::from(options_mask) == 0
        && (attributes.commonattr & (libc::ATTR_CMN_GEN_COUNT | libc::ATTR_CMN_DOCUMENT_ID) == 0
            || options & u64::from(libc::FSOPT_ATTR_CMN_EXTENDED) != 0)
}

pub(super) fn needs_stat(attributes: &libc::attrlist) -> bool {
    attributes.commonattr
        & !(REQUIRED | libc::ATTR_CMN_OBJTYPE | ATTR_CMN_ERROR | libc::ATTR_CMN_FULLPATH)
        != 0
        || attributes.dirattr != 0
        || attributes.fileattr != 0
        || attributes.forkattr != 0
}

pub(super) fn record(
    name: &[u8],
    logical: &Path,
    object_type: u32,
    metadata: Option<&Metadata>,
    errno: i32,
    attributes: &libc::attrlist,
    options: u64,
) -> Result<Vec<u8>> {
    let requested = [
        attributes.commonattr,
        0,
        attributes.dirattr,
        attributes.fileattr,
        attributes.forkattr,
    ];
    let mut returned = [libc::ATTR_CMN_RETURNED_ATTRS, 0, 0, 0, 0];
    let mut bytes = vec![0_u8; 24];
    let mut strings = Vec::new();
    let pack_invalid = options & u64::from(libc::FSOPT_PACK_INVAL_ATTRS) != 0;
    for (group, fields) in [(0, COMMON), (2, DIRECTORY), (3, FILE), (4, EXTENDED)] {
        for &(bit, width) in fields {
            if requested[group] & bit == 0 {
                continue;
            }
            let mut field = [0_u8; 32];
            let mut string = None;
            let valid = match (group, bit) {
                (0, ATTR_CMN_ERROR) => {
                    field[..4].copy_from_slice(&errno.to_ne_bytes());
                    true
                }
                (0, libc::ATTR_CMN_NAME) => {
                    string = Some(name);
                    true
                }
                (0, libc::ATTR_CMN_FULLPATH) => {
                    string = Some(logical.as_os_str().as_bytes());
                    true
                }
                (0, libc::ATTR_CMN_OBJTYPE) => {
                    field[..4].copy_from_slice(&object_type.to_ne_bytes());
                    true
                }
                (0, libc::ATTR_CMN_FSID) | (4, libc::ATTR_CMNEXT_REALFSID) => {
                    metadata.is_some_and(|metadata| {
                        field[..8].copy_from_slice(&metadata.filesystem_id);
                        true
                    })
                }
                _ => metadata
                    .is_some_and(|metadata| stat_field(group, bit, &metadata.status, &mut field)),
            };
            if valid {
                returned[group] |= bit;
                if let Some(string) = string {
                    strings.push((bytes.len(), string));
                }
            }
            if valid || pack_invalid {
                bytes.extend_from_slice(&field[..width]);
            }
        }
    }
    for (reference, string) in strings {
        let delta = i32::try_from(bytes.len() - reference)?;
        let length = u32::try_from(string.len() + 1)?;
        bytes[reference..reference + 4].copy_from_slice(&delta.to_ne_bytes());
        bytes[reference + 4..reference + 8].copy_from_slice(&length.to_ne_bytes());
        bytes.extend_from_slice(string);
        bytes.push(0);
        bytes.resize((bytes.len() + 3) & !3, 0);
    }
    let length = u32::try_from(bytes.len())?;
    bytes[..4].copy_from_slice(&length.to_ne_bytes());
    for (index, mask) in returned.into_iter().enumerate() {
        bytes[4 + index * 4..8 + index * 4].copy_from_slice(&mask.to_ne_bytes());
    }
    Ok(bytes)
}

fn stat_field(group: usize, bit: u32, status: &libc::stat, field: &mut [u8; 32]) -> bool {
    let directory = status.st_mode & libc::S_IFMT == libc::S_IFDIR;
    if group == 2 && !directory || group == 3 && directory {
        return false;
    }
    let time = match (group, bit) {
        (0, libc::ATTR_CMN_CRTIME) => Some((status.st_birthtime, status.st_birthtime_nsec)),
        (0, libc::ATTR_CMN_MODTIME) => Some((status.st_mtime, status.st_mtime_nsec)),
        (0, libc::ATTR_CMN_CHGTIME) => Some((status.st_ctime, status.st_ctime_nsec)),
        (0, libc::ATTR_CMN_ACCTIME) => Some((status.st_atime, status.st_atime_nsec)),
        _ => None,
    };
    if let Some((seconds, nanos)) = time {
        field[..8].copy_from_slice(&seconds.to_ne_bytes());
        field[8..16].copy_from_slice(&nanos.to_ne_bytes());
        return true;
    }
    let value = match (group, bit) {
        (0, libc::ATTR_CMN_DEVID) | (4, libc::ATTR_CMNEXT_REALDEVID) => status.st_dev as u64,
        (0, libc::ATTR_CMN_FILEID) | (4, libc::ATTR_CMNEXT_LINKID) => status.st_ino,
        (0, libc::ATTR_CMN_OWNERID) => u64::from(status.st_uid),
        (0, libc::ATTR_CMN_GRPID) => u64::from(status.st_gid),
        (0, libc::ATTR_CMN_ACCESSMASK) => u64::from(status.st_mode),
        (0, libc::ATTR_CMN_FLAGS) => u64::from(status.st_flags),
        (2, libc::ATTR_DIR_LINKCOUNT) | (3, libc::ATTR_FILE_LINKCOUNT) => {
            u64::from(status.st_nlink)
        }
        (3, libc::ATTR_FILE_DATALENGTH) => status.st_size as u64,
        (2, libc::ATTR_DIR_IOBLOCKSIZE) | (3, libc::ATTR_FILE_IOBLOCKSIZE) => {
            status.st_blksize as u64
        }
        (3, libc::ATTR_FILE_DEVTYPE) => status.st_rdev as u64,
        // Allocation, resource forks, ACLs and filesystem-specific IDs are not
        // logical stat properties. Do not invent values from ciphertext/anchors.
        _ => return false,
    };
    field[..8].copy_from_slice(&value.to_ne_bytes());
    true
}
