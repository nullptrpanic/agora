use anyhow::{Context, bail};
use dialoguer::{Input, Select, theme::ColorfulTheme};
use serde::Serialize;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};

const ACCENT: &str = "\u{1b}[1;36m";
const SUCCESS: &str = "\u{1b}[1;32m";
const RESET: &str = "\u{1b}[0m";

#[derive(Serialize)]
struct GeneratedConfig {
    channels: Vec<GeneratedChannel>,
    agents: Vec<GeneratedAgent>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum GeneratedChannel {
    Lark {
        name: &'static str,
        app_id: String,
        secret: String,
    },
    Telegram {
        name: &'static str,
        token: String,
    },
}

#[derive(Serialize)]
struct GeneratedAgent {
    name: &'static str,
    isolate: &'static str,
    workspace: String,
    #[serde(rename = "type")]
    agent_type: &'static str,
    path: String,
    model: String,
    effort: String,
    subscribe: Vec<GeneratedSubscription>,
}

#[derive(Serialize)]
struct GeneratedSubscription {
    channel: &'static str,
}

pub fn run(output_path: &Path) -> anyhow::Result<()> {
    let current_dir = std::env::current_dir().context("get current directory")?;
    let config_path = if output_path.is_absolute() {
        output_path.to_path_buf()
    } else {
        current_dir.join(output_path)
    };

    let stdin = io::stdin();
    let stdout = io::stdout();
    let interactive = stdin.is_terminal() && io::stderr().is_terminal();
    let colored_output = stdout.is_terminal();
    let config = if interactive {
        collect_config(
            &mut io::empty(),
            &mut stdout.lock(),
            &current_dir,
            find_executable("codex"),
            true,
            colored_output,
        )?
    } else {
        collect_config(
            &mut stdin.lock(),
            &mut stdout.lock(),
            &current_dir,
            find_executable("codex"),
            false,
            colored_output,
        )?
    };
    write_config(&config_path, &config)?;
    if colored_output {
        println!(
            "\n{SUCCESS}✓ Configuration written to {}{RESET}",
            config_path.display()
        );
    } else {
        println!("\n✓ Configuration written to {}", config_path.display());
    }
    Ok(())
}

fn collect_config(
    input: &mut impl BufRead,
    output: &mut impl Write,
    workspace: &Path,
    detected_codex: Option<PathBuf>,
    interactive: bool,
    colored: bool,
) -> anyhow::Result<GeneratedConfig> {
    section(output, "Channel", colored)?;
    let channel = choose(
        output,
        input,
        "Select a channel",
        &["Lark", "Telegram"],
        0,
        interactive,
    )?;
    let (channel_name, channel_config) = match channel {
        0 => {
            let app_id = prompt(output, input, "Lark App ID", None, false, interactive)?;
            let secret = prompt(output, input, "Lark App Secret", None, false, interactive)?;
            (
                "lark",
                GeneratedChannel::Lark {
                    name: "lark",
                    app_id,
                    secret,
                },
            )
        }
        1 => {
            let token = prompt(
                output,
                input,
                "Telegram bot token",
                None,
                false,
                interactive,
            )?;
            (
                "telegram",
                GeneratedChannel::Telegram {
                    name: "telegram",
                    token,
                },
            )
        }
        _ => unreachable!("channel choice is bounded"),
    };

    section(output, "Agent", colored)?;
    let agent_type = choose(
        output,
        input,
        "Select an agent type",
        &["Codex"],
        0,
        interactive,
    )?;
    debug_assert_eq!(agent_type, 0);
    let default_path = detected_codex
        .as_deref()
        .map(|path| path.to_string_lossy().into_owned());
    let codex_path = prompt(
        output,
        input,
        "Codex path",
        default_path.as_deref(),
        true,
        interactive,
    )?;
    let model = prompt(output, input, "Model", None, true, interactive)?;
    let effort = prompt(
        output,
        input,
        "Reasoning effort",
        Some("high"),
        true,
        interactive,
    )?;

    Ok(GeneratedConfig {
        channels: vec![channel_config],
        agents: vec![GeneratedAgent {
            name: "agent",
            isolate: "session",
            workspace: workspace.to_string_lossy().into_owned(),
            agent_type: "codex",
            path: codex_path,
            model,
            effort,
            subscribe: vec![GeneratedSubscription {
                channel: channel_name,
            }],
        }],
    })
}

fn section(output: &mut impl Write, name: &str, colored: bool) -> anyhow::Result<()> {
    if colored {
        writeln!(output, "\n{ACCENT}◆ {name}{RESET}")?;
    } else {
        writeln!(output, "\n◆ {name}")?;
    }
    Ok(())
}

fn choose(
    output: &mut impl Write,
    input: &mut impl BufRead,
    question: &str,
    choices: &[&str],
    default: usize,
    interactive: bool,
) -> anyhow::Result<usize> {
    if interactive {
        output.flush()?;
        return Select::with_theme(&ColorfulTheme::default())
            .with_prompt(question)
            .items(choices)
            .default(default)
            .interact()
            .with_context(|| format!("select {question}"));
    }

    writeln!(output, "{question}:")?;
    for (index, choice) in choices.iter().enumerate() {
        let marker = if index == default { '●' } else { '○' };
        writeln!(output, "  {marker} {}. {choice}", index + 1)?;
    }

    loop {
        write!(output, "  Select [1-{}] ({}): ", choices.len(), default + 1)?;
        output.flush()?;
        let value = read_line(input, question)?;
        if value.is_empty() {
            return Ok(default);
        }
        match value.parse::<usize>() {
            Ok(value) if (1..=choices.len()).contains(&value) => return Ok(value - 1),
            _ => writeln!(
                output,
                "  Please enter a number from 1 to {}.",
                choices.len()
            )?,
        }
    }
}

fn prompt(
    output: &mut impl Write,
    input: &mut impl BufRead,
    label: &str,
    default: Option<&str>,
    required: bool,
    interactive: bool,
) -> anyhow::Result<String> {
    if interactive {
        output.flush()?;
        let theme = ColorfulTheme::default();
        let mut prompt = Input::<String>::with_theme(&theme)
            .with_prompt(label)
            .allow_empty(!required);
        if let Some(default) = default {
            prompt = prompt.default(default.to_string());
        }
        return prompt
            .interact_text()
            .with_context(|| format!("read {label}"));
    }

    loop {
        match default {
            Some(default) => write!(output, "{label} [{default}]: ")?,
            None => write!(output, "{label}: ")?,
        }
        output.flush()?;
        let value = read_line(input, label)?;
        if !value.is_empty() {
            return Ok(value);
        }
        if let Some(default) = default {
            return Ok(default.to_string());
        }
        if !required {
            return Ok(String::new());
        }
        writeln!(output, "  A value is required.")?;
    }
}

fn read_line(input: &mut impl BufRead, label: &str) -> anyhow::Result<String> {
    let mut value = String::new();
    if input.read_line(&mut value)? == 0 {
        bail!("input closed while waiting for {label}");
    }
    Ok(value.trim().to_string())
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    executable_names(name).find_map(|name| {
        std::env::split_paths(&path).find_map(|directory| {
            let candidate = directory.join(&name);
            is_executable(&candidate).then_some(candidate)
        })
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(not(windows))]
fn executable_names(name: &str) -> impl Iterator<Item = OsString> {
    [OsString::from(name)].into_iter()
}

#[cfg(windows)]
fn executable_names(name: &str) -> impl Iterator<Item = OsString> {
    let extensions =
        std::env::var_os("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
    std::iter::once(OsString::from(name)).chain(
        extensions
            .to_string_lossy()
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| OsString::from(format!("{name}{extension}")))
            .collect::<Vec<_>>(),
    )
}

fn write_config(path: &Path, config: &GeneratedConfig) -> anyhow::Result<()> {
    let file = open_config_file(path)
        .with_context(|| format!("open configuration file {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, config)?;
    writeln!(writer)?;
    writer.flush()?;
    Ok(())
}

#[cfg(unix)]
fn open_config_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(test)]
mod tests;

#[cfg(not(unix))]
fn open_config_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}
