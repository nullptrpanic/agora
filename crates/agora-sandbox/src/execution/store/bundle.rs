use super::ExecutableStore;
use super::macho::{MachODependency, MachOImage};
use crate::filesystem::normalize_path;
use anyhow::{Context, Result, bail};
use std::collections::{HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

const MAX_BUNDLE_DEPENDENCIES: usize = 512;
const MAX_BUNDLE_DEPTH: usize = 32;
const MAX_BUNDLE_PREPARED_ENTRIES: usize = 100_000;
const MAX_BUNDLE_DEPENDENCY_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone)]
struct RunPath {
    path: PathBuf,
    materialized: bool,
}

struct PendingImage {
    path: PathBuf,
    inherited_rpaths: Vec<RunPath>,
    depth: usize,
}

struct ResolvedDependency {
    path: PathBuf,
    materialized: bool,
}

pub(super) fn prepare_dependencies(
    store: &ExecutableStore,
    executable: &Path,
    architecture: &str,
) -> Result<()> {
    let executable = normalize_path(executable)?;
    let Some(bundle_root) = bundle_root(&executable) else {
        return Ok(());
    };
    let executable_directory = executable
        .parent()
        .context("application executable has no parent")?
        .to_path_buf();
    let mut pending = VecDeque::from([PendingImage {
        path: executable.clone(),
        inherited_rpaths: Vec::new(),
        depth: 0,
    }]);
    let mut visited = HashSet::from([executable]);
    let mut prepared_entries = HashSet::new();
    let mut prepared_wrappers = HashSet::new();
    let mut dependency_count = 0_usize;
    let mut prepared_entry_count = 0_usize;
    let mut prepared_bytes = 0_u64;

    while let Some(current) = pending.pop_front() {
        if current.depth > MAX_BUNDLE_DEPTH {
            bail!(
                "application dependency depth exceeds {MAX_BUNDLE_DEPTH}: {}",
                current.path.display()
            );
        }
        let image = MachOImage::read(&current.path, architecture)?;
        let mut rpaths = image
            .rpaths
            .iter()
            .map(|rpath| expand_runpath(rpath, &current.path, &executable_directory))
            .collect::<Result<Vec<_>>>()?;
        rpaths.extend(current.inherited_rpaths);

        for dependency in image.dependencies {
            let Some(resolved) =
                resolve_dependency(&dependency, &current.path, &executable_directory, &rpaths)?
            else {
                if dependency.weak {
                    continue;
                }
                bail!(
                    "unresolved application dependency {:?} required by {}",
                    dependency.path,
                    current.path.display()
                );
            };
            if !resolved.materialized {
                continue;
            }
            if !resolved.path.starts_with(&bundle_root) {
                bail!(
                    "application dependency escapes bundle {}: {}",
                    bundle_root.display(),
                    resolved.path.display()
                );
            }
            match resolved.path.metadata() {
                Ok(metadata) if metadata.is_file() => {}
                Ok(_) => bail!(
                    "application dependency is not a file: {}",
                    resolved.path.display()
                ),
                Err(error) if dependency.weak && error.kind() == std::io::ErrorKind::NotFound => {
                    continue;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to inspect application dependency {}",
                            resolved.path.display()
                        )
                    });
                }
            }
            if !visited.insert(resolved.path.clone()) {
                continue;
            }
            dependency_count += 1;
            if dependency_count > MAX_BUNDLE_DEPENDENCIES {
                bail!("application dependency closure exceeds preparation limits");
            }
            prepare_loader_artifacts(
                store,
                &resolved.path,
                &mut prepared_entries,
                &mut prepared_wrappers,
                &mut prepared_entry_count,
                &mut prepared_bytes,
            )?;
            pending.push_back(PendingImage {
                path: resolved.path,
                inherited_rpaths: rpaths.clone(),
                depth: current.depth + 1,
            });
        }
    }
    Ok(())
}

fn prepare_loader_artifacts(
    store: &ExecutableStore,
    image: &Path,
    prepared_entries: &mut HashSet<PathBuf>,
    prepared_wrappers: &mut HashSet<PathBuf>,
    prepared_entry_count: &mut usize,
    prepared_bytes: &mut u64,
) -> Result<()> {
    if prepared_entries.contains(image) {
        return Ok(());
    }
    let wrapper = framework_wrapper_root(image);
    if let Some(wrapper) = wrapper.as_ref()
        && prepared_wrappers.insert(wrapper.clone())
    {
        let (entries, scanned) = collect_wrapper_entries(wrapper)?;
        let new_entries = entries
            .iter()
            .filter(|entry| !prepared_entries.contains(&entry.path))
            .collect::<Vec<_>>();
        let entry_count = prepared_entry_count
            .checked_add(scanned.saturating_add(1))
            .context("application dependency entry count overflow")?;
        let bytes = new_entries
            .iter()
            .try_fold(*prepared_bytes, |total, entry| {
                total
                    .checked_add(entry.bytes)
                    .context("application dependency size overflow")
            })?;
        if entry_count > MAX_BUNDLE_PREPARED_ENTRIES || bytes > MAX_BUNDLE_DEPENDENCY_BYTES {
            bail!("application dependency closure exceeds preparation limits");
        }
        *prepared_entry_count = entry_count;
        *prepared_bytes = bytes;
        store.overlay.prepare_loader_tree(wrapper)?;
        prepared_entries.extend(entries.into_iter().map(|entry| entry.path));
        return prepared_entries
            .contains(image)
            .then_some(())
            .context("framework loader image is not contained in its prepared wrapper");
    }
    let bytes_to_add = image
        .metadata()
        .with_context(|| format!("failed to inspect loader image {}", image.display()))?
        .len();
    let entry_count = prepared_entry_count
        .checked_add(1)
        .context("application dependency entry count overflow")?;
    let bytes = prepared_bytes
        .checked_add(bytes_to_add)
        .context("application dependency size overflow")?;
    if entry_count > MAX_BUNDLE_PREPARED_ENTRIES || bytes > MAX_BUNDLE_DEPENDENCY_BYTES {
        bail!("application dependency closure exceeds preparation limits");
    }
    store.overlay.prepare_loader_image(image)?;
    prepared_entries.insert(image.to_path_buf());
    *prepared_entry_count = entry_count;
    *prepared_bytes = bytes;
    Ok(())
}

struct LoaderEntry {
    path: PathBuf,
    bytes: u64,
}

fn loader_entry(path: &Path) -> Result<LoaderEntry> {
    let metadata = path.symlink_metadata().with_context(|| {
        format!(
            "failed to inspect application loader artifact {}",
            path.display()
        )
    })?;
    let file_type = metadata.file_type();
    let bytes = if file_type.is_file() {
        metadata.len()
    } else if file_type.is_symlink() {
        let target = fs::read_link(path).with_context(|| {
            format!(
                "failed to read application loader symlink {}",
                path.display()
            )
        })?;
        u64::try_from(target.as_os_str().as_bytes().len())
            .context("application loader symlink is too large")?
    } else {
        bail!(
            "unsupported application loader artifact: {}",
            path.display()
        );
    };
    Ok(LoaderEntry {
        path: path.to_path_buf(),
        bytes,
    })
}

fn collect_wrapper_entries(root: &Path) -> Result<(Vec<LoaderEntry>, usize)> {
    let mut pending = VecDeque::from([root.to_path_buf()]);
    let mut entries = Vec::new();
    let mut scanned = 0_usize;
    while let Some(directory) = pending.pop_front() {
        for entry in fs::read_dir(&directory).with_context(|| {
            format!(
                "failed to inspect framework wrapper {}",
                directory.display()
            )
        })? {
            let entry = entry?;
            scanned = scanned
                .checked_add(1)
                .context("framework wrapper entry count overflow")?;
            if scanned > MAX_BUNDLE_PREPARED_ENTRIES {
                bail!("application dependency closure exceeds preparation limits");
            }
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push_back(path);
            } else {
                entries.push(loader_entry(&path)?);
            }
        }
    }
    Ok((entries, scanned))
}

fn framework_wrapper_root(image: &Path) -> Option<PathBuf> {
    let framework = image
        .ancestors()
        .find(|ancestor| ancestor.extension() == Some(OsStr::new("framework")))?;
    let relative = image.strip_prefix(framework).ok()?;
    let mut components = relative.components();
    if components.next() == Some(Component::Normal(OsStr::new("Versions")))
        && let Some(Component::Normal(version)) = components.next()
    {
        let version_root = framework.join("Versions").join(version);
        if version_root
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        {
            return Some(version_root);
        }
    }
    Some(framework.to_path_buf())
}

fn bundle_root(executable: &Path) -> Option<PathBuf> {
    executable.ancestors().find_map(|ancestor| {
        (ancestor.extension() == Some(OsStr::new("app"))
            && ancestor.join("Contents/Info.plist").is_file())
        .then(|| ancestor.to_path_buf())
    })
}

fn expand_runpath(rpath: &OsStr, loader: &Path, executable_directory: &Path) -> Result<RunPath> {
    let bytes = rpath.as_bytes();
    if let Some(suffix) = token_suffix(bytes, b"@executable_path") {
        return Ok(RunPath {
            path: join_token_path(executable_directory, suffix)?,
            materialized: true,
        });
    }
    if let Some(suffix) = token_suffix(bytes, b"@loader_path") {
        return Ok(RunPath {
            path: join_token_path(
                loader.parent().context("Mach-O loader has no parent")?,
                suffix,
            )?,
            materialized: true,
        });
    }
    let path = PathBuf::from(rpath);
    if path.is_absolute() {
        return Ok(RunPath {
            path: normalize_path(&path)?,
            materialized: false,
        });
    }
    bail!("unsupported relative application RPATH {rpath:?}")
}

fn resolve_dependency(
    dependency: &MachODependency,
    loader: &Path,
    executable_directory: &Path,
    rpaths: &[RunPath],
) -> Result<Option<ResolvedDependency>> {
    let bytes = dependency.path.as_bytes();
    if let Some(suffix) = token_suffix(bytes, b"@executable_path") {
        return Ok(Some(ResolvedDependency {
            path: join_token_path(executable_directory, suffix)?,
            materialized: true,
        }));
    }
    if let Some(suffix) = token_suffix(bytes, b"@loader_path") {
        return Ok(Some(ResolvedDependency {
            path: join_token_path(
                loader.parent().context("Mach-O loader has no parent")?,
                suffix,
            )?,
            materialized: true,
        }));
    }
    if let Some(suffix) = token_suffix(bytes, b"@rpath") {
        for rpath in rpaths {
            let candidate = join_token_path(&rpath.path, suffix)?;
            if candidate.is_file() {
                return Ok(Some(ResolvedDependency {
                    path: candidate,
                    materialized: rpath.materialized,
                }));
            }
        }
        return Ok(None);
    }
    let path = PathBuf::from(&dependency.path);
    if path.is_absolute() {
        return Ok(Some(ResolvedDependency {
            path: normalize_path(&path)?,
            materialized: false,
        }));
    }
    Ok(None)
}

fn token_suffix<'a>(value: &'a [u8], token: &[u8]) -> Option<&'a [u8]> {
    if value == token {
        return Some(&[]);
    }
    value
        .strip_prefix(token)
        .and_then(|suffix| suffix.strip_prefix(b"/"))
}

fn join_token_path(base: &Path, suffix: &[u8]) -> Result<PathBuf> {
    normalize_path(&base.join(OsString::from_vec(suffix.to_vec())))
}

#[cfg(test)]
mod tests;
