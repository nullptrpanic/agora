use super::{Argument, CommandArguments, CommandContext, CommandHandler, CommandNode};
use crate::channel::ChannelReply;
use crate::daemon::ExecutionScheduler;
use crate::i18n;

#[derive(Clone)]
pub(super) struct StopCommand {
    scheduler: ExecutionScheduler,
}

impl StopCommand {
    pub(super) fn new(scheduler: ExecutionScheduler) -> Self {
        Self { scheduler }
    }

    pub(super) fn command(&self) -> CommandNode<CommandHandler> {
        let command = self.clone();
        CommandNode::new("stop", i18n::STOP_COMMAND_DESCRIPTION)
            .argument(Argument::optional(
                "agent_name",
                i18n::STOP_AGENT_ARGUMENT_DESCRIPTION,
            ))
            .handler(CommandHandler::new(move |context, arguments| {
                let command = command.clone();
                async move { command.stop(context, arguments).await }
            }))
    }

    async fn stop(
        &self,
        context: CommandContext,
        arguments: CommandArguments,
    ) -> anyhow::Result<Option<ChannelReply>> {
        let agent_name = arguments.argument("agent_name");
        let stopped = self
            .scheduler
            .stop(context.channel_name(), context.session_id(), agent_name);
        Ok(Some(Self::reply(agent_name, &stopped)))
    }

    fn reply(agent_name: Option<&str>, stopped: &[String]) -> ChannelReply {
        if stopped.is_empty() {
            return match agent_name {
                Some(agent_name) => ChannelReply::new(i18n::no_running_agent(agent_name)),
                None => ChannelReply::new(i18n::no_running_agents()),
            };
        }
        ChannelReply::new(i18n::stopped_agents(stopped))
    }
}
