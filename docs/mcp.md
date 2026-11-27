# 在 AI 助手里检索星栈

先同步一次并完成索引。给助手发一句「从我的 star 里挑一个做 CLI 的 Rust 库，引用仓库链接，并告诉我当初为什么收藏」。仓库名、相关度、链接和私人备注会随工具结果返回给你选择的助手。

## Claude Desktop 使用本机二进制

将示例中的绝对路径替换为本机路径。数据目录及 embedding 配置应与同步时一致。把 `mcpServers.starstack` 合并到 Claude Desktop 的开发者配置，退出并重新打开客户端。Windows 的 JSON 路径可用正斜杠。stdio 使用本机操作系统用户可读的初始收藏库，适用于个人桌面。[Claude Desktop 配置示例](../examples/claude_desktop_config.json)

## HTTP 客户端与多用户部署

1. 在星栈 Web 登录自己的账号，进入 **MCP 接入**，创建一个命名 token，复制首次显示的完整值。
2. 选择支持 **Streamable HTTP** 和自定义请求头的 MCP 客户端。URL 填 `http://127.0.0.1:8080/mcp`，请求头填 `Authorization: Bearer <你的星栈 token>`。
3. 远程访问时使用 HTTPS。把部署域名加入 `STARSTACK_MCP_HOSTS`，例如 `stars.example.org`。在客户端删除配置前，可先在后台撤销对应 token。

支持 HTTP 请求头配置的客户端可按其配置格式填写以下内容。示例中的 token 和域名为占位值。

```json
{
  "mcpServers": {
    "starstack": {
      "type": "http",
      "url": "https://stars.example.org/mcp",
      "headers": { "Authorization": "Bearer <StarStack token>" }
    }
  }
}
```

| 工具 | 适用场景 | 参数 |
| --- | --- | --- |
| `search_stars` | 一句话召回并按相关度推荐 | `query` 必填。可选 `list`（GitHub 收藏分类名，对应 CLI 的 `--list`）、`language`、`topic`、`tag`、`archived`、`pinned`、`since`、`until`、`limit` |
| `get_repository` | 阅读指定仓库的详情、README 和批注 | `full_name`，如 `clap-rs/clap` |
| `list_tags` | 找合适的私人标签 | 无 |
| `get_stats` | 查看收藏量、语言分布和最近同步时间 | 无 |
| `request_sync` | 请用户授权刷新收藏 | 无。返回待确认请求，同一用户在后台「同步与索引 → 计划与额度」确认；初始库也可在终端执行返回的确认命令 |

`search_stars` 使用令牌所属账号的语义索引。可在「MCP 接入 → 检索与推荐」开启 AI 聊天模型，默认关闭。开启后，星栈将原问题发送给配置的聊天服务以补充检索词，保持原本的语言、标签和日期等筛选条件。聊天服务还会收到最多五个候选仓库的名称、描述和语言，用于生成可选的 `answer` 字段。改写失败时会直接使用原问题检索，推荐失败时仍返回仓库卡片。

所有工具都通过官方 Rust SDK 提供。stdout 专用于 stdio 协议消息。MCP 同步请求只记录待确认状态，GitHub 下载和向量更新都在用户确认后开始。
