use super::ExecutableStore;
use super::macho::{MachODependency, MachOImage};
use crate::filesystem::normalize_path;
use anyhow::{Context, Result, bail};
use std::collections::{HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

const MAX_BUNDLE_DEPENDENCIES: usize = 512;
const MAX_BUNDLE_DEPTH: usize = 32;
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
    let mut dependency_count = 0_usize;
    let mut dependency_bytes = 0_u64;

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
            let metadata = match resolved.path.metadata() {
                Ok(metadata) if metadata.is_file() => metadata,
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
            };
            if !visited.insert(resolved.path.clone()) {
                continue;
            }
            dependency_count += 1;
            dependency_bytes = dependency_bytes
                .checked_add(metadata.len())
                .context("application dependency size overflow")?;
            if dependency_count > MAX_BUNDLE_DEPENDENCIES
                || dependency_bytes > MAX_BUNDLE_DEPENDENCY_BYTES
            {
                bail!("application dependency closure exceeds preparation limits");
            }
            store.overlay.prepare_loader_image(&resolved.path)?;
            pending.push_back(PendingImage {
                path: resolved.path,
                inherited_rpaths: rpaths.clone(),
                depth: current.depth + 1,
            });
        }
    }
    Ok(())
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
