use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStringExt;
use std::path::Path;

const MACH_64_MAGIC: u32 = 0xfeed_facf;
const FAT_MAGIC: u32 = 0xcafe_babe;
const FAT_MAGIC_64: u32 = 0xcafe_babf;
const CPU_TYPE_X86_64: u32 = 0x0100_0007;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const LC_LOAD_DYLIB: u32 = 0x0c;
const LC_LOAD_WEAK_DYLIB: u32 = 0x8000_0018;
const LC_REEXPORT_DYLIB: u32 = 0x8000_001f;
const LC_LAZY_LOAD_DYLIB: u32 = 0x20;
const LC_LOAD_UPWARD_DYLIB: u32 = 0x8000_0023;
const LC_RPATH: u32 = 0x8000_001c;
const MAX_LOAD_COMMANDS: usize = 4096;
const MAX_LOAD_COMMAND_BYTES: usize = 16 * 1024 * 1024;
const MAX_FAT_SLICES: usize = 64;

#[derive(Debug, Eq, PartialEq)]
pub(super) struct MachODependency {
    pub(super) path: OsString,
    pub(super) weak: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct MachOImage {
    pub(super) dependencies: Vec<MachODependency>,
    pub(super) rpaths: Vec<OsString>,
}

impl MachOImage {
    pub(super) fn read(path: &Path, architecture: &str) -> Result<Self> {
        let expected_cpu = match architecture {
            "x86_64" => CPU_TYPE_X86_64,
            "arm64" | "arm64e" => CPU_TYPE_ARM64,
            _ => bail!("unsupported Mach-O architecture {architecture}"),
        };
        let mut file = File::open(path)
            .with_context(|| format!("failed to open Mach-O image {}", path.display()))?;
        let file_size = file.metadata()?.len();
        let mut prefix = [0_u8; 8];
        file.read_exact(&mut prefix)
            .with_context(|| format!("failed to read Mach-O header {}", path.display()))?;
        let magic = u32::from_be_bytes(prefix[0..4].try_into().unwrap());
        let (slice_offset, slice_size) = if matches!(magic, FAT_MAGIC | FAT_MAGIC_64) {
            let count = u32::from_be_bytes(prefix[4..8].try_into().unwrap()) as usize;
            if count == 0 || count > MAX_FAT_SLICES {
                bail!("invalid Mach-O fat slice count in {}", path.display());
            }
            let entry_size = if magic == FAT_MAGIC_64 { 32 } else { 20 };
            let mut entries = vec![0_u8; count * entry_size];
            file.read_exact(&mut entries)
                .with_context(|| format!("failed to read Mach-O fat header {}", path.display()))?;
            let mut selected = None;
            for entry in entries.chunks_exact(entry_size) {
                if u32::from_be_bytes(entry[0..4].try_into().unwrap()) != expected_cpu {
                    continue;
                }
                let (offset, size) = if magic == FAT_MAGIC_64 {
                    (
                        u64::from_be_bytes(entry[8..16].try_into().unwrap()),
                        u64::from_be_bytes(entry[16..24].try_into().unwrap()),
                    )
                } else {
                    (
                        u32::from_be_bytes(entry[8..12].try_into().unwrap()) as u64,
                        u32::from_be_bytes(entry[12..16].try_into().unwrap()) as u64,
                    )
                };
                if size < 32 || offset.checked_add(size).is_none_or(|end| end > file_size) {
                    bail!("invalid Mach-O fat slice bounds in {}", path.display());
                }
                selected = Some((offset, size));
                break;
            }
            selected.with_context(|| {
                format!(
                    "Mach-O image {} has no {architecture} slice",
                    path.display()
                )
            })?
        } else {
            (0, file_size)
        };
        let mut header = [0_u8; 32];
        file.seek(SeekFrom::Start(slice_offset))?;
        file.read_exact(&mut header)
            .with_context(|| format!("failed to read Mach-O header {}", path.display()))?;
        if u32::from_le_bytes(header[0..4].try_into().unwrap()) != MACH_64_MAGIC {
            bail!("unsupported Mach-O image {}", path.display());
        }
        if u32::from_le_bytes(header[4..8].try_into().unwrap()) != expected_cpu {
            bail!(
                "Mach-O image {} has no {architecture} slice",
                path.display()
            );
        }
        let command_count = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
        let command_bytes = u32::from_le_bytes(header[20..24].try_into().unwrap()) as usize;
        if command_count > MAX_LOAD_COMMANDS || command_bytes > MAX_LOAD_COMMAND_BYTES {
            bail!("Mach-O load commands exceed limits in {}", path.display());
        }
        if 32_u64
            .checked_add(command_bytes as u64)
            .is_none_or(|size| size > slice_size)
        {
            bail!("Mach-O load commands exceed slice in {}", path.display());
        }
        let mut commands = vec![0_u8; command_bytes];
        file.seek(SeekFrom::Start(slice_offset + 32))?;
        file.read_exact(&mut commands)
            .with_context(|| format!("failed to read Mach-O commands {}", path.display()))?;
        Self::parse_commands(path, command_count, &commands)
    }

    fn parse_commands(path: &Path, command_count: usize, commands: &[u8]) -> Result<Self> {
        let mut dependencies = Vec::new();
        let mut rpaths = Vec::new();
        let mut offset = 0_usize;
        for _ in 0..command_count {
            let header = commands
                .get(offset..offset.saturating_add(8))
                .with_context(|| format!("truncated Mach-O command in {}", path.display()))?;
            let command = u32::from_le_bytes(header[0..4].try_into().unwrap());
            let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
            if size < 8 || offset.saturating_add(size) > commands.len() {
                bail!("invalid Mach-O command size in {}", path.display());
            }
            let bytes = &commands[offset..offset + size];
            if matches!(
                command,
                LC_LOAD_DYLIB
                    | LC_LOAD_WEAK_DYLIB
                    | LC_REEXPORT_DYLIB
                    | LC_LAZY_LOAD_DYLIB
                    | LC_LOAD_UPWARD_DYLIB
            ) {
                dependencies.push(MachODependency {
                    path: Self::command_string(path, bytes, 24)?,
                    weak: command == LC_LOAD_WEAK_DYLIB,
                });
            } else if command == LC_RPATH {
                rpaths.push(Self::command_string(path, bytes, 12)?);
            }
            offset += size;
        }
        if offset != commands.len() {
            bail!("Mach-O command size mismatch in {}", path.display());
        }
        Ok(Self {
            dependencies,
            rpaths,
        })
    }

    fn command_string(path: &Path, command: &[u8], header_size: usize) -> Result<OsString> {
        let raw_offset = command
            .get(8..12)
            .with_context(|| format!("missing Mach-O string offset in {}", path.display()))?;
        let offset = u32::from_le_bytes(raw_offset.try_into().unwrap()) as usize;
        if offset < header_size {
            bail!("invalid Mach-O string offset in {}", path.display());
        }
        let value = command
            .get(offset..)
            .with_context(|| format!("invalid Mach-O string offset in {}", path.display()))?;
        let end = value
            .iter()
            .position(|byte| *byte == 0)
            .with_context(|| format!("unterminated Mach-O command string in {}", path.display()))?;
        if end == 0 {
            bail!("empty Mach-O command string in {}", path.display());
        }
        Ok(OsString::from_vec(value[..end].to_vec()))
    }
}

#[cfg(test)]
mod tests;
