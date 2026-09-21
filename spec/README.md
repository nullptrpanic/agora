# Agora Spec

Agora is a context-isolated multi-agent collaboration runtime. A local node receives tasks from
external channels, invokes configured agents, and streams execution events back to the originating
channel. The sandbox provides process-tree-scoped filesystem and network boundaries for local
execution.

This directory records durable product direction, current architecture, and engineering
conventions. Architecture documents describe implemented behavior and active constraints; temporary
plans and completed milestones do not belong here. Keep these documents aligned with changes to
module responsibilities, runtime behavior, public APIs, protocols, data flow, security assumptions,
and operational expectations.

## Product Direction

- [Long-Term Vision](vision/long-term.md)

## Architecture

- [Project Modules](architecture/modules.md)
- [Agent Channel](architecture/agent-channel.md)
- [Local Store](architecture/local-store.md)
- [Node Configuration](architecture/node-config.md)
- [Process Lifecycle](architecture/process-lifecycle.md)
- [Sandbox Network](architecture/sandbox-network.md)
- [Sandbox Filesystem And Executable Preparation](architecture/sandbox.md)

## Engineering

- [Engineering Conventions](engineering/conventions.md)
