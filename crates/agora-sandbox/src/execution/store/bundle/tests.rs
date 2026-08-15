use super::prepare_dependencies;
use crate::execution::store::ExecutableStore;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const LC_LOAD_DYLIB: u32 = 0x0c;
const LC_LOAD_WEAK_DYLIB: u32 = 0x8000_0018;
const LC_RPATH: u32 = 0x8000_001c;

fn aligned(value: usize) -> usize {
    (value + 7) & !7
}

fn string_command(command: u32, header_size: usize, path: &[u8]) -> Vec<u8> {
    let size = aligned(header_size + path.len() + 1);
    let mut bytes = vec![0_u8; size];
    bytes[0..4].copy_from_slice(&command.to_le_bytes());
    bytes[4..8].copy_from_slice(&(size as u32).to_le_bytes());
    bytes[8..12].copy_from_slice(&(header_size as u32).to_le_bytes());
    bytes[header_size..header_size + path.len()].copy_from_slice(path);
    bytes
}

fn macho(commands: &[Vec<u8>]) -> Vec<u8> {
    let command_bytes = commands.iter().map(Vec::len).sum::<usize>();
    let mut image = vec![0_u8; 32];
    image[0..4].copy_from_slice(&0xfeed_facf_u32.to_le_bytes());
    image[4..8].copy_from_slice(&0x0100_0007_u32.to_le_bytes());
    image[16..20].copy_from_slice(&(commands.len() as u32).to_le_bytes());
    image[20..24].copy_from_slice(&(command_bytes as u32).to_le_bytes());
    for command in commands {
        image.extend_from_slice(command);
    }
    image
}

fn write_image(path: &Path, commands: &[Vec<u8>]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, macho(commands)).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn prepares_the_transitive_bundle_local_rpath_dependency_closure() {
    let root = tempfile::tempdir().unwrap();
    let bundle = root.path().join("Fixture.app");
    let contents = bundle.join("Contents");
    let executable = contents.join("MacOS/fixture");
    let first = contents.join("Frameworks/libfirst.dylib");
    let second = contents.join("Frameworks/libsecond.dylib");
    let third = contents.join("Frameworks/libthird.dylib");
    std::fs::create_dir_all(&contents).unwrap();
    std::fs::write(contents.join("Info.plist"), b"fixture").unwrap();
    write_image(
        &executable,
        &[
            string_command(LC_LOAD_DYLIB, 24, b"@rpath/libfirst.dylib"),
            string_command(LC_RPATH, 12, b"@executable_path/../Frameworks"),
        ],
    );
    write_image(
        &first,
        &[
            string_command(LC_LOAD_DYLIB, 24, b"@loader_path/libsecond.dylib"),
            string_command(LC_LOAD_DYLIB, 24, b"@rpath/libthird.dylib"),
        ],
    );
    write_image(
        &second,
        &[string_command(
            LC_LOAD_DYLIB,
            24,
            b"@loader_path/libfirst.dylib",
        )],
    );
    write_image(&third, &[]);
    let cache = root.path().join("workdir/fs");
    let store = ExecutableStore::new(cache.clone()).unwrap();

    prepare_dependencies(&store, &executable, "x86_64").unwrap();

    for dependency in [&first, &second, &third] {
        let prepared = cache.join(dependency.strip_prefix(Path::new("/")).unwrap());
        assert_eq!(
            std::fs::read(prepared).unwrap(),
            std::fs::read(dependency).unwrap()
        );
    }
    assert!(
        !cache
            .join(executable.strip_prefix(Path::new("/")).unwrap())
            .exists()
    );
}

#[test]
fn ignores_missing_weak_dependencies_but_rejects_missing_required_dependencies() {
    let root = tempfile::tempdir().unwrap();
    let contents = root.path().join("Fixture.app/Contents");
    let executable = contents.join("MacOS/fixture");
    std::fs::create_dir_all(&contents).unwrap();
    std::fs::write(contents.join("Info.plist"), b"fixture").unwrap();
    let rpath = string_command(LC_RPATH, 12, b"@executable_path/../Frameworks");
    let store = ExecutableStore::new(root.path().join("workdir/fs")).unwrap();

    write_image(
        &executable,
        &[
            string_command(LC_LOAD_WEAK_DYLIB, 24, b"@rpath/liboptional.dylib"),
            rpath.clone(),
        ],
    );
    prepare_dependencies(&store, &executable, "x86_64").unwrap();

    write_image(
        &executable,
        &[
            string_command(LC_LOAD_DYLIB, 24, b"@rpath/librequired.dylib"),
            rpath,
        ],
    );
    let error = prepare_dependencies(&store, &executable, "x86_64").unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unresolved application dependency")
    );
}

#[test]
fn rejects_tokenized_dependencies_that_escape_the_application_bundle() {
    let root = tempfile::tempdir().unwrap();
    let contents = root.path().join("Fixture.app/Contents");
    let executable = contents.join("MacOS/fixture");
    let outside = root.path().join("outside.dylib");
    std::fs::create_dir_all(&contents).unwrap();
    std::fs::write(contents.join("Info.plist"), b"fixture").unwrap();
    write_image(&outside, &[]);
    write_image(
        &executable,
        &[string_command(
            LC_LOAD_DYLIB,
            24,
            b"@executable_path/../../../outside.dylib",
        )],
    );
    let store = ExecutableStore::new(root.path().join("workdir/fs")).unwrap();

    let error = prepare_dependencies(&store, &executable, "x86_64").unwrap_err();

    assert!(
        error
            .to_string()
            .contains("application dependency escapes bundle")
    );
}
