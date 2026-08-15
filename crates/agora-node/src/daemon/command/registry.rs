use crate::i18n;
use crate::task::CommandRequest;
use anyhow::{Result, bail};
use std::collections::HashSet;

const HELP: &str = "help";

#[derive(Clone, Debug)]
pub(in crate::daemon) struct Argument {
    name: &'static str,
    description: &'static str,
    required: bool,
    consume_remaining: bool,
}

impl Argument {
    pub(in crate::daemon) fn required(name: &'static str, description: &'static str) -> Self {
        Self {
            name,
            description,
            required: true,
            consume_remaining: false,
        }
    }

    pub(in crate::daemon) fn required_remaining(
        name: &'static str,
        description: &'static str,
    ) -> Self {
        Self {
            name,
            description,
            required: true,
            consume_remaining: true,
        }
    }

    pub(in crate::daemon) fn optional(name: &'static str, description: &'static str) -> Self {
        Self {
            name,
            description,
            required: false,
            consume_remaining: false,
        }
    }

    fn syntax(&self) -> String {
        if self.required {
            format!("{{{}}}", self.name)
        } else {
            format!("[{{{}}}]", self.name)
        }
    }

    fn help(&self) -> String {
        let requirement = if self.required {
            i18n::REQUIRED
        } else {
            i18n::OPTIONAL
        };
        format!("{} ({requirement}) - {}", self.name, self.description)
    }
}

#[derive(Clone, Debug)]
pub(in crate::daemon) struct CommandNode<H> {
    name: &'static str,
    description: &'static str,
    arguments: Vec<Argument>,
    handler: Option<H>,
    subcommands: Vec<CommandNode<H>>,
}

impl<H> CommandNode<H> {
    pub(in crate::daemon) fn new(name: &'static str, description: &'static str) -> Self {
        Self {
            name,
            description,
            arguments: Vec::new(),
            handler: None,
            subcommands: Vec::new(),
        }
    }

    pub(in crate::daemon) fn argument(mut self, argument: Argument) -> Self {
        self.arguments.push(argument);
        self
    }

    pub(in crate::daemon) fn handler(mut self, handler: H) -> Self {
        self.handler = Some(handler);
        self
    }

    pub(in crate::daemon) fn subcommand(mut self, subcommand: CommandNode<H>) -> Self {
        self.subcommands.push(subcommand);
        self
    }

    fn validate(&self, parent: &str) -> Result<()> {
        let path = if parent.is_empty() {
            self.name.to_string()
        } else {
            format!("{parent} {}", self.name)
        };
        if !Self::valid_name(self.name) {
            bail!("invalid command name: {path}");
        }
        if self.name == HELP {
            bail!("command name is reserved: {path}");
        }
        if self.description.trim().is_empty() {
            bail!("command description is empty: {path}");
        }
        if self.handler.is_none() && !self.arguments.is_empty() {
            bail!("command without a handler has arguments: {path}");
        }
        if self.handler.is_none() && self.subcommands.is_empty() {
            bail!("command has neither a handler nor subcommands: {path}");
        }

        let mut optional_seen = false;
        let mut argument_names = HashSet::new();
        for (index, argument) in self.arguments.iter().enumerate() {
            if !Self::valid_name(argument.name) {
                bail!("invalid argument name in /{path}: {}", argument.name);
            }
            if !argument_names.insert(argument.name) {
                bail!("duplicate argument in /{path}: {}", argument.name);
            }
            if optional_seen && argument.required {
                bail!("required argument follows an optional argument in /{path}");
            }
            if argument.consume_remaining && index + 1 != self.arguments.len() {
                bail!(
                    "remaining argument is not last in /{path}: {}",
                    argument.name
                );
            }
            optional_seen |= !argument.required;
        }

        let mut subcommand_names = HashSet::new();
        for subcommand in &self.subcommands {
            if !subcommand_names.insert(subcommand.name) {
                bail!("duplicate subcommand in /{path}: {}", subcommand.name);
            }
            subcommand.validate(&path)?;
        }
        Ok(())
    }

    fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
    }

    fn syntax(&self, path: &[&str]) -> String {
        let mut syntax = format!("/{}", path.join(" "));
        for argument in &self.arguments {
            syntax.push(' ');
            syntax.push_str(&argument.syntax());
        }
        syntax
    }

    fn help(&self, path: &[&str]) -> String {
        let command_path = format!("/{}", path.join(" "));
        let mut lines = vec![format!("{command_path} - {}", self.description)];

        if self.handler.is_some() {
            lines.push(String::new());
            lines.push(i18n::USAGE_TITLE.to_string());
            lines.push(self.syntax(path));
            if !self.arguments.is_empty() {
                lines.push(String::new());
                lines.push(i18n::ARGUMENTS_TITLE.to_string());
                lines.extend(self.arguments.iter().map(Argument::help));
            }
        }

        if !self.subcommands.is_empty() {
            lines.push(String::new());
            lines.push(i18n::SUBCOMMANDS_TITLE.to_string());
            for subcommand in &self.subcommands {
                let mut subcommand_path = path.to_vec();
                subcommand_path.push(subcommand.name);
                lines.push(subcommand.syntax(&subcommand_path));
                lines.push(format!("  {}", subcommand.description));
                lines.extend(
                    subcommand
                        .arguments
                        .iter()
                        .map(|argument| format!("  {}", argument.help())),
                );
            }
            lines.push(String::new());
            lines.push(i18n::command_details_hint(&command_path));
        }

        lines.join("\n")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedArgument {
    name: &'static str,
    value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::daemon) struct CommandArguments {
    values: Vec<ParsedArgument>,
}

impl CommandArguments {
    pub(in crate::daemon) fn argument(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|argument| argument.name == name)
            .map(|argument| argument.value.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::daemon) struct CommandInvocation<H> {
    handler: H,
    arguments: CommandArguments,
}

impl<H> CommandInvocation<H> {
    pub(in crate::daemon) fn into_parts(self) -> (H, CommandArguments) {
        (self.handler, self.arguments)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::daemon) enum CommandResolution<H> {
    AgentInput,
    Invocation(CommandInvocation<H>),
    Reply(String),
}

pub(in crate::daemon) struct CommandRegistry<H> {
    commands: Vec<CommandNode<H>>,
}

impl<H> CommandRegistry<H>
where
    H: Clone,
{
    pub(in crate::daemon) fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    pub(in crate::daemon) fn register(&mut self, command: CommandNode<H>) -> Result<()> {
        command.validate("")?;
        if self
            .commands
            .iter()
            .any(|current| current.name == command.name)
        {
            bail!("duplicate root command: {}", command.name);
        }
        self.commands.push(command);
        Ok(())
    }

    #[cfg(test)]
    pub(in crate::daemon) fn route(&self, input: &str) -> CommandResolution<H> {
        self.route_text(input)
    }

    pub(in crate::daemon) fn route_text(&self, input: &str) -> CommandResolution<H> {
        let input = input.trim();
        if !input.starts_with('/') {
            return CommandResolution::AgentInput;
        }

        let mut parts = input.split_whitespace();
        let token = parts.next().unwrap_or_default();
        let command_name = token.strip_prefix('/').unwrap_or_default();
        let remaining = parts.collect::<Vec<_>>();

        if command_name == HELP {
            return if remaining.is_empty() {
                CommandResolution::Reply(self.help())
            } else {
                CommandResolution::Reply(i18n::usage("/help"))
            };
        }

        let Some(command) = self
            .commands
            .iter()
            .find(|command| command.name == command_name)
        else {
            return CommandResolution::Reply(i18n::unknown_command(token));
        };

        let mut path = vec![command.name];
        Self::resolve(command, &mut path, &remaining)
    }

    pub(in crate::daemon) fn route_structured(
        &self,
        request: &CommandRequest,
    ) -> CommandResolution<H> {
        let Some((root, remaining)) = request.path().split_first() else {
            return CommandResolution::Reply(i18n::UNKNOWN_STRUCTURED_COMMAND.to_string());
        };
        let Some(mut command) = self.commands.iter().find(|command| command.name == root) else {
            return CommandResolution::Reply(i18n::unknown_command(&format!(
                "/{}",
                request.path().join(" ")
            )));
        };
        let mut path = vec![command.name];
        for name in remaining {
            let Some(subcommand) = command
                .subcommands
                .iter()
                .find(|subcommand| subcommand.name == name)
            else {
                return CommandResolution::Reply(i18n::unknown_command(&format!(
                    "/{}",
                    request.path().join(" ")
                )));
            };
            command = subcommand;
            path.push(command.name);
        }

        let Some(handler) = &command.handler else {
            return CommandResolution::Reply(command.help(&path));
        };
        if request.arguments().keys().any(|name| {
            !command
                .arguments
                .iter()
                .any(|argument| argument.name == name)
        }) || command
            .arguments
            .iter()
            .any(|argument| argument.required && request.argument(argument.name).is_none())
        {
            return CommandResolution::Reply(i18n::usage(&command.syntax(&path)));
        }

        CommandResolution::Invocation(CommandInvocation {
            handler: handler.clone(),
            arguments: CommandArguments {
                values: command
                    .arguments
                    .iter()
                    .filter_map(|definition| {
                        request
                            .argument(definition.name)
                            .map(|value| ParsedArgument {
                                name: definition.name,
                                value: value.to_string(),
                            })
                    })
                    .collect(),
            },
        })
    }

    fn resolve(
        command: &CommandNode<H>,
        path: &mut Vec<&'static str>,
        remaining: &[&str],
    ) -> CommandResolution<H> {
        if let Some(token) = remaining.first() {
            if *token == HELP {
                return if remaining.len() == 1 {
                    CommandResolution::Reply(command.help(path))
                } else {
                    CommandResolution::Reply(i18n::usage(&format!("/{} help", path.join(" "))))
                };
            }
            if let Some(subcommand) = command
                .subcommands
                .iter()
                .find(|subcommand| subcommand.name == *token)
            {
                path.push(subcommand.name);
                return Self::resolve(subcommand, path, &remaining[1..]);
            }
        }

        let Some(handler) = &command.handler else {
            return if remaining.is_empty() {
                CommandResolution::Reply(command.help(path))
            } else {
                let command_path = format!("/{}", path.join(" "));
                CommandResolution::Reply(i18n::unknown_subcommand(&command_path, remaining[0]))
            };
        };

        if remaining.is_empty() && !command.subcommands.is_empty() {
            return CommandResolution::Reply(command.help(path));
        }

        let required = command
            .arguments
            .iter()
            .filter(|argument| argument.required)
            .count();
        let consume_remaining = command
            .arguments
            .last()
            .is_some_and(|argument| argument.consume_remaining);
        if remaining.len() < required
            || (!consume_remaining && remaining.len() > command.arguments.len())
        {
            return CommandResolution::Reply(i18n::usage(&command.syntax(path)));
        }

        let arguments = CommandArguments {
            values: command
                .arguments
                .iter()
                .enumerate()
                .filter_map(|(index, definition)| {
                    let value = if definition.consume_remaining {
                        remaining[index..].join(" ")
                    } else {
                        remaining.get(index)?.to_string()
                    };
                    Some(ParsedArgument {
                        name: definition.name,
                        value,
                    })
                })
                .collect(),
        };
        CommandResolution::Invocation(CommandInvocation {
            handler: handler.clone(),
            arguments,
        })
    }

    fn help(&self) -> String {
        let mut lines = vec![i18n::COMMANDS_TITLE.to_string()];
        lines.extend(
            self.commands
                .iter()
                .map(|command| format!("/{} - {}", command.name, command.description)),
        );
        lines.push(format!("/help - {}", i18n::HELP_DESCRIPTION));
        lines.push(String::new());
        lines.push(i18n::root_command_details_hint().to_string());
        lines.join("\n")
    }
}
