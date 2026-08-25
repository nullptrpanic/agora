# Agora

[English](README.md) | [简体中文](README.zh-CN.md)

Agora 提供本地 Agent 真正落地所需的两个基础能力：一个可靠的聊天入口，以及一个“能使用真实宿主上下文、但不把宿主状态交出去”的 Rootless 执行边界。目前二者作为两个可以独立使用的组件交付：

- **Agora Node** 把飞书或 Telegram 变成本地 Codex 和自定义 Agent 的可观察操作界面，而不是一条看不见过程的远程 Shell。
- **Agora Sandbox** 让命令继续使用熟悉的宿主路径，但覆盖范围内的修改只留在私有、加密且可追踪的沙箱空间。

> Agora 目前处于活跃开发阶段（`0.1.0`）。Node 与 Sandbox 已可分别使用，但 Node 任务还不会自动通过 Agora Sandbox 执行。

## Agora 的核心差异

Agora 关注的是“能运行命令的 Demo”和“用户敢于真正使用的 Agent Runtime”之间的差距：

- **聊天窗口就是操作界面，不只是消息通知。** 用户可以直接提交任务、查看命令与进展、停止或重置执行，并在飞书或 Telegram 中获得最终结果。
- **Agent 能使用宿主上下文，但不能反写宿主数据。** 沙箱保留熟悉的下层路径；覆盖范围内的写入、重命名和删除只进入私有 Overlay。
- **沙箱产生的明文不需要宿主文件路径。** 加密模式下，业务文件名和内容落盘即密文；运行时明文存在于匿名文件中，而不是普通宿主可见的 Backing File。
- **每个关键动作都有证据。** 进程执行（包括 `execve`）、文件打开与关闭、网络连接共享 Trace 上下文，可以直接回答“执行了什么、打开了什么、访问了哪里”。
- **部署保持本地且无特权。** 不需要挂载、root 守护进程、内核扩展、重启或修改 macOS 系统保护。

## 工作区结构

| Crate | 职责 | 当前状态 |
| --- | --- | --- |
| [`agora-node`](crates/agora-node/README.zh-CN.md) | 本地守护进程、聊天 Channel、Agent 执行、调度与会话状态 | 已支持飞书、Telegram、Codex 和自定义 Agent |
| [`agora-sandbox`](crates/agora-sandbox/README.zh-CN.md) | Rootless 命令沙箱、Overlay 与加密、远程文件、TLS 拦截和运行审计 | 已在 macOS 实现 |
| `agora-core` | 共享日志、生命周期和稳定的领域工具 | 内部共享库 |
| `agora-server` | 未来的服务端控制面 | 当前仅为骨架，尚无协议实现 |

## 架构概览

```text
飞书 / Telegram
       |
       v
  Agora Node  ------>  Codex / 自定义 Agent
       |                       |
       `-------- 会话状态 -----'

命令 / SDK
       |
       v
 Agora Sandbox  ---> 私有文件系统视图
       |          |-> 进程、文件与网络审计
       |          `-> 可选 SMB 远程目录
       `------------> Rootless 网络与 TLS 拦截
```

Node 与 Sandbox 有意保持独立的职责边界。Channel 不需要知道 Agent 如何执行，Agent 不依赖具体 Channel，Sandbox 也不依赖二者。这让每个组件都可以独立测试，也为未来建立显式、稳定的集成边界留下空间。

## 快速开始

使用 stable Rust 工具链构建整个工作区：

```bash
cargo build --workspace --release
```

然后选择需要运行的组件：

- [运行 Agora Node](crates/agora-node/README.zh-CN.md#使用方法)
- [运行 Agora Sandbox](crates/agora-sandbox/README.zh-CN.md#使用方法)

Agora Sandbox 的运行时仅支持 macOS，构建过程会使用 Xcode Command Line Tools 中的 `clang`、`lipo` 和 `codesign`。配置与平台限制请查看对应组件 README。

## 开发

工作区常用检查命令如下：

```bash
cargo fmt --all -- --check
cargo test --workspace --all-targets --jobs 16
cargo clippy --workspace --all-targets --all-features --jobs 16 -- -D warnings
```
