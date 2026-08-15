use super::TraceViewerOptions;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

pub(super) struct ResolvedViewerConfig {
    pub(super) config_path: PathBuf,
    pub(super) log_path: PathBuf,
    pub(super) sandbox_binary: PathBuf,
}

struct ResolveEnvironment {
    current_dir: PathBuf,
    current_exe: PathBuf,
    home_dir: PathBuf,
    path_entries: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct SandboxProjection {
    workdir: Option<PathBuf>,
    #[serde(default)]
    log: LogProjection,
}

#[derive(Default, Deserialize)]
struct LogProjection {
    file: Option<PathBuf>,
}

pub(super) fn resolve(options: &TraceViewerOptions) -> Result<ResolvedViewerConfig> {
    let current_dir = env::current_dir().context("failed to resolve the current directory")?;
    let current_exe = env::current_exe().context("failed to resolve the agora-tools binary")?;
    let home_dir = env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is required to resolve sandbox paths")?;
    let path_entries = env::var_os("PATH")
        .map(|value| env::split_paths(&value).collect())
        .unwrap_or_default();
    resolve_with_environment(
        options,
        &ResolveEnvironment {
            current_dir,
            current_exe,
            home_dir,
            path_entries,
        },
    )
}

fn resolve_with_environment(
    options: &TraceViewerOptions,
    environment: &ResolveEnvironment,
) -> Result<ResolvedViewerConfig> {
    let config_path = resolve_path(
        &environment.current_dir,
        &options.config,
        &environment.home_dir,
    )?;
    validate_regular_file(&config_path, "sandbox config")?;
    let stored: SandboxProjection = serde_json::from_slice(
        &fs::read(&config_path)
            .with_context(|| format!("failed to read sandbox config {}", config_path.display()))?,
    )
    .with_context(|| format!("failed to parse sandbox config {}", config_path.display()))?;
    let config_dir = config_path.parent().unwrap_or(Path::new("/"));
    let workdir = stored
        .workdir
        .as_deref()
        .map(|path| resolve_path(config_dir, path, &environment.home_dir))
        .transpose()?
        .unwrap_or_else(|| environment.home_dir.join(".agora-sandbox"));
    let log_path = stored
        .log
        .file
        .as_deref()
        .map(|path| resolve_path(&workdir, path, &environment.home_dir))
        .transpose()?
        .unwrap_or_else(|| workdir.join("runtime/logs/sandbox.log"));
    let sandbox_binary = if let Some(path) = options.sandbox_bin.as_deref() {
        resolve_path(&environment.current_dir, path, &environment.home_dir)?
    } else {
        find_sandbox_binary(environment)?
    };
    validate_executable(&sandbox_binary)?;
    if let Some(parent) = log_path.parent()
        && parent.exists()
        && !parent.is_dir()
    {
        bail!(
            "sandbox log parent is not a directory: {}",
            parent.display()
        );
    }

    Ok(ResolvedViewerConfig {
        config_path,
        log_path,
        sandbox_binary,
    })
}

fn find_sandbox_binary(environment: &ResolveEnvironment) -> Result<PathBuf> {
    if let Some(parent) = environment.current_exe.parent() {
        let sibling = parent.join("agora-sandbox");
        if is_executable(&sibling) {
            return Ok(sibling);
        }
    }
    for directory in &environment.path_entries {
        let directory = if directory.is_absolute() {
            directory.clone()
        } else {
            normalize_absolute(&environment.current_dir.join(directory))?
        };
        let candidate = directory.join("agora-sandbox");
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    bail!("could not find an executable agora-sandbox binary")
}

fn resolve_path(base: &Path, path: &Path, home_dir: &Path) -> Result<PathBuf> {
    let expanded = expand_home(path, home_dir)?;
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    };
    normalize_absolute(&absolute)
}

fn expand_home(path: &Path, home_dir: &Path) -> Result<PathBuf> {
    let Some(value) = path.to_str() else {
        return Ok(path.to_path_buf());
    };
    if value == "~" {
        return Ok(home_dir.to_path_buf());
    }
    if let Some(remainder) = value.strip_prefix("~/") {
        return Ok(home_dir.join(remainder));
    }
    if value.starts_with('~') {
        bail!("unsupported home-relative path: {value}");
    }
    Ok(path.to_path_buf())
}

fn normalize_absolute(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("path is not absolute: {}", path.display());
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) => bail!("unsupported path prefix: {}", path.display()),
        }
    }
    Ok(normalized)
}

fn validate_regular_file(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "{label} is not a regular non-symlink file: {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_executable(path: &Path) -> Result<()> {
    validate_regular_file(path, "agora-sandbox binary")?;
    if fs::metadata(path)?.permissions().mode() & 0o111 == 0 {
        bail!("agora-sandbox binary is not executable: {}", path.display());
    }
    Ok(())
}

fn is_executable(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.permissions().mode() & 0o111 != 0
    })
}

#[cfg(test)]
mod tests {
    use super::{ResolveEnvironment, resolve_with_environment};
    use crate::trace_viewer::TraceViewerOptions;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};

    fn executable(path: &Path) {
        fs::write(path, b"binary").unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn resolves_relative_workdir_and_log_from_their_documented_bases() {
        let root = tempfile::tempdir().unwrap();
        let config_dir = root.path().join("config");
        fs::create_dir(&config_dir).unwrap();
        let config = config_dir.join("sandbox.json");
        fs::write(
            &config,
            r#"{
                "workdir": "../state",
                "log": { "file": "logs/audit.jsonl" },
                "filesystem": { "local": { "encrypt": "plain" } }
            }"#,
        )
        .unwrap();
        let sandbox = root.path().join("agora-sandbox");
        executable(&sandbox);

        let resolved = resolve_with_environment(
            &TraceViewerOptions {
                config: PathBuf::from("config/sandbox.json"),
                sandbox_bin: Some(PathBuf::from("agora-sandbox")),
                open_browser: false,
            },
            &ResolveEnvironment {
                current_dir: root.path().to_path_buf(),
                current_exe: root.path().join("agora-tools"),
                home_dir: root.path().join("home"),
                path_entries: Vec::new(),
            },
        )
        .unwrap();

        assert_eq!(resolved.config_path, config);
        assert_eq!(
            resolved.log_path,
            root.path().join("state/logs/audit.jsonl")
        );
        assert_eq!(resolved.sandbox_binary, sandbox);
    }

    #[test]
    fn resolves_relative_path_entries_from_the_current_directory() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("sandbox.json");
        fs::write(&config, "{}").unwrap();
        let bin_dir = root.path().join("bin");
        fs::create_dir(&bin_dir).unwrap();
        let sandbox = bin_dir.join("agora-sandbox");
        executable(&sandbox);

        let resolved = resolve_with_environment(
            &TraceViewerOptions {
                config,
                sandbox_bin: None,
                open_browser: false,
            },
            &ResolveEnvironment {
                current_dir: root.path().to_path_buf(),
                current_exe: root.path().join("missing/agora-tools"),
                home_dir: root.path().join("home"),
                path_entries: vec![PathBuf::from("bin")],
            },
        )
        .unwrap();

        assert_eq!(resolved.sandbox_binary, sandbox);
    }

    #[test]
    fn applies_default_workdir_log_and_sibling_sandbox_binary() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("sandbox.json");
        fs::write(&config, "{}").unwrap();
        let tool = root.path().join("target/debug/agora-tools");
        fs::create_dir_all(tool.parent().unwrap()).unwrap();
        let sandbox = tool.parent().unwrap().join("agora-sandbox");
        executable(&sandbox);
        let home = root.path().join("home");

        let resolved = resolve_with_environment(
            &TraceViewerOptions {
                config,
                sandbox_bin: None,
                open_browser: false,
            },
            &ResolveEnvironment {
                current_dir: root.path().to_path_buf(),
                current_exe: tool,
                home_dir: home.clone(),
                path_entries: Vec::new(),
            },
        )
        .unwrap();

        assert_eq!(
            resolved.log_path,
            home.join(".agora-sandbox/runtime/logs/sandbox.log")
        );
        assert_eq!(resolved.sandbox_binary, sandbox);
    }

    #[test]
    fn expands_home_only_for_supported_home_relative_paths() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("sandbox.json");
        fs::write(
            &config,
            r#"{"workdir":"~/sandbox","log":{"file":"~/logs/audit.jsonl"}}"#,
        )
        .unwrap();
        let sandbox = root.path().join("agora-sandbox");
        executable(&sandbox);
        let home = root.path().join("home");

        let resolved = resolve_with_environment(
            &TraceViewerOptions {
                config,
                sandbox_bin: Some(sandbox),
                open_browser: false,
            },
            &ResolveEnvironment {
                current_dir: root.path().to_path_buf(),
                current_exe: root.path().join("agora-tools"),
                home_dir: home.clone(),
                path_entries: Vec::new(),
            },
        )
        .unwrap();

        assert_eq!(resolved.log_path, home.join("logs/audit.jsonl"));
    }

    #[test]
    fn rejects_a_symlink_config_without_following_it() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real.json");
        let link = root.path().join("sandbox.json");
        fs::write(&real, "{}").unwrap();
        symlink(&real, &link).unwrap();

        let Err(error) = resolve_with_environment(
            &TraceViewerOptions {
                config: link,
                sandbox_bin: Some(root.path().join("missing")),
                open_browser: false,
            },
            &ResolveEnvironment {
                current_dir: root.path().to_path_buf(),
                current_exe: root.path().join("agora-tools"),
                home_dir: root.path().join("home"),
                path_entries: Vec::new(),
            },
        ) else {
            panic!("symlink config was accepted");
        };

        assert!(error.to_string().contains("regular non-symlink file"));
    }

    #[test]
    fn rejects_non_executable_sandbox_binary() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("sandbox.json");
        let sandbox = root.path().join("agora-sandbox");
        fs::write(&config, "{}").unwrap();
        fs::write(&sandbox, b"binary").unwrap();

        let Err(error) = resolve_with_environment(
            &TraceViewerOptions {
                config,
                sandbox_bin: Some(sandbox),
                open_browser: false,
            },
            &ResolveEnvironment {
                current_dir: root.path().to_path_buf(),
                current_exe: root.path().join("agora-tools"),
                home_dir: root.path().join("home"),
                path_entries: Vec::new(),
            },
        ) else {
            panic!("non-executable sandbox binary was accepted");
        };

        assert!(error.to_string().contains("not executable"));
    }

    #[test]
    fn rejects_log_path_below_an_existing_regular_file() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("sandbox.json");
        fs::write(
            &config,
            r#"{"workdir":"state","log":{"file":"blocked/audit.jsonl"}}"#,
        )
        .unwrap();
        fs::create_dir(root.path().join("state")).unwrap();
        fs::write(root.path().join("state/blocked"), b"not a directory").unwrap();
        let sandbox = root.path().join("agora-sandbox");
        executable(&sandbox);

        let Err(error) = resolve_with_environment(
            &TraceViewerOptions {
                config,
                sandbox_bin: Some(sandbox),
                open_browser: false,
            },
            &ResolveEnvironment {
                current_dir: root.path().to_path_buf(),
                current_exe: root.path().join("agora-tools"),
                home_dir: root.path().join("home"),
                path_entries: Vec::new(),
            },
        ) else {
            panic!("log path below a regular file was accepted");
        };

        assert!(error.to_string().contains("log parent is not a directory"));
    }
}
