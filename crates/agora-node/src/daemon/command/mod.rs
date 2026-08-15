mod ask;
mod executor;
mod registry;
mod reset;
mod stop;

use crate::agent::ConfiguredAgent;
use crate::channel::ChannelReply;
use crate::daemon::ExecutionScheduler;
use crate::store::SessionStore;
use crate::task::ChannelTaskInput;
use anyhow::Result;

pub(super) use executor::{AgentDispatch, CommandContext, CommandExecution, CommandHandler};
pub(super) use registry::{
    Argument, CommandArguments, CommandNode, CommandRegistry, CommandResolution,
};

pub(super) enum CommandOutcome {
    PassThrough,
    Reply(Option<ChannelReply>),
    Dispatch(AgentDispatch),
}

pub(super) struct CommandRuntime {
    registry: CommandRegistry<CommandHandler>,
}

impl CommandRuntime {
    pub(super) fn new(store: SessionStore, scheduler: ExecutionScheduler) -> Result<Self> {
        let stop = stop::StopCommand::new(scheduler.clone());
        let reset = reset::ResetCommand::new(store.clone(), scheduler);
        let ask = ask::AskCommand::new(store);
        let mut registry = CommandRegistry::new();
        registry.register(stop.command())?;
        registry.register(reset.command())?;
        registry.register(ask.command())?;
        Ok(Self { registry })
    }

    pub(super) async fn handle(
        &self,
        channel_name: &str,
        session_id: &str,
        agents: &[ConfiguredAgent],
        input: &ChannelTaskInput,
    ) -> Result<CommandOutcome> {
        let (resolution, context) = match input {
            ChannelTaskInput::Message(content) => (
                self.registry.route_text(content.text()),
                CommandContext::text(channel_name, session_id, agents.to_vec())
                    .with_message(content.clone()),
            ),
            ChannelTaskInput::Command(request) => (
                self.registry.route_structured(request),
                CommandContext::structured(channel_name, session_id, agents.to_vec()),
            ),
        };

        match resolution {
            CommandResolution::AgentInput => Ok(CommandOutcome::PassThrough),
            CommandResolution::Reply(reply) => {
                Ok(CommandOutcome::Reply(Some(ChannelReply::new(reply))))
            }
            CommandResolution::Invocation(invocation) => {
                let (handler, arguments) = invocation.into_parts();
                Ok(match handler.execute(context, arguments).await? {
                    CommandExecution::Reply(reply) => CommandOutcome::Reply(reply),
                    CommandExecution::Dispatch(dispatch) => CommandOutcome::Dispatch(dispatch),
                })
            }
        }
    }

    #[cfg(test)]
    pub(super) fn registry(&self) -> &CommandRegistry<CommandHandler> {
        &self.registry
    }
}

#[cfg(test)]
mod tests;
