use crate::i18n;

// A display label for common literal invocations. The original command is
// always rendered in the disclosure. Nothing here is evaluated as shell input.
pub(super) fn shell_action_title(command: &str) -> String {
    if command
        .trim()
        .contains(['|', ';', '\n', '\r', '<', '>', '&', '\x60'])
        || command.contains("$(")
    {
        return i18n::TOOL_COMMAND.into();
    }
    let (mut executable, mut args) = command_head(command);
    if matches!(executable, "bash" | "sh" | "zsh") {
        let (flag, script) = split_word(args);
        if matches!(flag, "-c" | "-lc")
            && let Some(script) = unquote(script)
        {
            (executable, args) = command_head(script);
        }
    }
    if executable == "timeout" {
        let (duration, rest) = split_word(args);
        let seconds = duration
            .strip_suffix(['s', 'm', 'h', 'd'])
            .unwrap_or(duration);
        if seconds.starts_with(|c: char| c.is_ascii_digit()) && seconds.parse::<f64>().is_ok() {
            (executable, args) = command_head(rest);
        }
    }
    if !short_name(executable) {
        return i18n::TOOL_COMMAND.into();
    }
    if executable == "curl"
        && let Some(url) = curl_target(args)
        && let Some(host) = url.host_str()
        && host.len() <= 28
    {
        return format!("{} {host}", i18n::REQUEST_HOST);
    }
    let (subcommand, _) = split_word(args);
    if matches!(executable, "cargo" | "git" | "go") && short_name(subcommand) {
        return format!("{} {executable} {subcommand}", i18n::RUN_PROGRAM);
    }
    format!("{} {executable}", i18n::RUN_PROGRAM)
}

fn short_name(word: &str) -> bool {
    word.len() <= 24
        && word.starts_with(|c: char| c.is_ascii_alphanumeric())
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b))
}

fn split_word(input: &str) -> (&str, &str) {
    let input = input.trim();
    input
        .split_once(char::is_whitespace)
        .map_or((input, ""), |(word, rest)| (word, rest.trim_start()))
}

fn command_head(command: &str) -> (&str, &str) {
    let (path, args) = split_word(command);
    (path.rsplit('/').next().unwrap_or(path), args)
}

fn unquote(input: &str) -> Option<&str> {
    let quote = input.chars().next().filter(|c| matches!(c, '\'' | '"'))?;
    input.strip_prefix(quote)?.strip_suffix(quote)
}

// Accept only options whose arity is known. A proxy address, header or second
// positional argument must never be promoted to the destination in the title.
fn curl_target(mut args: &str) -> Option<reqwest::Url> {
    let mut target = None;
    while !args.is_empty() {
        let (word, rest) = curl_word(args)?;
        args = rest;
        match word {
            "--silent" | "--show-error" | "--head" | "--location" | "--fail" => {}
            flag if flag.starts_with('-')
                && flag.len() > 1
                && flag[1..].bytes().all(|b| b"sSLIfkv".contains(&b)) => {}
            "--max-time" | "-m" | "--connect-timeout" | "-o" | "--output" | "-w"
            | "--write-out" | "-H" | "--header" | "-X" | "--request" | "-x" | "--proxy" => {
                (_, args) = curl_word(args)?;
            }
            url if target.is_none()
                && (url.starts_with("http://") || url.starts_with("https://")) =>
            {
                target = Some(url);
            }
            _ => return None,
        }
    }
    let url = target?;
    if url.contains(['$', '\\', '{', '}', '*']) {
        return None;
    }
    reqwest::Url::parse(url).ok()
}

fn curl_word(input: &str) -> Option<(&str, &str)> {
    let first = input.chars().next()?;
    if matches!(first, '\'' | '"') {
        let (value, rest) = input[1..].split_once(first)?;
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return Some((value, rest.trim_start()));
        }
        return None;
    }
    let (word, rest) = split_word(input);
    (!word.contains(['\'', '"', '\\'])).then_some((word, rest))
}
