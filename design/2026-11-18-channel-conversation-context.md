# 飞书和 Telegram 的 Codex 输入

Node 对聊天消息执行权限和 @ 检查，从入站事件取得会话标识，将用户问题和标识写为一个 JSON 对象。普通消息使用处理过机器人提及的原文，`/ask <agent> <问题>` 使用解析出的问题。以下是飞书用户引用一条消息问「看下这个问题」时的完整标准输入示例。

```json
{"user_request":"看下这个问题","context":{"channel":"lark","chat_type":"group","chat_id":"oc_demo","message_id":"om_question","reply_to_message_id":"om_parent"}}
```

`message_id` 是本次提问，`reply_to_message_id` 是被引用的消息。两个渠道按需携带 `thread_id`、`reply_to_message_id` 和 `mentions`，所有消息与会话 ID 都是字符串。发送者身份由渠道用于权限检查、引用回复和群内 @。

Codex 在本轮执行中可以通过环境中已配置的外部 skill，按会话 ID 查询历史或按引用 ID 获取原消息。可见范围取决于该 skill 使用的账号和权限。查询结果随当前 Codex 会话处理。Node 只负责投递请求、转发工具进度和回复。[频道架构规范](../spec/architecture/agent-channel.md#group-conversation-context)
