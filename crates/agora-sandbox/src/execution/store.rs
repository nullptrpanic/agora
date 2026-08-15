use crate::filesystem::{EntryState, FileCipher, Materializer, OverlayStore};
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::macos::fs::MetadataExt as MacMetadataExt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

#[cfg(test)]
thread_local! {
    static ARCHITECTURE_INSPECTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

use super::DEFAULT_EXECUTABLE_PATH;

const MACH_64_MAGIC: u32 = 0xfeed_facf;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const CPU_SUBTYPE_ARM64E: u32 = 2;
const SF_RESTRICTED: u32 = 0x0008_0000;
const CSR_ALLOW_UNRESTRICTED_FS: libc::c_uint = 1 << 1;
const CS_RESTRICT: u32 = 0x0000_0800;
const CS_REQUIRE_LV: u32 = 0x0000_2000;
const CS_RUNTIME: u32 = 0x0001_0000;
const CS_DYLD_RESTRICTED: u32 = CS_RESTRICT | CS_REQUIRE_LV | CS_RUNTIME;
const MAX_SHEBANG_LINE_SIZE: usize = 1024;
const SIGNATURE_CACHE_CAPACITY: usize = 1024;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct ExecutableIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    flags: u32,
}

impl ExecutableIdentity {
    fn new(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            flags: metadata.st_flags(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ArchitectureSelection {
    slice: String,
    rewrite_arm64e: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Shebang {
    pub(crate) interpreter: PathBuf,
    pub(crate) argument: Option<OsString>,
}

pub(super) struct ExecutableStore {
    overlay: OverlayStore,
}

impl ExecutableStore {
    pub(super) fn new(directory: PathBuf) -> Result<Self> {
        Self::with_cipher(directory, None)
    }

    pub(super) fn encrypted(directory: PathBuf, cipher: FileCipher) -> Result<Self> {
        Self::with_cipher(directory, Some(cipher))
    }

    fn with_cipher(directory: PathBuf, cipher: Option<FileCipher>) -> Result<Self> {
        fs::create_dir_all(&directory).with_context(|| {
            format!(
                "failed to create sandbox executable directory {}",
                directory.display()
            )
        })?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!(
                "failed to secure sandbox executable directory {}",
                directory.display()
            )
        })?;
        let overlay = match cipher {
            Some(cipher) => OverlayStore::encrypted(directory.clone(), cipher)?,
            None => OverlayStore::new(directory.clone())?,
        };
        Ok(Self { overlay })
    }

    pub(super) fn prepare(&self, source: &Path) -> Result<PathBuf> {
        if !self.overlay.is_internal(source) && self.overlay.is_private(source)? {
            return Err(io::Error::from_raw_os_error(libc::EACCES)).with_context(|| {
                format!(
                    "sandbox executable is inside the private work directory: {}",
                    source.display()
                )
            });
        }
        let source = self.resolve_source(source)?;
        let metadata = Self::validate_source(&source)?;
        if resolve_shebang(&source)?.is_some() {
            return Ok(source);
        }
        if !self.overlay.is_internal(&source) {
            if !Self::requires_copy(&source, &metadata)? {
                return Ok(source);
            }
            self.prepare_copy(&source, &metadata)
        } else if matches!(
            self.overlay.state(&source)?,
            Some(EntryState::Cached {
                materializer: Materializer::Executable,
                ..
            })
        ) {
            Ok(source)
        } else {
            let source = self.prepare_internal_copy(&source, &metadata)?;
            self.overlay.mark_executable(&source)?;
            Ok(source)
        }
    }

    fn resolve_source(&self, requested: &Path) -> Result<PathBuf> {
        self.overlay
            .visible_path(requested)
            .and_then(|source| {
                if self.overlay.is_internal(&source) {
                    Ok(source)
                } else {
                    source.canonicalize().with_context(|| {
                        format!("failed to resolve executable {}", requested.display())
                    })
                }
            })
            .with_context(|| format!("failed to resolve executable {}", requested.display()))
    }

    fn requires_copy(source: &Path, metadata: &Metadata) -> Result<bool> {
        let sip_restricted =
            metadata.st_flags() & SF_RESTRICTED != 0 && sip_restricts_protected_files();
        Ok(sip_restricted
            || Self::cached_code_signing_flags(metadata, || Self::code_signing_flags(source))?
                & CS_DYLD_RESTRICTED
                != 0)
    }

    fn cached_code_signing_flags(
        metadata: &Metadata,
        inspect: impl FnOnce() -> Result<u32>,
    ) -> Result<u32> {
        static CACHE: OnceLock<Mutex<HashMap<ExecutableIdentity, u32>>> = OnceLock::new();

        let identity = ExecutableIdentity::new(metadata);
        let mut cache = CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(flags) = cache.get(&identity) {
            return Ok(*flags);
        }
        let flags = inspect()?;
        if cache.len() >= SIGNATURE_CACHE_CAPACITY {
            cache.clear();
        }
        cache.insert(identity, flags);
        Ok(flags)
    }

    fn code_signing_flags(source: &Path) -> Result<u32> {
        let output = Command::new("/usr/bin/codesign")
            .args(["--display", "--verbose=4"])
            .arg(source)
            .output()
            .context("failed to run codesign while inspecting an executable signature")?;
        if !output.status.success() {
            return Ok(0);
        }
        Self::parse_code_signing_flags(&output.stderr)
            .context("failed to parse executable code signature")
    }

    fn parse_code_signing_flags(details: &[u8]) -> Result<u32> {
        let details = std::str::from_utf8(details)?;
        let flags = details
            .lines()
            .find_map(|line| line.split_once(" flags=0x").map(|(_, flags)| flags))
            .context("codesign output has no CodeDirectory flags")?;
        let end = flags
            .find(|byte: char| !byte.is_ascii_hexdigit())
            .unwrap_or(flags.len());
        u32::from_str_radix(&flags[..end], 16).context("invalid CodeDirectory flags")
    }

    fn prepare_copy(&self, source: &Path, metadata: &Metadata) -> Result<PathBuf> {
        self.overlay.prepare_executable(source, |temporary| {
            let architectures = Self::architectures(source)?;
            let selected = Self::select_architecture(Self::native_architecture(), &architectures)
                .with_context(|| {
                format!(
                    "executable {} is incompatible with sandbox build target {}",
                    source.display(),
                    Self::native_architecture()
                )
            })?;
            Self::prepare_executable_contents(
                source,
                temporary,
                metadata,
                &architectures,
                &selected,
            )
        })
    }

    fn prepare_internal_copy(&self, source: &Path, metadata: &Metadata) -> Result<PathBuf> {
        let architectures = Self::architectures(source)?;
        let selected = Self::select_architecture(Self::native_architecture(), &architectures)
            .with_context(|| {
                format!(
                    "executable {} is incompatible with sandbox build target {}",
                    source.display(),
                    Self::native_architecture()
                )
            })?;
        let parent = source
            .parent()
            .context("sandbox executable has no parent")?;
        let temporary = parent.join(format!(".agora-executable-{}.tmp", Uuid::new_v4().simple()));
        let result = Self::prepare_executable_contents(
            source,
            &temporary,
            metadata,
            &architectures,
            &selected,
        )
        .and_then(|()| {
            fs::rename(&temporary, source).with_context(|| {
                format!("failed to publish sandbox executable {}", source.display())
            })
        });
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result.map(|()| source.to_path_buf())
    }

    fn prepare_executable_contents(
        source: &Path,
        temporary: &Path,
        metadata: &Metadata,
        architectures: &[String],
        selected: &ArchitectureSelection,
    ) -> Result<()> {
        if architectures.len() == 1 {
            fs::copy(source, temporary).with_context(|| {
                format!(
                    "failed to copy executable {} to {}",
                    source.display(),
                    temporary.display()
                )
            })?;
        } else {
            Self::run_tool(
                "/usr/bin/lipo",
                [
                    source.as_os_str(),
                    OsStr::new("-thin"),
                    OsStr::new(&selected.slice),
                    OsStr::new("-output"),
                    temporary.as_os_str(),
                ],
                "failed to extract native executable architecture",
            )?;
        }
        let source_mode = metadata.mode();
        fs::set_permissions(temporary, fs::Permissions::from_mode(source_mode | 0o200))?;
        if selected.rewrite_arm64e {
            Self::rewrite_arm64e_subtype(temporary)?;
        }
        Self::run_tool(
            "/usr/bin/codesign",
            [
                OsStr::new("--force"),
                OsStr::new("--sign"),
                OsStr::new("-"),
                OsStr::new("--timestamp=none"),
                OsStr::new("--preserve-metadata=entitlements"),
                temporary.as_os_str(),
            ],
            "failed to ad-hoc sign executable copy",
        )?;
        fs::set_permissions(temporary, fs::Permissions::from_mode(source_mode))?;
        Ok(())
    }

    #[cfg(test)]
    fn prepare_copy_for_test(&self, source: &Path) -> Result<PathBuf> {
        let source = source
            .canonicalize()
            .with_context(|| format!("failed to resolve executable {}", source.display()))?;
        let metadata = Self::validate_source(&source)?;
        self.prepare_copy(&source, &metadata)
    }

    fn validate_source(source: &Path) -> Result<Metadata> {
        let metadata = source
            .metadata()
            .with_context(|| format!("failed to inspect executable {}", source.display()))?;
        if !metadata.is_file() {
            bail!("sandbox executable is not a file: {}", source.display());
        }
        if metadata.mode() & 0o111 == 0 {
            bail!("sandbox executable is not executable: {}", source.display());
        }
        Ok(metadata)
    }

    #[cfg(test)]
    fn destination(&self, source: &Path) -> Result<PathBuf> {
        let relative = source.strip_prefix(Path::new("/")).with_context(|| {
            format!(
                "sandbox executable path is not absolute: {}",
                source.display()
            )
        })?;
        Ok(self.overlay.root().join(relative))
    }

    fn architectures(source: &Path) -> Result<Vec<String>> {
        #[cfg(test)]
        ARCHITECTURE_INSPECTIONS.with(|inspections| inspections.set(inspections.get() + 1));
        let output = Command::new("/usr/bin/lipo")
            .arg("-archs")
            .arg(source)
            .output()
            .with_context(|| format!("failed to inspect Mach-O file {}", source.display()))?;
        let architectures =
            Self::check_output(output, "failed to inspect executable architectures")?;
        Ok(architectures
            .split_ascii_whitespace()
            .map(ToString::to_string)
            .collect())
    }

    fn native_architecture() -> &'static str {
        match std::env::consts::ARCH {
            "aarch64" => "arm64",
            architecture => architecture,
        }
    }

    fn select_architecture(
        target: &str,
        architectures: &[String],
    ) -> Result<ArchitectureSelection> {
        if architectures.iter().any(|value| value == target) {
            return Ok(ArchitectureSelection {
                slice: target.to_string(),
                rewrite_arm64e: false,
            });
        }
        if target == "arm64" && architectures.iter().any(|value| value == "arm64e") {
            return Ok(ArchitectureSelection {
                slice: "arm64e".to_string(),
                rewrite_arm64e: true,
            });
        }
        bail!("no architecture compatible with build target {target}")
    }

    fn rewrite_arm64e_subtype(path: &Path) -> Result<()> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("failed to open executable copy {}", path.display()))?;
        let mut header = [0_u8; 12];
        file.read_exact(&mut header)
            .with_context(|| format!("failed to read Mach-O header {}", path.display()))?;
        let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let cpu_type = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let cpu_subtype = u32::from_le_bytes(header[8..12].try_into().unwrap());
        if magic != MACH_64_MAGIC
            || cpu_type != CPU_TYPE_ARM64
            || cpu_subtype & 0x00ff_ffff != CPU_SUBTYPE_ARM64E
        {
            bail!("extracted executable is not an arm64e Mach-O file");
        }
        file.seek(SeekFrom::Start(8))?;
        file.write_all(&0_u32.to_le_bytes())?;
        file.flush()?;
        Ok(())
    }

    fn run_tool<'a>(
        program: &str,
        arguments: impl IntoIterator<Item = &'a OsStr>,
        context: &'static str,
    ) -> Result<()> {
        let output = Command::new(program)
            .args(arguments)
            .output()
            .with_context(|| context)?;
        Self::check_output(output, context).map(|_| ())
    }

    fn check_output(output: Output, context: &'static str) -> Result<String> {
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!("{context}: {stderr}");
        }
        String::from_utf8(output.stdout).with_context(|| context)
    }
}

pub(crate) fn resolve_shebang(path: &Path) -> Result<Option<Shebang>> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to open executable {}", path.display()))?;
    let mut line = [0_u8; MAX_SHEBANG_LINE_SIZE];
    let length = file
        .read(&mut line)
        .with_context(|| format!("failed to read executable {}", path.display()))?;
    let line = &line[..length];
    if !line.starts_with(b"#!") {
        return Ok(None);
    }
    let end = match line.iter().position(|byte| *byte == b'\n') {
        Some(end) => end,
        None if length < MAX_SHEBANG_LINE_SIZE => length,
        None => bail!("executable shebang is too long: {}", path.display()),
    };
    let mut command = &line[2..end];
    if command.last() == Some(&b'\r') {
        command = &command[..command.len() - 1];
    }
    command = trim_ascii_whitespace(command);
    if command.is_empty() {
        bail!("executable shebang has no interpreter: {}", path.display());
    }
    let interpreter_end = command
        .iter()
        .position(|byte| byte.is_ascii_whitespace())
        .unwrap_or(command.len());
    let interpreter = PathBuf::from(OsString::from_vec(command[..interpreter_end].to_vec()));
    if !interpreter.is_absolute() {
        bail!(
            "executable shebang interpreter is not absolute: {}",
            path.display()
        );
    }
    let argument = trim_ascii_whitespace(&command[interpreter_end..]);
    let argument = (!argument.is_empty()).then(|| OsString::from_vec(argument.to_vec()));
    Ok(Some(Shebang {
        interpreter,
        argument,
    }))
}

fn trim_ascii_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn sip_restricts_protected_files() -> bool {
    static RESTRICTED: OnceLock<bool> = OnceLock::new();
    *RESTRICTED.get_or_init(|| unsafe { csr_check(CSR_ALLOW_UNRESTRICTED_FS) != 0 })
}

unsafe extern "C" {
    fn csr_check(mask: libc::c_uint) -> libc::c_int;
}

pub(crate) fn resolve_executable(
    program: &OsStr,
    current_dir: Option<&Path>,
    environment: &BTreeMap<OsString, OsString>,
) -> Result<PathBuf> {
    let base = current_dir
        .map(Path::to_path_buf)
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)?;
    if program.as_bytes().contains(&b'/') {
        let path = Path::new(program);
        return Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            base.join(path)
        });
    }

    let path = environment
        .get(OsStr::new("PATH"))
        .cloned()
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_else(|| OsString::from(DEFAULT_EXECUTABLE_PATH));
    for directory in std::env::split_paths(&path) {
        let directory = if directory.as_os_str().is_empty() {
            base.clone()
        } else if directory.is_absolute() {
            directory
        } else {
            base.join(directory)
        };
        let candidate = directory.join(program);
        if candidate
            .metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
        {
            return Ok(candidate);
        }
    }
    bail!("sandbox executable was not found in PATH: {:?}", program)
}

#[cfg(test)]
mod tests;
