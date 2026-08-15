use anyhow::{Context, Result, bail, ensure};
use md5::{Digest, Md5};
use serde_json::Value;
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const INNER_BUILD: &str = "AGORA_SANDBOX_INNER_HOOK_BUILD";
const HOOK_CFG: &str = "agora_sandbox_hook_build";
const HOOK_FILE_NAME: &str = "libagora_sandbox.dylib";
const HOOK_TARGET_DIRECTORY: &str = "agora-sandbox-hook";

fn main() {
    if let Err(error) = run() {
        panic!("failed to build embedded sandbox hook: {error:#}");
    }
}

fn run() -> Result<()> {
    println!("cargo:rustc-check-cfg=cfg({HOOK_CFG})");
    println!("cargo:rustc-check-cfg=cfg(coverage)");
    println!("cargo:rerun-if-env-changed={INNER_BUILD}");
    println!("cargo:rerun-if-env-changed=CARGO_TARGET_DIR");
    let inner_build = env::var_os(INNER_BUILD).is_some();
    for input in [
        "build.rs",
        "Cargo.toml",
        "../../Cargo.toml",
        "../../Cargo.lock",
    ] {
        println!("cargo:rerun-if-changed={input}");
    }
    if inner_build {
        println!("cargo:rerun-if-changed=src/platform/macos/hook/filesystem/filesystem_shim.c");
    } else {
        println!("cargo:rerun-if-changed=src");
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "macos" {
        compile_filesystem_shim();
    }
    if target_os != "macos" || inner_build {
        return Ok(());
    }

    let artifact = build_hook_dylib()?;
    validate_target_architecture(&artifact)?;
    verify_linker_signature(&artifact)?;
    configure_embedded_artifact(&artifact)
}

fn compile_filesystem_shim() {
    cc::Build::new()
        .file("src/platform/macos/hook/filesystem/filesystem_shim.c")
        .warnings(true)
        .compile("agora_sandbox_filesystem_shim");
}

fn build_hook_dylib() -> Result<PathBuf> {
    let cargo = env::var_os("CARGO").context("Cargo did not provide the CARGO executable")?;
    let manifest_directory = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").context("Cargo did not provide CARGO_MANIFEST_DIR")?,
    );
    let out_directory =
        PathBuf::from(env::var_os("OUT_DIR").context("Cargo did not provide OUT_DIR")?);
    let target = env::var("TARGET").context("Cargo did not provide TARGET")?;
    let profile = env::var("PROFILE").context("Cargo did not provide PROFILE")?;
    let profile = if profile == "debug" {
        "dev".to_owned()
    } else {
        profile
    };

    let mut command = Command::new(cargo);
    command
        .arg("rustc")
        .arg("--manifest-path")
        .arg(manifest_directory.join("Cargo.toml"))
        .args(["--package", "agora-sandbox", "--lib", "--target"])
        .arg(&target)
        .arg("--profile")
        .arg(profile)
        .arg("--target-dir")
        .arg(hook_target_directory(&manifest_directory)?)
        .arg("--no-default-features")
        .args([
            "--message-format",
            "json-render-diagnostics",
            "--crate-type",
            "cdylib",
            "--",
            "--cfg",
            HOOK_CFG,
            "-Clink-arg=-Wl,-adhoc_codesign",
        ])
        .env(INNER_BUILD, "1")
        .current_dir(&manifest_directory)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut child = command
        .spawn()
        .context("failed to start inner Cargo hook build")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to capture inner Cargo output")?;
    let mut artifact = None;
    for line in BufReader::new(stdout).lines() {
        let line = line.context("failed to read inner Cargo output")?;
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            eprintln!("{line}");
            continue;
        };
        if let Some(rendered) = message
            .get("message")
            .and_then(|message| message.get("rendered"))
            .and_then(Value::as_str)
        {
            eprint!("{rendered}");
        }
        if message.get("reason").and_then(Value::as_str) != Some("compiler-artifact")
            || message
                .get("target")
                .and_then(|target| target.get("name"))
                .and_then(Value::as_str)
                != Some("agora_sandbox")
        {
            continue;
        }
        if let Some(path) = message
            .get("filenames")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(PathBuf::from)
            .find(|path| path.extension().and_then(|value| value.to_str()) == Some("dylib"))
        {
            let destination = out_directory.join(format!("embedded-{HOOK_FILE_NAME}"));
            fs::copy(&path, &destination).with_context(|| {
                format!(
                    "failed to snapshot hook artifact {} to {}",
                    path.display(),
                    destination.display()
                )
            })?;
            artifact = Some(destination);
        }
    }
    let status = child
        .wait()
        .context("failed to wait for inner Cargo hook build")?;
    ensure!(
        status.success(),
        "inner Cargo hook build failed with {status}"
    );
    let artifact = artifact.context("inner Cargo build did not report a dylib artifact")?;
    artifact
        .canonicalize()
        .with_context(|| format!("failed to resolve hook artifact {}", artifact.display()))
}

fn hook_target_directory(manifest_directory: &Path) -> Result<PathBuf> {
    let workspace = manifest_directory
        .parent()
        .and_then(Path::parent)
        .context("agora-sandbox manifest is not inside the workspace")?;
    let target = match env::var_os("CARGO_TARGET_DIR") {
        Some(directory) => {
            let directory = PathBuf::from(directory);
            if directory.is_absolute() {
                directory
            } else {
                workspace.join(directory)
            }
        }
        None => workspace.join("target"),
    };
    Ok(target.join(HOOK_TARGET_DIRECTORY))
}

fn validate_target_architecture(artifact: &Path) -> Result<()> {
    let target = env::var("TARGET").context("Cargo did not provide TARGET")?;
    let expected = match target.as_str() {
        "aarch64-apple-darwin" => "arm64",
        "x86_64-apple-darwin" => "x86_64",
        _ => bail!("unsupported macOS hook target: {target}"),
    };
    let output = Command::new("lipo")
        .arg("-archs")
        .arg(artifact)
        .output()
        .with_context(|| format!("failed to inspect hook architecture {}", artifact.display()))?;
    ensure!(
        output.status.success(),
        "lipo failed for {}: {}",
        artifact.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let architectures =
        String::from_utf8(output.stdout).context("lipo returned a non-UTF-8 architecture list")?;
    let architectures = architectures.split_whitespace().collect::<Vec<_>>();
    ensure!(
        architectures == [expected],
        "hook architecture mismatch for {}: expected {expected}, found {}",
        artifact.display(),
        architectures.join(", ")
    );
    Ok(())
}

fn verify_linker_signature(artifact: &Path) -> Result<()> {
    let output = Command::new("codesign")
        .args(["--verify", "--strict"])
        .arg(artifact)
        .output()
        .with_context(|| format!("failed to verify hook signature {}", artifact.display()))?;
    ensure!(
        output.status.success(),
        "invalid linker-generated hook signature for {}: {}",
        artifact.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn configure_embedded_artifact(artifact: &Path) -> Result<()> {
    let checksum = checksum(artifact)?;
    let artifact = artifact
        .to_str()
        .context("embedded hook artifact path is not valid UTF-8")?;
    println!("cargo:rustc-env=AGORA_SANDBOX_EMBEDDED_HOOK_PATH={artifact}");
    println!("cargo:rustc-env=AGORA_SANDBOX_EMBEDDED_HOOK_MD5={checksum}");
    Ok(())
}

fn checksum(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {} for MD5", path.display()))?;
    let mut digest = Md5::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {} for MD5", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex_digest(digest.finalize().as_slice()))
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
