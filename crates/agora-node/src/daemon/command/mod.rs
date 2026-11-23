mod ask;
mod executor;
mod registry;
mod replay;
mod reset;
mod stop;

use crate::agent::ConfiguredAgent;
use crate::channel::ChannelReply;
use crate::daemon::ExecutionScheduler;
use crate::store::ChannelIdentity;
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
    replies: replay::CommandReplies,
}

impl CommandRuntime {
    pub(super) fn is_control(&self, input: &ChannelTaskInput) -> bool {
        let resolution = match input {
            ChannelTaskInput::Message(content) => self.registry.route_text(content.text()),
            ChannelTaskInput::Command(request) => self.registry.route_structured(request),
        };
        match resolution {
            CommandResolution::Invocation(invocation) => invocation.into_parts().0.is_control(),
            CommandResolution::Reply(_) => true,
            CommandResolution::AgentInput => false,
        }
    }

    pub(super) fn new(store: SessionStore, scheduler: ExecutionScheduler) -> Result<Self> {
        let stop = stop::StopCommand::new(scheduler.clone());
        let reset = reset::ResetCommand::new(store.clone(), scheduler);
        let ask = ask::AskCommand::new(store);
        let mut registry = CommandRegistry::new();
        registry.register(stop.command())?;
        registry.register(reset.command())?;
        registry.register(ask.command())?;
        registry.register(ask.management_command())?;
        Ok(Self {
            registry,
            replies: Default::default(),
        })
    }

    pub(super) fn reply_delivered(
        &self,
        channel: ChannelIdentity,
        session_id: &str,
        task_id: &str,
    ) {
        self.replies.delivered(&(
            crate::store::StoreChannelSessionKey::new(channel, session_id),
            task_id.to_owned(),
        ));
    }

    #[cfg(test)]
    pub(super) async fn handle(
        &self,
        channel: &ChannelIdentity,
        session_id: &str,
        agents: &[ConfiguredAgent],
        input: &ChannelTaskInput,
    ) -> Result<CommandOutcome> {
        self.handle_admitted(channel, session_id, agents, input, None, Default::default())
            .await
    }

    pub(super) async fn handle_admitted(
        &self,
        channel: &ChannelIdentity,
        session_id: &str,
        agents: &[ConfiguredAgent],
        input: &ChannelTaskInput,
        task_id: Option<&str>,
        admission: super::RouteAdmission,
    ) -> Result<CommandOutcome> {
        let (resolution, context) = match input {
            ChannelTaskInput::Message(content) => (
                self.registry.route_text(content.text()),
                CommandContext::text(channel.clone(), session_id, agents.to_vec())
                    .with_message(content.clone()),
            ),
            ChannelTaskInput::Command(request) => (
                self.registry.route_structured(request),
                CommandContext::structured(channel.clone(), session_id, agents.to_vec()),
            ),
        };

        match resolution {
            CommandResolution::AgentInput => Ok(CommandOutcome::PassThrough),
            CommandResolution::Reply(reply) => {
                Ok(CommandOutcome::Reply(Some(ChannelReply::new(reply))))
            }
            CommandResolution::Invocation(invocation) => {
                let (handler, arguments) = invocation.into_parts();
                if handler.is_mutation()
                    && let Some(task_id) = task_id
                {
                    let scope =
                        crate::store::StoreChannelSessionKey::new(channel.clone(), session_id);
                    let command_admission = admission.clone();
                    let result = self
                        .replies
                        .run((scope, task_id.to_owned()), &admission, async move {
                            match handler
                                .execute(context.with_admission(command_admission), arguments)
                                .await?
                            {
                                CommandExecution::Reply(reply) => Ok(reply),
                                CommandExecution::Dispatch(_) => {
                                    anyhow::bail!("control command dispatched an agent")
                                }
                            }
                        })
                        .await?;
                    return Ok(CommandOutcome::Reply(result));
                }
                Ok(
                    match handler
                        .execute(context.with_admission(admission), arguments)
                        .await?
                    {
                        CommandExecution::Reply(reply) => CommandOutcome::Reply(reply),
                        CommandExecution::Dispatch(dispatch) => CommandOutcome::Dispatch(dispatch),
                    },
                )
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
