use super::{
    AgentDispatch, Argument, CommandArguments, CommandContext, CommandExecution, CommandHandler,
    CommandNode,
};
use crate::channel::{ChannelAgentStatus, ChannelButton, ChannelButtonStyle, ChannelReply};
use crate::i18n;
use crate::store::{ChannelSessionKey, SessionStore};
use crate::task::{CommandRequest, TaskContent};
use anyhow::{Result, bail};
use std::collections::HashSet;

#[derive(Clone)]
pub(super) struct AskCommand {
    store: SessionStore,
}

impl AskCommand {
    pub(super) fn new(store: SessionStore) -> Self {
        Self { store }
    }

    pub(super) fn command(&self) -> CommandNode<CommandHandler> {
        let command = self.clone();
        CommandNode::new("ask", i18n::ASK_COMMAND_DESCRIPTION)
            .argument(Argument::required(
                "agent_name",
                i18n::AGENT_NAME_ARGUMENT_DESCRIPTION,
            ))
            .argument(Argument::required_remaining(
                "prompt",
                i18n::ASK_PROMPT_ARGUMENT_DESCRIPTION,
            ))
            .handler(CommandHandler::new(move |context, arguments| {
                let command = command.clone();
                async move { command.ask(context, arguments) }
            }))
            .subcommand(self.list_command())
            .subcommand(self.agent_command("status", i18n::ASK_STATUS_DESCRIPTION, Self::status))
            .subcommand(self.agent_command("disable", i18n::ASK_DISABLE_DESCRIPTION, Self::disable))
            .subcommand(self.agent_command("enable", i18n::ASK_ENABLE_DESCRIPTION, Self::enable))
    }

    fn ask(
        &self,
        context: CommandContext,
        arguments: CommandArguments,
    ) -> Result<CommandExecution> {
        let agent_name = Self::required_argument(&arguments, "agent_name")?;
        let prompt = Self::required_argument(&arguments, "prompt")?;
        let Some(agent) = context
            .agents()
            .iter()
            .find(|agent| agent.name() == agent_name)
            .cloned()
        else {
            return Ok(CommandExecution::Reply(Some(Self::unknown_agent_reply(
                &agent_name,
            ))));
        };
        let mut content = TaskContent::new(prompt);
        if let Some(message) = context.message() {
            for attachment in message.attachments() {
                content = content.with_attachment(attachment.clone());
            }
        }
        Ok(CommandExecution::Dispatch(AgentDispatch::new(
            vec![agent],
            content,
        )))
    }

    fn list_command(&self) -> CommandNode<CommandHandler> {
        let command = self.clone();
        CommandNode::new("list", i18n::ASK_LIST_DESCRIPTION).handler(CommandHandler::new(
            move |context, arguments| {
                let command = command.clone();
                async move { command.list(context, arguments).await }
            },
        ))
    }

    fn agent_command(
        &self,
        name: &'static str,
        description: &'static str,
        handler: fn(&Self, CommandContext, CommandArguments) -> Result<Option<ChannelReply>>,
    ) -> CommandNode<CommandHandler> {
        let command = self.clone();
        CommandNode::new(name, description)
            .argument(Argument::required(
                "agent_name",
                i18n::AGENT_NAME_ARGUMENT_DESCRIPTION,
            ))
            .handler(CommandHandler::new(move |context, arguments| {
                let command = command.clone();
                async move { handler(&command, context, arguments) }
            }))
    }

    async fn list(
        &self,
        context: CommandContext,
        _arguments: CommandArguments,
    ) -> Result<Option<ChannelReply>> {
        Ok(Some(ChannelReply::agent_list(self.agent_statuses(
            context.channel_name(),
            context.session_id(),
            context.agents(),
        )?)))
    }

    fn status(
        &self,
        context: CommandContext,
        arguments: CommandArguments,
    ) -> Result<Option<ChannelReply>> {
        let agent_name = Self::required_argument(&arguments, "agent_name")?;
        let Some(agent) = context
            .agents()
            .iter()
            .find(|agent| agent.name() == agent_name)
        else {
            return Ok(Some(Self::unknown_agent_reply(&agent_name)));
        };
        let key = ChannelSessionKey::new(context.channel_name(), context.session_id());
        Ok(Some(ChannelReply::agent_status(ChannelAgentStatus::new(
            agent.name(),
            self.store.is_agent_enabled(&key, agent.name())?,
        ))))
    }

    fn disable(
        &self,
        context: CommandContext,
        arguments: CommandArguments,
    ) -> Result<Option<ChannelReply>> {
        self.update_agent_status(context, arguments, false)
    }

    fn enable(
        &self,
        context: CommandContext,
        arguments: CommandArguments,
    ) -> Result<Option<ChannelReply>> {
        self.update_agent_status(context, arguments, true)
    }

    fn update_agent_status(
        &self,
        context: CommandContext,
        arguments: CommandArguments,
        enabled: bool,
    ) -> Result<Option<ChannelReply>> {
        let agent_name = Self::required_argument(&arguments, "agent_name")?;
        let Some(agent) = context
            .agents()
            .iter()
            .find(|agent| agent.name() == agent_name)
        else {
            return Ok(Some(Self::unknown_agent_reply(&agent_name)));
        };
        self.set_agent_enabled(
            context.channel_name(),
            context.session_id(),
            agent.name(),
            enabled,
        )?;

        if context.is_structured() {
            return Ok(Some(ChannelReply::agent_list(self.agent_statuses(
                context.channel_name(),
                context.session_id(),
                context.agents(),
            )?)));
        }
        Ok(Some(ChannelReply::agent_status(ChannelAgentStatus::new(
            agent.name(),
            enabled,
        ))))
    }

    fn agent_statuses(
        &self,
        channel_name: &str,
        session_id: &str,
        agents: &[crate::agent::ConfiguredAgent],
    ) -> Result<Vec<ChannelAgentStatus>> {
        let key = ChannelSessionKey::new(channel_name, session_id);
        let disabled = self
            .store
            .disabled_agents(&key)?
            .into_iter()
            .collect::<HashSet<_>>();
        Ok(agents
            .iter()
            .map(|agent| {
                let enabled = !disabled.contains(agent.name());
                ChannelAgentStatus::new(agent.name(), enabled)
                    .with_button(Self::agent_button(agent.name(), enabled))
            })
            .collect())
    }

    fn set_agent_enabled(
        &self,
        channel_name: &str,
        session_id: &str,
        agent_name: &str,
        enabled: bool,
    ) -> Result<()> {
        let key = ChannelSessionKey::new(channel_name, session_id);
        if enabled {
            self.store.enable_agent(&key, agent_name)?;
        } else {
            self.store.disable_agent(&key, agent_name)?;
        }
        Ok(())
    }

    fn agent_button(agent_name: &str, enabled: bool) -> ChannelButton {
        let (text, style, command) = if enabled {
            (i18n::DISABLE_AGENT, ChannelButtonStyle::Default, "disable")
        } else {
            (i18n::ENABLE_AGENT, ChannelButtonStyle::Primary, "enable")
        };
        ChannelButton::new(
            text,
            style,
            CommandRequest::new(["ask", command]).with_argument("agent_name", agent_name),
        )
    }

    fn required_argument(arguments: &CommandArguments, name: &str) -> Result<String> {
        let Some(value) = arguments.argument(name) else {
            bail!("validated command invocation is missing argument: {name}");
        };
        Ok(value.to_string())
    }

    fn unknown_agent_reply(agent_name: &str) -> ChannelReply {
        ChannelReply::new(i18n::unknown_agent(agent_name))
    }
}
