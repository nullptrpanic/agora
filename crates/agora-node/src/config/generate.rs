use super::{
    AgentConfig, AgentSubscription, AgentType, ChannelConfig, ChannelPermissionConfig,
    ChannelUserPermissionConfig, IsolateMode, LarkChannelConfig, NodeConfig, TelegramChannelConfig,
};
use anyhow::{Context, bail};
use dialoguer::{Input, Password, Select, theme::ColorfulTheme};
use std::io::{self, BufRead, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};

const ACCENT: &str = "\u{1b}[1;36m";
const SUCCESS: &str = "\u{1b}[1;32m";
const RESET: &str = "\u{1b}[0m";

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
) -> anyhow::Result<NodeConfig> {
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
            let app_id = prompt(output, input, "Lark App ID", None, true, interactive)?;
            let secret = secret_prompt(output, input, "Lark App Secret", interactive)?;
            let allowed_user = prompt(
                output,
                input,
                "Allowed Lark user ID",
                None,
                true,
                interactive,
            )?;
            (
                "lark",
                ChannelConfig::Lark(LarkChannelConfig {
                    name: "lark".into(),
                    app_id,
                    secret,
                    permission: permission(allowed_user),
                    proxy: None,
                }),
            )
        }
        1 => {
            let token = secret_prompt(output, input, "Telegram bot token", interactive)?;
            let allowed_user = prompt(
                output,
                input,
                "Allowed Telegram user ID",
                None,
                true,
                interactive,
            )?;
            (
                "telegram",
                ChannelConfig::Telegram(TelegramChannelConfig {
                    name: "telegram".into(),
                    token,
                    permission: permission(allowed_user),
                    proxy: None,
                }),
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

    let config = NodeConfig {
        proxy: None,
        runtime: Default::default(),
        channels: vec![channel_config],
        agents: vec![AgentConfig {
            name: "agent".into(),
            isolate: IsolateMode::Session,
            workspace: workspace.to_string_lossy().into_owned(),
            agent_type: AgentType::Codex,
            path: codex_path,
            model: Some(model),
            effort: Some(effort),
            agent_sandbox: None,
            proxy: None,
            timeout_seconds: super::default_timeout_seconds(),
            max_output_bytes: super::default_max_output_bytes(),
            subscribe: vec![AgentSubscription {
                channel: channel_name.to_string(),
                filter: None,
            }],
        }],
    };
    config.validate()?;
    Ok(config)
}

fn permission(user_id: String) -> ChannelPermissionConfig {
    ChannelPermissionConfig {
        users: vec![ChannelUserPermissionConfig { id: user_id }],
        groups: Vec::new(),
    }
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

fn secret_prompt(
    output: &mut impl Write,
    input: &mut impl BufRead,
    label: &str,
    interactive: bool,
) -> anyhow::Result<String> {
    if !interactive {
        return prompt(output, input, label, None, true, false);
    }
    output.flush()?;
    Password::with_theme(&ColorfulTheme::default())
        .with_prompt(label)
        .allow_empty_password(false)
        .interact()
        .with_context(|| format!("read {label}"))
}

fn read_line(input: &mut impl BufRead, label: &str) -> anyhow::Result<String> {
    let mut value = String::new();
    if input.read_line(&mut value)? == 0 {
        bail!("input closed while waiting for {label}");
    }
    Ok(value.trim().to_string())
}

fn find_executable(name: &str) -> Option<PathBuf> {
    super::resolve_agent_executable(name, Path::new(name)).ok()
}

fn write_config(path: &Path, config: &NodeConfig) -> anyhow::Result<()> {
    atomic_write(path, |writer| {
        serde_json::to_writer_pretty(&mut *writer, config)?;
        writeln!(writer)?;
        Ok(())
    })
}

fn atomic_write(
    path: &Path,
    write: impl FnOnce(&mut dyn Write) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("configuration path has no parent: {}", path.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "create temporary configuration file in {}",
            parent.display()
        )
    })?;
    set_private_permissions(temporary.path())?;
    {
        let mut writer = BufWriter::new(temporary.as_file_mut());
        write(&mut writer)?;
        writer.flush()?;
    }
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replace configuration file {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests;
