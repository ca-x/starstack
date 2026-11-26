<p align="center"><img src="web/src/assets/logo-display.webp" width="96" alt="StarStack"></p>

# StarStack

[简体中文](README.md)

Ask “What was that Rust parser I starred last year?” StarStack searches your own GitHub stars and returns repositories with their links, star dates, private tags and notes. The command line, web dashboard and MCP server ship in one Rust executable.

## First search

1. Build the executable with Rust 1.97.1 or newer and Node.js 24. Run these commands from the repository root. The result is `target/release/starstack`, with the dashboard embedded. Put it on PATH to use the commands below.

   ```bash
   (cd web && npm ci && npm run build)
   cargo build --release --locked
   ```

2. Start a local Ollama service and download the multilingual embedding model.

   ```bash
   ollama pull qwen3-embedding:0.6b
   ```

3. Connect your GitHub account. If GitHub CLI is already signed in, run **`starstack auth login --gh`**. StarStack remembers the credential source and reads the token from `gh` when syncing. To use Device Flow instead, register a GitHub OAuth App with Device Flow enabled, set its client ID, then enter the displayed code at GitHub. The server can reach GitHub through an HTTP or SOCKS5 proxy configured with `STARSTACK_PROXY_URL`. You can enter the device code in any browser with access to GitHub. Alternatively, supply a personal access token through `GITHUB_TOKEN`. Leave the OAuth scope empty for public repositories. For a fine-grained token, grant read access to account **Starring** and to repository **Contents** for private READMEs.

   ```bash
   export STARSTACK_GITHUB_CLIENT_ID='your OAuth app client ID'
   starstack auth login
   ```

4. Sync your collection and ask a question. The first sync saves the repository catalog, fetches a budgeted batch of READMEs, then builds the semantic index.

   ```bash
   starstack sync
   starstack search 'a Go library for local vector search'
   ```

## Dashboard and Docker

On first launch, set `STARSTACK_ADMIN_USER` and `STARSTACK_ADMIN_PASSWORD` (at least eight bytes). Run `starstack serve` and open <http://127.0.0.1:8080/login>. **Sync & index** connects your GitHub account and manages refreshes. Search, filter and edit notes from the library. Display preferences on the login page and dashboard control language and theme.

For Docker, put the following variables in a local `.env` next to `compose.yaml`, replace the placeholders, then run `docker compose up -d --build`. The named volume persists the library across container recreation. Ollama on the host must be reachable from the container. The default published port binds to localhost.

```dotenv
STARSTACK_ADMIN_USER=admin
STARSTACK_ADMIN_PASSWORD=choose-a-long-unique-password
STARSTACK_GITHUB_CLIENT_ID=your-device-flow-enabled-client-id
STARSTACK_GITHUB_SCOPE=
STARSTACK_OLLAMA_URL=http://host.docker.internal:11434
STARSTACK_MODEL=qwen3-embedding:0.6b
STARSTACK_SYNC_INTERVAL_MINUTES=360
```

Admins can create and disable users, reset passwords and change roles. Each signed-in user has an independent collection, GitHub authorization, sync schedule and MCP tokens in the shared SQLite database. Password and role changes invalidate that user's sessions and MCP tokens. The local CLI and stdio transport use the initial administrator's library. For other users, connect over HTTP using the token issued by their account. [Deployment guide](docs/deploy.md)

## GitHub Star Lists

StarStack mirrors the lists you maintain on GitHub, including their names and member repositories, through the GraphQL API. Run `starstack lists --refresh` to inspect yours. Private tags are stored in your local StarStack library. A repository can belong to multiple GitHub lists and also carry private tags. [GitHub Stars and Lists](https://docs.github.com/en/get-started/exploring-projects-on-github/saving-repositories-with-stars)

```bash
starstack lists
starstack list --list "Dev tools"
starstack search 'fuzzy search on the command line' --list "Dev tools"
```

List counts come from GitHub. Filtered results include only repositories in the current account’s active star catalog.

Replace `Dev tools` with an existing list name. The MCP tool `search_stars` accepts the equivalent JSON field, for example `{"query":"fuzzy search on the command line","list":"Dev tools"}`.

## Organize and export

Replace the repository name with one of your stars.

```bash
starstack list --sort stars --language Rust --archived false
starstack tag clap-rs/clap cli
starstack note clap-rs/clap 'Use this for the next release tool'
starstack pin clap-rs/clap
starstack index
starstack search 'command line parsing' --tag cli
starstack export --pinned true --output shortlist.md
starstack export --format json --output stars.json
```

Edits appear immediately in the list. The next search refreshes stale annotation embeddings. Run the incremental indexer to update them in advance. Shared exports include the matching private annotations. [Markdown example](examples/collection.md) · [JSON example](examples/collection.json)

## Ask an assistant

- **In StarStack**, use AI recommendations in the dashboard or run `starstack ask 'Which CLI library from my stars should I use?'`. Download the local chat model with `ollama pull qwen3:4b` first. Aimux can also connect to an explicitly configured remote provider using `STARSTACK_AI_PROVIDER`, `STARSTACK_AI_MODEL`, `STARSTACK_AI_BASE_URL` and `STARSTACK_AI_API_KEY`. The question plus the names, descriptions and languages of up to five candidates go to that provider. Numbered references correspond to the result links.
- **In Claude Desktop**, add the example configuration to your MCP settings, replacing the binary and data paths. The official Rust SDK also serves Streamable HTTP at `/mcp`. Generate a token in the dashboard and send `Authorization: Bearer <StarStack token>` with requests. For a remote HTTPS endpoint, configure your domain in `STARSTACK_MCP_HOSTS`. The tools expose semantic search, repository details, tags, stats and a sync request. Confirm the latter in the dashboard before the sync runs. [Claude Desktop configuration](examples/claude_desktop_config.json) · [MCP tool reference](docs/mcp.md)

## Updates and recovery

- Use `starstack sync --catalog-only` for a large initial import. It pages through stars, saves repository metadata and syncs GitHub lists. You can browse immediately or run `starstack index` to search the available text. Subsequent catalog runs are incremental. Add `--full` to read all pages again.
- Run `sync` for recent stars or `sync --full` to refresh older metadata and reconcile unstars. Personal notes survive an unstar and reappear if you star the same repository again.
- The dashboard schedules a full catalog refresh every six hours and processes queued READMEs in the background every 30 minutes, updating embeddings for changed text. Each user can adjust both intervals in **Sync & index**. Set either to zero to disable that job.
- The semantic index includes the repository name, description, language, topics, license, README and private annotations. Set `STARSTACK_MODEL` and run `starstack index --rebuild` after a model change or to recover a damaged index.
- StarStack tracks REST and GraphQL rate windows separately for each upstream account. The default policy reserves 5% of the allowance and budgets 200 README requests per run. Catalog requests can use the reserve. A rate-limited or failed README stays in the queue, preserving the previous text and private annotations. Follow the retry time in the summary. Reauthorize an expired credential. When Ollama is unavailable, synced metadata is retained. Start Ollama and run `starstack index`.
- Stop writers before backing up the entire `STARSTACK_HOME`. The directory includes SQLite, the HawDB projections and the GitHub device credential. Keep the backup private.

The CLI processes one README batch in the foreground. Dashboard users can set their own batch budget, reserve and fetch method. To parse public GitHub pages using Firecrawl Local, build with `cargo build --release --locked --features firecrawl-local` or `STARSTACK_FEATURES=firecrawl-local docker compose up -d --build`. The release workflow builds the base configuration. The optional `firecrawl-local` crate uses the AGPL-3.0 license. [Deployment](docs/deploy.md) · [Firecrawl Local](https://crates.io/crates/firecrawl-local/0.1.1)

## Screenshots

These screenshots show a separate example library. The repository names refer to public projects. Star counts, saved dates, tags and notes are sample data. Select an image to see it at full size.

### Desktop · 1440 × 900

#### Light

| Library | Repository details | Collection statistics |
| --- | --- | --- |
| [![Desktop · Light · Library](docs/screenshots/en-desktop-light-library.webp)](docs/screenshots/en-desktop-light-library.webp) | [![Desktop · Light · Repository details](docs/screenshots/en-desktop-light-detail.webp)](docs/screenshots/en-desktop-light-detail.webp) | [![Desktop · Light · Collection statistics](docs/screenshots/en-desktop-light-statistics.webp)](docs/screenshots/en-desktop-light-statistics.webp) |

#### Dark

| Library | Repository details | Collection statistics |
| --- | --- | --- |
| [![Desktop · Dark · Library](docs/screenshots/en-desktop-dark-library.webp)](docs/screenshots/en-desktop-dark-library.webp) | [![Desktop · Dark · Repository details](docs/screenshots/en-desktop-dark-detail.webp)](docs/screenshots/en-desktop-dark-detail.webp) | [![Desktop · Dark · Collection statistics](docs/screenshots/en-desktop-dark-statistics.webp)](docs/screenshots/en-desktop-dark-statistics.webp) |


### Mobile · 390 × 844

#### Light

| Library | Repository details | Collection statistics |
| --- | --- | --- |
| [![Mobile · Light · Library](docs/screenshots/en-mobile-light-library.webp)](docs/screenshots/en-mobile-light-library.webp) | [![Mobile · Light · Repository details](docs/screenshots/en-mobile-light-detail.webp)](docs/screenshots/en-mobile-light-detail.webp) | [![Mobile · Light · Collection statistics](docs/screenshots/en-mobile-light-statistics.webp)](docs/screenshots/en-mobile-light-statistics.webp) |

#### Dark

| Library | Repository details | Collection statistics |
| --- | --- | --- |
| [![Mobile · Dark · Library](docs/screenshots/en-mobile-dark-library.webp)](docs/screenshots/en-mobile-dark-library.webp) | [![Mobile · Dark · Repository details](docs/screenshots/en-mobile-dark-detail.webp)](docs/screenshots/en-mobile-dark-detail.webp) | [![Mobile · Dark · Collection statistics](docs/screenshots/en-mobile-dark-statistics.webp)](docs/screenshots/en-mobile-dark-statistics.webp) |
