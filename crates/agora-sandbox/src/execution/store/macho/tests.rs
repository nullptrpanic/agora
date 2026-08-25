use super::{MachODependency, MachOImage};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStringExt;

const LC_LOAD_DYLIB: u32 = 0x0c;
const LC_LOAD_WEAK_DYLIB: u32 = 0x8000_0018;
const LC_RPATH: u32 = 0x8000_001c;

fn aligned(value: usize) -> usize {
    (value + 7) & !7
}

fn dylib_command(command: u32, path: &[u8]) -> Vec<u8> {
    let size = aligned(24 + path.len() + 1);
    let mut bytes = vec![0_u8; size];
    bytes[0..4].copy_from_slice(&command.to_le_bytes());
    bytes[4..8].copy_from_slice(&(size as u32).to_le_bytes());
    bytes[8..12].copy_from_slice(&24_u32.to_le_bytes());
    bytes[24..24 + path.len()].copy_from_slice(path);
    bytes
}

fn rpath_command(path: &[u8]) -> Vec<u8> {
    let size = aligned(12 + path.len() + 1);
    let mut bytes = vec![0_u8; size];
    bytes[0..4].copy_from_slice(&LC_RPATH.to_le_bytes());
    bytes[4..8].copy_from_slice(&(size as u32).to_le_bytes());
    bytes[8..12].copy_from_slice(&12_u32.to_le_bytes());
    bytes[12..12 + path.len()].copy_from_slice(path);
    bytes
}

fn thin_image(cpu_type: u32, commands: &[Vec<u8>]) -> Vec<u8> {
    let command_bytes = commands.iter().map(Vec::len).sum::<usize>();
    let mut image = vec![0_u8; 32];
    image[0..4].copy_from_slice(&0xfeed_facf_u32.to_le_bytes());
    image[4..8].copy_from_slice(&cpu_type.to_le_bytes());
    image[16..20].copy_from_slice(&(commands.len() as u32).to_le_bytes());
    image[20..24].copy_from_slice(&(command_bytes as u32).to_le_bytes());
    for command in commands {
        image.extend_from_slice(command);
    }
    image
}

fn fat_image(slices: &[(u32, Vec<u8>)], fat64: bool) -> Vec<u8> {
    let entry_size = if fat64 { 32 } else { 20 };
    let header_size = 8 + slices.len() * entry_size;
    let mut offsets = Vec::with_capacity(slices.len());
    let mut cursor = aligned(header_size);
    for (_, slice) in slices {
        offsets.push(cursor);
        cursor = aligned(cursor + slice.len());
    }
    let mut bytes = vec![0_u8; cursor];
    bytes[0..4].copy_from_slice(
        &(if fat64 {
            0xcafe_babf_u32
        } else {
            0xcafe_babe_u32
        })
        .to_be_bytes(),
    );
    bytes[4..8].copy_from_slice(&(slices.len() as u32).to_be_bytes());
    for (index, ((cpu_type, slice), offset)) in slices.iter().zip(offsets).enumerate() {
        let entry = 8 + index * entry_size;
        bytes[entry..entry + 4].copy_from_slice(&cpu_type.to_be_bytes());
        if fat64 {
            bytes[entry + 8..entry + 16].copy_from_slice(&(offset as u64).to_be_bytes());
            bytes[entry + 16..entry + 24].copy_from_slice(&(slice.len() as u64).to_be_bytes());
        } else {
            bytes[entry + 8..entry + 12].copy_from_slice(&(offset as u32).to_be_bytes());
            bytes[entry + 12..entry + 16].copy_from_slice(&(slice.len() as u32).to_be_bytes());
        }
        bytes[offset..offset + slice.len()].copy_from_slice(slice);
    }
    bytes
}

#[test]
fn reads_dependencies_and_rpaths_from_a_thin_macho_image() {
    let required = b"@rpath/libfixture.dylib";
    let weak = b"@loader_path/liboptional\xff.dylib";
    let bytes = thin_image(
        0x0100_0007,
        &[
            dylib_command(LC_LOAD_DYLIB, required),
            dylib_command(LC_LOAD_WEAK_DYLIB, weak),
            rpath_command(b"@executable_path/../Frameworks"),
        ],
    );
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), bytes).unwrap();

    let image = MachOImage::read(file.path(), "x86_64").unwrap();

    assert_eq!(
        image.dependencies,
        vec![
            MachODependency {
                path: OsString::from_vec(required.to_vec()),
                weak: false,
            },
            MachODependency {
                path: OsString::from_vec(weak.to_vec()),
                weak: true,
            },
        ]
    );
    assert_eq!(image.rpaths, [OsStr::new("@executable_path/../Frameworks")]);
}

#[test]
fn reads_only_the_requested_slice_from_fat_macho_images() {
    let x86 = thin_image(
        0x0100_0007,
        &[dylib_command(LC_LOAD_DYLIB, b"@rpath/libx86.dylib")],
    );
    let arm = thin_image(
        0x0100_000c,
        &[dylib_command(LC_LOAD_DYLIB, b"@rpath/libarm.dylib")],
    );
    for fat64 in [false, true] {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            fat_image(
                &[(0x0100_000c, arm.clone()), (0x0100_0007, x86.clone())],
                fat64,
            ),
        )
        .unwrap();

        assert_eq!(
            MachOImage::read(file.path(), "x86_64")
                .unwrap()
                .dependencies[0]
                .path,
            OsStr::new("@rpath/libx86.dylib")
        );
        assert_eq!(
            MachOImage::read(file.path(), "arm64").unwrap().dependencies[0].path,
            OsStr::new("@rpath/libarm.dylib")
        );
    }
}

#[test]
fn rejects_load_commands_outside_the_selected_fat_slice() {
    let x86 = thin_image(
        0x0100_0007,
        &[dylib_command(LC_LOAD_DYLIB, b"@rpath/libfixture.dylib")],
    );
    let mut bytes = fat_image(&[(0x0100_0007, x86)], false);
    bytes[20..24].copy_from_slice(&32_u32.to_be_bytes());
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), bytes).unwrap();

    let error = MachOImage::read(file.path(), "x86_64").unwrap_err();

    assert!(error.to_string().contains("load commands exceed slice"));
}

#[test]
fn rejects_strings_that_overlap_their_load_command_header() {
    let mut command = dylib_command(LC_LOAD_DYLIB, b"@rpath/libfixture.dylib");
    command[8..12].copy_from_slice(&8_u32.to_le_bytes());
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), thin_image(0x0100_0007, &[command])).unwrap();

    let error = MachOImage::read(file.path(), "x86_64").unwrap_err();

    assert!(error.to_string().contains("invalid Mach-O string offset"));
}
