# Rust 实现与行为对照

星栈在空仓库中按产品需求开发。没有前序实现、旧数据格式或原语言测试可做逐项迁移。这里记录可从外部观察的行为，方便后续实现核对兼容性。

| 接口 | 行为 | 检查位置 |
| --- | --- | --- |
| GitHub Stars → CLI / Web 同步 | 以稳定仓库 ID 合并，同秒收藏都保留。完整对账确认删除后隐藏 unstar 项。局部失败保留可用页面及批注，游标留待重试。 | [同步测试](src/sync/tests.rs) |
| 自然语言 → 仓库卡片 | 使用真实 embedding 接口和 HawDB，按当前用户和筛选条件选择候选，再做余弦精排。结果附带私人批注。 | [检索测试](src/index/tests.rs) |
| 模型 / 索引切换 | 模型身份、维度或索引完整性变化有可操作提示。重建结束后发布新一代投影。 | [索引实现](src/index.rs) |
| GitHub Device Flow → 用户凭证 | 尊重轮询间隔、过期与 slow_down，临时网络故障保留授权请求。 | [授权实现与测试](src/auth.rs) |
| MCP stdio / HTTP → 工具结果 | 使用官方 Rust SDK。HTTP 凭证绑定用户；同步需另行确认，改密后旧会话及 token 失效。 | [二进制接口测试](tests/interfaces.rs) |
| 本机语义检索 → AI 推荐 | Aimux 获得最多五个候选的有限字段，并按编号引用来源。 | [Aimux 测试](src/ai.rs) |
| 收藏 → Markdown / JSON | 两种导出使用同一批筛选结果，保留可分享的链接、标签及备注。 | [导出样本](examples/collection.md) |

开发检查使用 `npm ci`、`npm run build`、`cargo fmt --all -- --check`、`cargo clippy --locked --all-targets --all-features -- -D warnings`、`cargo test --locked --all-targets` 和 `cargo build --release --locked`。浏览器截图来自独立示例库，详见两份 README。

GitHub Lists 单独记录分类 ID、名称和成员关系。列表改名和成员移除以 GitHub 最近一次完整快照为准；私人标签仍由用户维护。多用户共享同一个上游 GitHub 账号时，共享该账号同一资源的限额观测和冷却。各自的预留比例、每轮预算、补全周期仍独立。目录写入和 README 待办在同一事务提交，补抓在事务外进行。进程中断后按队列继续，完成的向量投影按代号发布。
