# 部署 StarStack

StarStack 将网页打包进 Rust 可执行文件。部署时只需一个二进制文件，或一个容器和一个持久数据卷。首次启动 Web 服务时，用 `STARSTACK_ADMIN_USER` 和 `STARSTACK_ADMIN_PASSWORD` 创建初始管理员。账号保存在数据卷中，后续启动可省略这两个变量。CLI 和 MCP 无需设置它们。

## Docker Compose

在含有 `compose.yaml` 的目录运行以下命令。示例使用 Bash，密码输入时不会回显。

```bash
export STARSTACK_ADMIN_USER=admin
read -rsp 'StarStack 管理员密码：' STARSTACK_ADMIN_PASSWORD
echo
export STARSTACK_ADMIN_PASSWORD
docker compose up -d --build
```

打开 <http://127.0.0.1:8080>，用刚刚设置的账号和密码登录。Compose 默认将网页端口绑定到宿主机回环地址。通过域名提供服务时，可在宿主机配置 HTTPS 反向代理，转发至 `127.0.0.1:8080`。

从 GHCR 运行已发布版本时，在同一目录执行以下命令。首次使用空数据卷时，先按上例设置两个管理员变量。将 `vX.Y.Z` 替换为 Releases 中已有的标签。

```bash
export STARSTACK_TAG=vX.Y.Z
docker compose pull
docker compose up -d --no-build
```

镜像地址为 `ghcr.io/ca-x/starstack`，支持 Linux amd64 和 arm64。Compose 使用命名卷 `starstack-data` 保存 `/data`，容器重建和 `docker compose down` 会保留该卷。`docker compose down -v` 会删除卷。换用宿主机目录挂载时，让容器用户 UID 10001 对该目录具有读写权限。

可在 `compose.yaml` 同目录、权限为 `0600` 的 `.env` 文件中设置环境变量，方便日后运行 Compose 命令。更新服务配置后运行 `docker compose up -d` 重新创建容器。已有账号保留原密码，重新设置初始化变量不会重置账号。

### 通过 HTTPS 域名访问

假设域名 `stars.example.com` 已解析到运行 StarStack 和 Caddy 的宿主机，且公网可访问该机器的 80、443 端口。保持 Compose 的 `127.0.0.1:8080` 映射，在 Compose 的 `.env` 中加入以下两项，并运行 `docker compose up -d`。

```dotenv
STARSTACK_SECURE_COOKIE=1
STARSTACK_MCP_HOSTS=stars.example.com
```

在宿主机的 Caddyfile 中添加此站点，替换为自己的域名并重新加载 Caddy 配置。

```caddyfile
stars.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

打开 `https://stars.example.com/login` 登录。MCP 客户端可连接 `https://stars.example.com/mcp`，并以 `Authorization: Bearer <token>` 发送当前用户在管理页面签发的 MCP 令牌。若也需要经回环地址直接连接 MCP，可设 `STARSTACK_MCP_HOSTS=stars.example.com,localhost,127.0.0.1`。反向代理应保留请求的 Host，MCP 用它核对允许的域名。

### 本地构建 Firecrawl Local 版本

默认发布的二进制文件和 GHCR 镜像使用 GitHub REST 读取 README。需要用本机解析公开仓库页面时，构建时可显式启用 Cargo feature，再到「同步与索引」为自己的账号选择 Firecrawl Local。

```bash
# Compose，沿用上文的管理员和数据卷配置
export STARSTACK_FEATURES=firecrawl-local
docker compose up -d --build

# 或者在仓库根目录构建本机可执行文件
(cd web && npm ci && npm run build)
cargo build --release --locked --features firecrawl-local
```

在 Docker CLI 中使用 `docker build --build-arg STARSTACK_FEATURES=firecrawl-local -t starstack:firecrawl .`。基础构建的 `STARSTACK_FEATURES` 为空。启用后，StarStack 读取 `github.com` 公开仓库的 HTML，在本机提取 README 并转换成 Markdown，每次至多并发处理 4 个公开仓库。私有仓库和可见性未知的仓库使用带身份认证的 GitHub REST API。抓取失败时保留旧文本和重试记录。可选依赖 `firecrawl-local` 的上游许可为 AGPL-3.0；构建或分发该组合时请核对上游条款。[Firecrawl Local crate](https://crates.io/crates/firecrawl-local/0.1.1) · [Firecrawl 上游](https://github.com/firecrawl/firecrawl)

## 二进制文件

从 GitHub Releases 下载适合当前机器的文件和同版本的 `SHA256SUMS`。Linux 提供 amd64、arm64 版本，macOS 提供 Intel、Apple Silicon 版本，Windows 提供 x64 版本。[下载 StarStack](https://github.com/ca-x/starstack/releases)

例如，在 Linux amd64 上，将下载的两个文件放到同一目录，再运行以下命令。GNU `sha256sum` 会验证当前目录中已有的发布文件。

```bash
sha256sum --check --ignore-missing SHA256SUMS
chmod +x starstack-linux-amd64
export STARSTACK_ADMIN_USER=admin
read -rsp 'StarStack 管理员密码：' STARSTACK_ADMIN_PASSWORD
echo
export STARSTACK_ADMIN_PASSWORD
export STARSTACK_HOME="$HOME/.starstack"
./starstack-linux-amd64 serve --bind 127.0.0.1:8080
```

macOS 可用 `shasum -a 256 文件名` 核对摘要，再用 `chmod +x 文件名` 添加执行权限。Windows 可在 PowerShell 使用 `Get-FileHash .\starstack-windows-amd64.exe -Algorithm SHA256` 核对摘要，设置相同的环境变量后运行 `.\starstack-windows-amd64.exe serve --bind 127.0.0.1:8080`。

在有 GitHub CLI 的本机，可运行 `gh auth login --hostname github.com` 后执行 `starstack auth login --gh`。已登录的 gh 只需第二条命令。此方式针对初始资料库，同步时需要能从该进程的 PATH 调用 gh。Compose 镜像使用下面的设备授权或环境 token。

## 环境变量

| 变量 | 用途 |
| --- | --- |
| `STARSTACK_ADMIN_USER` | 首次创建管理员使用的用户名。 |
| `STARSTACK_ADMIN_PASSWORD` | 首次创建管理员使用的密码，长度为 8 至 1024 字节。 |
| `STARSTACK_SECURE_COOKIE` | 通过 HTTPS 提供网页时设为 `1`，登录 Cookie 带 `Secure` 属性。 |
| `STARSTACK_MCP_HOSTS` | HTTP MCP 接受的主机名，以逗号分隔。本机使用时省略，沿用 loopback 默认值。 |
| `STARSTACK_HOME` | 保存 SQLite 数据库和 HawDB 向量索引的目录。容器使用 `/data`。 |
| `STARSTACK_GITHUB_CLIENT_ID` | 使用 GitHub 设备授权登录时设置为已启用 Device Flow 的 GitHub OAuth App 的 Client ID。此流程无需 client secret。 |
| `STARSTACK_GITHUB_SCOPE` | GitHub 设备授权请求的 OAuth scope，默认留空。需要访问私有仓库时可显式设为 `repo`。 |
| `STARSTACK_PROXY_URL` | 可选的实例级 GitHub 出站代理，支持 HTTP、HTTPS、SOCKS5、SOCKS5h。非空值优先于管理后台配置。 |
| `STARSTACK_README_INTERVAL_MINUTES` | 用户未保存自定义间隔时，后台补全待处理 README 的默认间隔，单位为分钟，默认 30，范围 0 至 10080。设为 0 暂停此后台任务。 |
| `STARSTACK_SYNC_INTERVAL_MINUTES` | Web 服务后台同步的间隔，单位为分钟，默认 360。设为 0 可关闭定时同步。每轮核对完整收藏列表，并针对变更重新计算向量。 |
| `GITHUB_TOKEN` 或 `GH_TOKEN` | 初始管理员用于 GitHub 同步的令牌，其他用户需分别授权。Compose 优先采用 `GITHUB_TOKEN`，其次采用 `GH_TOKEN`。 |
| `STARSTACK_OLLAMA_URL` | 由你管理的 Ollama 服务的基址，例如本机使用 `http://127.0.0.1:11434`。Compose 默认为 `http://host.docker.internal:11434`。 |
| `STARSTACK_MODEL` | Ollama 嵌入模型名称，默认 `qwen3-embedding:0.6b`。 |
| `STARSTACK_AI_PROVIDER` | Aimux 对话服务商 ID，默认 `ollama`。 |
| `STARSTACK_AI_MODEL` | 对话模型。Ollama 默认 `qwen3:4b`，选择其他服务商时必须设置。 |
| `STARSTACK_AI_BASE_URL` | 对话 API 基址，可选。Ollama 默认使用 `STARSTACK_OLLAMA_URL` 加 `/v1`。 |
| `STARSTACK_AI_API_KEY` | 远程对话服务所需的密钥。本地 Ollama 无需密钥。 |
| `STARSTACK_TAG` | Compose 拉取的 GHCR 镜像标签，默认为 `latest`。 |

使用语义搜索时，在运行 Ollama 的机器上执行 `ollama pull qwen3-embedding:0.6b`，或拉取 `STARSTACK_MODEL` 指定的模型。容器内的 `127.0.0.1` 指向容器本身。Linux 上使用宿主机的 Ollama 时，请让该服务监听 Docker 网桥可达的宿主机地址，并按需限制访问。若 Ollama 与 StarStack 位于同一 Compose 网络，可把 `STARSTACK_OLLAMA_URL` 设为 `http://ollama:11434`，其中 `ollama` 为服务名。

### 大收藏库、GitHub 分类与 README 配额

首次运行 `starstack sync --catalog-only` 会按每页 100 个收藏读取 GitHub 的 REST Stars 列表，并通过 GraphQL 镜像用户在 GitHub 上维护的 Lists。此命令先保存仓库目录并记录待补抓的 README，完成后可浏览和筛选。需要语义检索时运行 `starstack index`。后续目录更新默认从游标增量读取；如要核对完整成员关系和取消收藏的仓库，请运行 `starstack sync --catalog-only --full`。

```bash
starstack lists --refresh
starstack list --list "Dev tools"
starstack search '模糊搜索' --list "Dev tools"
```

`Dev tools` 应替换成自己的 GitHub 分类名。星栈的私人标签另存于 SQLite，可用 `--tag` 与分类一起筛选。MCP 的 `search_stars` 使用 JSON 字段 `list` 和 `tag`。GitHub 分类服务暂时不可用时，已存储的分类和仓库数据仍可使用。[GitHub Lists](https://docs.github.com/en/get-started/exploring-projects-on-github/saving-repositories-with-stars) · [MCP 工具](mcp.md)

Web 服务先提交目录，再由独立任务按用户预算补抓 README。默认目录周期为 360 分钟，README 周期为 30 分钟，每轮 README 预算为 200 次。将 README 预算设为 0 可暂停抓取，将某一后台周期设为 0 可关闭对应定时任务。CLI 的 `starstack sync` 会在前台处理一批 README，再索引已有内容。应用退出后，未完成的仓库随下次任务继续；尚未补齐时仍可使用上次保存的 README 和个人批注。

GitHub 通过响应头分别报告 REST 和 GraphQL 的额度上限、剩余量和重置时间。星栈按上游账号和资源记录实际窗口，同时保存每个星栈用户自己的预算与预留比例（默认 5%）。达到预留值时，暂停非必要的 REST README 请求，剩余额度可供目录使用。额度耗尽或收到冷却指示时，按 `Retry-After` 或重置时间等待。多台服务和其他工具使用同一 GitHub 身份时也会消耗该账户的配额。[REST 限额](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api) · [GraphQL 限额](https://docs.github.com/en/graphql/overview/rate-limits-and-query-limits-for-the-graphql-api)

REST 分页支持 gzip 传输。一次无凭据的公开样本中，100 条 Stars 的 JSON 解压后为 655,421 字节，gzip 响应传输了 52,193 字节。压缩可减少慢连接上的下载量，每页仍计一次 API 请求。只导入目录还可以省下每个新仓库一次 README 请求的成本。[公开分页样本](https://api.github.com/users/sindresorhus/starred?per_page=100&page=1)

### 配置 GitHub 出站代理

实例管理员可以在 **用户管理 → 实例代理** 设置所有账号共用的连接方式。GitHub 设备授权、收藏及分类同步、README 抓取使用同一代理。保存的设置用于后续新建的连接。**跟随系统代理** 使用服务进程的默认代理环境，包括 `HTTPS_PROXY`、`ALL_PROXY` 和 `NO_PROXY`。**直接连接** 绕过系统代理。**指定代理** 支持 `http://`、`https://`、`socks5://` 和 `socks5h://`，其中 `socks5h` 将目标域名交由代理解析。

也可在 Compose 的 `.env` 中设置 `STARSTACK_PROXY_URL=socks5h://host.docker.internal:1080` 后重新创建容器。该变量非空时覆盖数据库中的设置，后台显示为只读。将它取消并重新启动服务后，恢复数据库中的配置。尚未配置的实例默认跟随系统代理。容器访问宿主机代理时，代理需监听容器可达的地址。需要认证时可使用 `http://用户名:密码@代理主机:端口`，凭据中含 `@`、`:` 等保留字符时请按 URL 百分号编码。后台只回显主机和端口，替换地址时需要重新输入认证信息。使用后台保存时，代理地址及凭据保存在实例 SQLite 文件中，因此数据库及其备份应当与部署凭据同等保护。

设备授权时，可在任意能访问 GitHub 的浏览器中打开验证地址并输入短码，例如使用手机。服务端代理负责请求设备码、轮询授权结果和之后的同步。浏览器访问验证页时使用该设备自己的网络设置。

每个账号可在 **同步与索引 → 同步设置** 分别调整收藏同步和 README 后台补全间隔，调整其中一项不会更改另一项。

### 配置 GitHub 设备授权

1. 登录 GitHub，在 **Settings → Developer settings → OAuth Apps** 选择 **New OAuth App**。填写应用名称和 StarStack 的主页 URL。GitHub 注册表单要求填写 Authorization callback URL，可以填写同一主页地址。Device Flow 使用设备代码完成授权。
2. 注册后进入该 OAuth App 的设置，勾选 **Enable Device Flow** 并保存。复制页面上的 **Client ID**。[GitHub OAuth App 设置](https://github.com/settings/developers)
3. 将下面两行加入 Compose 的 `.env` 文件，把第一行的占位符换成自己的 Client ID。已启动的实例运行 `docker compose up -d` 加载配置。

```dotenv
STARSTACK_GITHUB_CLIENT_ID=替换为你的GitHub_OAuth_App_Client_ID
STARSTACK_GITHUB_SCOPE=
```

每位用户分别登录自己的 StarStack 账号，发起 GitHub 设备授权，按照页面显示的验证地址和一次性代码在 GitHub 确认。默认留空的 scope 可读取公开仓库。如果确实需要访问私有仓库，可以将 `STARSTACK_GITHUB_SCOPE` 改为 `repo` 并重新授权。GitHub 的经典 `repo` scope 包含私库读写等广泛权限。初始管理员可通过 `GITHUB_TOKEN` 提供 fine-grained PAT，以限制授权范围。创建令牌时，仅选择所需仓库，并授予只读权限。Stars 列表需要账户的 **Starring: Read-only**，读取私有仓库的 README 还需所选仓库的 **Contents: Read-only**。[GitHub 设备授权](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#device-flow) · [Stars API 权限](https://docs.github.com/en/rest/activity/starring#list-repositories-starred-by-the-authenticated-user)

### 使用 Aimux 问答

StarStack 默认将问答发给本机 Ollama。在 Ollama 所在的机器执行 `ollama pull qwen3:4b`，即可与上面的嵌入模型配合使用。选择 AI 问答时，StarStack 发送问题原文，以及当前账号筛选出的至多五个候选仓库的名称、摘要（最多 1024 个字符）、语言和来源编号。答案里的 `[1]` 等编号对应候选列表。私人标签、备注、README、GitHub 令牌和管理员凭据均不在对话请求中。

若显式选择远程对话服务，该服务会收到上述问题和元数据，其中可能含私有仓库名称。以下示例选择 OpenAI；在 Bash 中输入密钥后，重新创建 Compose 容器或重新启动二进制服务即可生效。模型 ID 需在服务商账号中可用。[Aimux 官方仓库](https://github.com/arcships/aimux)

```bash
export STARSTACK_AI_PROVIDER=openai
export STARSTACK_AI_MODEL=gpt-4.1-mini
read -rsp '对话服务密钥：' STARSTACK_AI_API_KEY
echo
export STARSTACK_AI_API_KEY
```

向量索引和生成回答使用独立的模型。索引请求会把仓库文本及个人整理内容提交给 `STARSTACK_OLLAMA_URL` 指向的嵌入服务，请将该地址设为自己的实例。

备份前停止写入，以便数据库和索引处于一致状态。本机部署复制整个 `STARSTACK_HOME` 目录。Compose 部署可先执行 `docker compose stop starstack`，再执行 `docker compose cp starstack:/data/. ./starstack-backup`，最后执行 `docker compose start starstack`。

## GitHub Actions

- 分支 push 和 pull request 会运行 CI，使用 Node 24 构建网页，再检查 Rust 格式并执行包含 Firecrawl Local 的 `cargo test --locked --all-targets --all-features`。另一项任务构建 Linux 容器，启动 Web 服务并检查登录页和数据卷的写权限。
- 推送 `v*` 标签会启动发布流程，编译各平台的二进制文件，并推送带相同标签的双架构镜像。没有连字符的标签会同时更新 `latest`。
- 所有平台和镜像构建成功后，工作流创建 GitHub Release，附上二进制文件和 `SHA256SUMS`。含连字符的标签（如 `v1.0.0-rc.1`）会标为预发布。
- 发布使用 GitHub Actions 自带的 `GITHUB_TOKEN`。镜像任务需要仓库包的写权限，Release 任务需要仓库内容的写权限。
