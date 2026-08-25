# Agora Sandbox

[English](README.md) | [简体中文](README.zh-CN.md) | [工作区](../../README.zh-CN.md)

本地 Agent 最难的问题，不是把整台机器完全藏起来，而是既让工具读取完成任务所需的真实上下文，又不允许它们反写宿主状态。Agora Sandbox 通过私有 Overlay、透明加密、远程文件与运行证据，为一个 macOS 进程树解决这个取舍，而且不需要任何特权部署。

> Agora Sandbox 是协作式的用户态边界，不是内核 Namespace 或虚拟机。用于不可信代码前，请先阅读[安全模型与限制](#安全模型与限制)。

## 核心亮点

- **能读宿主，写入只进沙箱。** 在受管路径内，程序继续看到熟悉的宿主下层视图，但写入、重命名和删除只进入私有上层，宿主原始条目保持不变。
- **私有视图仍然像真实文件系统一样工作。** 普通与定位 I/O、独立文件描述符 Offset、文件锁、Append 和可写 `MAP_SHARED` 都保留原生语义，应用不需要迁移到一套专用存储 API。
- **加密数据没有普通明文 Backing Path。** 加密模式下，业务文件名和内容落盘为带认证的密文；共享运行时明文存在于匿名、已 Unlink 的 Vnode 中，大文件按范围物化，而不是在 Open 时整文件解密。
- **审计直接回答三个问题：执行了什么、打开了什么、访问了哪里。** `execve` 等进程启动、逻辑文件打开/关闭和覆盖范围内的 TCP 连接都带有进程与 Trace 上下文；能够识别时还会补充 HTTP Host 或 TLS SNI。
- **获得 TLS 可见性，不需要向系统安装 CA。** Auto 模式在父进程侧代理中终止覆盖范围内的 TLS、验证上游证书，并把信任限制在子进程范围内，不修改宿主 Keychain；审计仍然只记录元数据。
- **使用远程 Workspace，不需要宿主挂载。** SMB2/3 目录出现在普通逻辑路径中，凭据和协议会话留在父进程；子进程拿到的是匿名文件描述符，而不是挂载能力或原始凭据。
- **Rootless 改变了部署方式。** 不需要 root 守护进程、FUSE 挂载、内核扩展、重启或修改 SIP，因此可以为一次本地工作流即时建立边界，而不是先给整台机器安装基础设施。
- **运行行为在发生时就能看见。** Runtime Trace 把真实 `/bin/bash` 终端和实时、可搜索的进程/文件/网络时间线放在一起，并由 JSON Lines 审计日志提供数据。

## 使用方法

### 1. 前置条件

- Apple Silicon（`aarch64`）或 Intel（`x86_64`）macOS。
- stable Rust 工具链。
- Xcode Command Line Tools，包括 `clang`、`lipo` 和 `codesign`。

### 2. 构建

在工作区根目录执行：

```bash
cargo build --release -p agora-sandbox
```

构建会生成 `target/release/agora-sandbox`，并把当前架构对应的已签名 Hook Library 嵌入二进制。默认构建包含 SMB 远程文件系统与 Runtime Trace。

### 3. 创建配置

创建 `sandbox.json`：

```json
{
  "workdir": "~/.agora-sandbox",
  "tls": "auto",
  "filesystem": {
    "bypass": [],
    "local": {
      "encrypt": "encrypted",
      "key": "replace-with-a-private-passphrase"
    },
    "nfs": []
  },
  "log": {
    "file": "runtime/logs/sandbox.log"
  }
}
```

加密 Key 和远程凭据都是保存在该 JSON 文件中的 Secret。不要提交这个文件，并限制它的权限，例如：

```bash
chmod 600 sandbox.json
```

`workdir` 默认是 `~/.agora-sandbox`。配置中的相对路径会根据字段从配置文件目录或工作目录解析；默认日志路径最终为 `<workdir>/runtime/logs/sandbox.log`。

不需要加密时，可以把 `filesystem.local.encrypt` 设置为 `plain` 并省略 `key`。将 `tls` 设置为 `off` 可以关闭 TLS 卸载。

### 4. 运行命令

```bash
target/release/agora-sandbox run \
  --config sandbox.json \
  --executable "/bin/bash -lc 'printf sandboxed > /tmp/agora-demo.txt && cat /tmp/agora-demo.txt'"
```

`--executable` 的值会被拆分为可执行文件和参数，但 CLI 本身不会解释 Shell 操作符。需要管道、重定向、变量或组合命令时，应显式使用 `/bin/bash -lc '...'`。

命令会继承调用方的标准输入、标准输出、标准错误、环境和当前目录；命令退出码会成为 CLI 退出码。

### 5. 打开 Runtime Trace

```bash
target/release/agora-sandbox web --config sandbox.json
```

该命令会在 PTY 中启动一个固定的交互式 `/bin/bash`，并打开本地 Runtime Trace 页面。使用 `--no-open` 可以只打印 Loopback URL，不自动打开浏览器：

```bash
target/release/agora-sandbox web --config sandbox.json --no-open
```

页面可以发送终端输入、调整尺寸、停止并重新启动这个固定 Shell，但不能选择其他可执行文件、配置、日志路径或宿主命令。UI 及其 JavaScript 依赖都嵌入 Rust 二进制，不需要 Node.js Runtime 或 CDN。

### 6. 查看审计日志

```bash
tail -f ~/.agora-sandbox/runtime/logs/sandbox.log
```

日志采用 JSON Lines，与子进程 stdout/stderr 分离。每条紧凑记录描述一次被拦截的进程执行、文件打开/关闭或网络连接尝试，并带有将后代活动关联起来所需的 Trace 身份。

### 7. 更换加密 Key

先停止所有使用该工作目录的命令，再运行交互式迁移：

```bash
target/release/agora-sandbox key migrate --workdir ~/.agora-sandbox
```

迁移与正常执行使用同一把 Workspace 排他锁，因此 Workspace 仍在使用时会拒绝迁移。

### 8. 增加 SMB 远程目录

配置字段名为 `nfs`，代表 Agora 的协议无关网络文件系统层；当前已实现的后端是 SMB2/3：

```json
{
  "filesystem": {
    "local": {
      "encrypt": "encrypted",
      "key": "replace-with-a-private-passphrase"
    },
    "nfs": [
      {
        "type": "smb",
        "dir": "/workspace",
        "server": "smb://files.example.com/share/project",
        "username": "user",
        "password": "replace-me"
      }
    ]
  }
}
```

`dir` 是沙箱内可见的绝对逻辑路径。远程目录的查找优先级高于本地上层和宿主下层。它不会创建 macOS 挂载：读取按需拉取到匿名本地 Snapshot，发生变化的 Snapshot 会先与打开时的远端基线比对，再通过分阶段的整文件替换发布。因此它提供的是远程文件系统语义，而不是每次 `write` 都同步产生一次 SMB 写请求。

## 文件系统模型

### 什么是 Overlay

Overlay 文件系统把多个层组合成一个逻辑视图：

```text
                       沙箱路径：/project/report.txt
                                  |
                             先查找上层
                                  |
                +-----------------+-----------------+
                |                                   |
             私有上层                            宿主下层
        沙箱改动 / Whiteout                   宿主原始数据
                |                                   |
                +------------- 合并视图 ------------+
```

- **读：** 有远程条目或私有上层条目时优先使用，否则回落到宿主下层条目。
- **首次修改：** 在上层创建或复制对应逻辑对象，后续修改发生在这个私有版本上。
- **删除：** 在上层记录 Whiteout，让下层对象只在沙箱视图中消失。
- **重命名和元数据修改：** 更新逻辑上层 Namespace，不改变宿主下层对象。
- **Bypass：** `/dev` 和通过 `filesystem.bypass` 显式配置的绝对路径使用宿主原生行为，有意绕过 Overlay、加密与文件审计。

它带来的实际效果是非对称访问：沙箱可以继续使用熟悉的宿主路径，但覆盖范围内的修改只保留在自己的工作目录。在加密模式下，上层中的业务文件名和内容都是密文；明文存在于匿名、已 Unlink 的 Vnode 中，而不是普通宿主可见文件。以同一个 macOS 用户身份运行的其他进程仍可删除或篡改物理工作目录，所以这里提供的是静态数据保护和视图隔离，而不是对抗同 OS 身份恶意进程的安全边界。

### 加密 I/O 与大文件

加密采用带认证的分块格式。对于大于 1 MiB 的加密文件，非截断式冷打开只验证 Header，并创建一个长度正确的稀疏匿名明文 Vnode，不会解密整个正文。读取只物化请求范围并做有界 Readahead；写入会报告实际完成的字节范围，只重新加密受影响的 4 KiB 密文块，边界上的部分块使用 Read-Modify-Write。

同一密文 Inode 的多个独立 Open 会共享一个明文 Vnode，因此已完成写入和共享映射可以直接互相可见，不需要反复解密彼此的 Snapshot。与此同时，每次 Open 仍保留独立的 Offset 和状态 Flag，文件锁也能跨 Open 共享同一身份。

`mmap` 使用 macOS 原生 Pager 映射这个匿名明文 Vnode：

- 在调用原生 `mmap` 前先物化本次请求的映射范围；
- Page Fault 不会调用加密 Broker，因此不存在自定义的缺页时 Pager；
- 可写 `MAP_SHARED` 范围会被登记，并在 `msync`、`munmap`、最后关闭、exec 和正常运行时关闭等同步边界重新加密；运行期间也会周期性检测变化；
- `MAP_PRIVATE` 的修改不会写回。

这样可以避免普通大文件在 Open 时整文件解密，同时保留原生文件描述符和映射行为。不过，如果直接映射一个很大的范围，该请求范围仍需要在 `mmap` 前完成物化。

### 共享 Workspace Session

使用相同规范化工作目录、Build 和有效配置的重叠 CLI 命令，会加入同一个临时 Workspace Session。它们共享 Overlay 状态、加密明文 Vnode、文件锁、网络 Controller、审计和远程文件系统状态，但每个调用方仍保留自己的终端与进程组。最后一个命令释放 Lease 后，辅助进程自动退出；系统不会安装常驻的全局守护进程。

## 运行时审计

所有审计记录都带有 Sandbox ID、Run ID 或 Root Trace 身份以及进程上下文，因此后代活动可以串联起来，而不是一组互不相关的日志行。

| 活动 | 记录内容 |
| --- | --- |
| 进程执行 | `execve`、`execv`、`execvp`、`posix_spawn` 或 `posix_spawnp`；可执行文件、完整参数、当前目录、PID、PPID 和 Trace |
| 文件系统 | Overlay 映射前的逻辑路径、打开或关闭、读写访问模式，以及 create/truncate/append/exclusive Flag |
| 网络 | TCP 目标 IP 与端口、PID 和 Trace，以及能够观察到的 Target Host、HTTP Host、TLS SNI 或规范化域名 |

紧凑 CLI 日志有意不记录文件内容、终端输入输出、HTTP Body 或完整 URL。公开 Rust Callback 可以收到更丰富的版本化事件模型；对于网络连接尝试，它可以返回 `Allow`、`Deny` 或 HTTP `Proxy` 决策。

## 网络与 TLS 模型

注入后的进程树会把覆盖范围内的 `connect` 和简单 `connectx` 重定向到带认证的 IPv4/IPv6 Loopback Proxy。Proxy 检查有上限的初始数据，以提取 HTTP Host 或 TLS SNI，询问 Callback 决策，再连接原始目标或选择的 HTTP CONNECT Proxy。

配置 `tls: "auto"` 后，父进程使用系统原生 Root 验证上游 TLS，通过工作目录内或显式配置的 CA 为子进程签发短期证书，并转发已解密的应用数据。信任范围只覆盖注入后的 `SecTrust` 客户端和常见的环境变量感知工具；Agora 不会把 CA 安装进登录或系统 Keychain，私钥也不会传给子进程。

## Rust Library 用法

Library 提供类型化配置、接近标准进程 API 的命令构建、异步 Callback、`spawn`，以及前台执行的便捷方法 `run`：

```rust,no_run
use agora_sandbox::{
    callback::{Decision, Event},
    hook_library,
    runner::{Sandbox, SandboxCommand, SandboxConfig},
};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let workdir = PathBuf::from("/absolute/path/to/.agora-sandbox");
    let hook = hook_library::materialize(&workdir)?;
    let key = std::env::var("AGORA_SANDBOX_KEY")?;
    let config = SandboxConfig::new(hook)
        .with_workdir(&workdir)
        .with_encrypted_workspace(key.as_bytes());

    let sandbox = Sandbox::new(config, |event: Event| async move {
        eprintln!("{event:?}");
        Decision::Allow
    });
    let outcome = sandbox
        .run(SandboxCommand::new("/usr/bin/env").arg("true"))
        .await?;

    println!("sandbox={} run={} status={}", outcome.sandbox_id(), outcome.run_id(), outcome.status());
    Ok(())
}
```

`SandboxCommand` 还支持 `args`、`env`、`current_dir` 和标准 `Stdio` 配置。`spawn` 返回一个仍在运行的 `SandboxChild`；管道读取、执行超时和输出上限由调用方负责。

## 安全模型与限制

- **协作式边界。** 覆盖范围只包括成功加载 Agora Hook 的进程。这不是内核强制的 Namespace、Container 或 VM 边界。
- **仅支持 macOS。** Runtime Hook 和可执行文件准备支持原生 Apple Silicon 与 Intel macOS Target。
- **尚无严格出口限制。** 覆盖范围内的 TCP 拦截错误会中止该连接，但进程可能使用尚未覆盖的调用路径或网络栈。不能把 Intercept 模式当作完整网络封禁。
- **API 覆盖仍有限。** 直接系统调用和部分文件系统 API Family 仍在 Hook 范围之外；若干不支持的修改会返回 `ENOTSUP`。Native Passthrough 路径会有意绕过 Overlay 与审计。
- **宿主身份保持真实。** 沙箱不会虚拟化用户、用户组、ACL、Entitlement、TCC 或 Keychain；后代进程可以使用当前用户的宿主 Keychain。
- **系统代为启动的进程不在当前边界内。** 可以直接准备 App Binary，但不支持 `/usr/bin/open`，因为 LaunchServices 会在注入进程树的生命周期之外启动程序。
- **TLS 客户端兼容性不同。** 同时忽略 macOS `SecTrust` 和 CA 环境变量的客户端，需要单独配置其信任源。
- **正常关闭持久性。** 正常同步和关闭会 Flush 已修改的加密数据；Controller 或宿主崩溃仍可能丢失尚未持久同步的更新，Overlay Namespace 修改也不提供断电回滚或 Journal 恢复。
- **远程写回基于 Snapshot。** 修改后的 SMB 文件需要完整逻辑基线并执行整文件发布；检测到远端并发修改时会拒绝发布，而不是自动合并。

可执行文件准备或沙箱初始化出错时，命令会停止，不会静默脱离已配置的边界运行。

## 开发

在工作区根目录运行聚焦检查：

```bash
cargo test -p agora-sandbox --all-targets --jobs 16
cargo clippy -p agora-sandbox --all-targets --all-features --jobs 16 -- -D warnings
```
