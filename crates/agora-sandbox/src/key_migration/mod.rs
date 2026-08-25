use agora_sandbox::runner::{
    FilesystemKeyMigrationProgress, SandboxConfig, migrate_filesystem_key_with_progress,
};
use anyhow::{Context, Result, bail};
use dialoguer::{Input, theme::ColorfulTheme};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;

const ACCENT: &str = "\u{1b}[1;36m";
const SUCCESS: &str = "\u{1b}[1;32m";
const RESET: &str = "\u{1b}[0m";
const BAR_WIDTH: usize = 28;

pub(super) async fn run(workdir: Option<PathBuf>) -> Result<()> {
    let workdir = workdir.unwrap_or_else(SandboxConfig::default_workdir);
    let stdin = io::stdin();
    let stdout = io::stdout();
    let interactive = stdin.is_terminal() && io::stderr().is_terminal();
    let colored = stdout.is_terminal();
    let mut output = stdout.lock();

    section(&mut output, "Filesystem key migration", colored)?;
    writeln!(output, "  Work directory: {}", workdir.display())?;
    let keys = if interactive {
        collect_interactive_keys(&mut output)?
    } else {
        collect_keys(&mut stdin.lock(), &mut output)?
    };

    section(&mut output, "Migration progress", colored)?;
    let mut display = ProgressDisplay::new(&mut output, interactive, colored);
    let mut render_error = None;
    let result =
        migrate_filesystem_key_with_progress(&workdir, keys.current, keys.new, |progress| {
            if render_error.is_none()
                && let Err(error) = display.render(progress)
            {
                render_error = Some(error);
            }
        })
        .await;
    if let Some(error) = render_error {
        return Err(error);
    }
    result?;
    if colored {
        writeln!(
            output,
            "\n{SUCCESS}✓ Filesystem key migrated; existing data was preserved.{RESET}"
        )?;
    } else {
        writeln!(
            output,
            "\n✓ Filesystem key migrated; existing data was preserved."
        )?;
    }
    Ok(())
}

struct MigrationKeys {
    current: String,
    new: String,
}

fn collect_interactive_keys(output: &mut impl Write) -> Result<MigrationKeys> {
    output.flush()?;
    let theme = ColorfulTheme::default();
    let current = Input::<String>::with_theme(&theme)
        .with_prompt("Current filesystem key")
        .interact_text()
        .context("read current filesystem key")?;
    let new = Input::<String>::with_theme(&theme)
        .with_prompt("New filesystem key")
        .interact_text()
        .context("read new filesystem key")?;
    Ok(MigrationKeys { current, new })
}

fn collect_keys(input: &mut impl BufRead, output: &mut impl Write) -> Result<MigrationKeys> {
    let current = prompt(input, output, "Current filesystem key")?;
    let new = prompt(input, output, "New filesystem key")?;
    Ok(MigrationKeys { current, new })
}

fn prompt(input: &mut impl BufRead, output: &mut impl Write, label: &str) -> Result<String> {
    loop {
        write!(output, "  {label}: ")?;
        output.flush()?;
        let mut value = String::new();
        if input.read_line(&mut value)? == 0 {
            bail!("input closed while waiting for {label}");
        }
        let value = value.trim_end_matches(['\r', '\n']).to_string();
        if !value.is_empty() {
            return Ok(value);
        }
        writeln!(output, "  A value is required.")?;
    }
}

fn section(output: &mut impl Write, name: &str, colored: bool) -> Result<()> {
    if colored {
        writeln!(output, "\n{ACCENT}◆ {name}{RESET}")?;
    } else {
        writeln!(output, "\n◆ {name}")?;
    }
    Ok(())
}

struct ProgressDisplay<'a, W> {
    output: &'a mut W,
    interactive: bool,
    colored: bool,
}

impl<'a, W: Write> ProgressDisplay<'a, W> {
    fn new(output: &'a mut W, interactive: bool, colored: bool) -> Self {
        Self {
            output,
            interactive,
            colored,
        }
    }

    fn render(&mut self, progress: FilesystemKeyMigrationProgress) -> Result<()> {
        let percent = usize::from(progress.percent());
        let filled = percent * BAR_WIDTH / 100;
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(BAR_WIDTH - filled));
        let prefix = if self.interactive { "\r" } else { "  " };
        if self.colored {
            write!(
                self.output,
                "{prefix}{ACCENT}[{bar}] {:>3}%{RESET}  {}",
                progress.percent(),
                progress.description()
            )?;
        } else {
            write!(
                self.output,
                "{prefix}[{bar}] {:>3}%  {}",
                progress.percent(),
                progress.description()
            )?;
        }
        if !self.interactive || progress == FilesystemKeyMigrationProgress::Completed {
            writeln!(self.output)?;
        }
        self.output.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
