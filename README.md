# Venticode

Venticode is a coding and bot agent that runs independently or as part of the
Ventic distribution. It works
like Claude Code in the browser: connected ventiharness machines appear in an
inventory on the left, each with project folders and their conversations. The
agent runs files, commands, terminals, sub-agents and MCP tools on the harness,
asking for permission per tool call. The same agent can answer `@ventic`
mentions in Slack, Telegram and GitLab, holding any write for review.

The browser uses the Rust service in an installation and a loopback-only
synthetic API during UI development. In Ventic mode, Ventic control authorizes
the workspace and relays member-scoped requests. In standalone mode, Venticode
issues its own sessions, connects directly to its runner, and calls the LLM API.

## Layout

| Path | Contents |
|---|---|
| `api/openapi.yaml` | Browser API contract (source of truth) |
| `web/` | Ventic workspace shell and WebAssembly terminal; `web/src/api/schema.ts` is generated |
| `server/` | Rust HTTP service, PostgreSQL schema and encrypted S3 event storage |
| `harness/` | `venticode-harness`, the native remote runner for project tools, terminals, sub-agents and MCP |
| `connector/` | Apache Camel loopback transport for Slack, Telegram and GitLab |
| `dev/mock.ts` | Synthetic in-memory API used for development and tests |

## Standalone installation

A standalone instance serves one workspace and one operator. Build the service
and runner with `cargo build --workspace --release --locked` and the browser
with `make setup && make build`. The service applies its PostgreSQL migrations
on startup. Create a PostgreSQL database and a private S3 bucket first.

Set the following environment variables on the **service**:

| Variable | Value |
|---|---|
| `VENTICODE_MODE` | `standalone` |
| `VENTICODE_ORIGIN` | Browser-visible origin, such as `https://code.example.org` or `http://127.0.0.1:4189` for local development |
| `LISTEN` | Bind address, default `127.0.0.1:4189` |
| `VENTICODE_WORKSPACE_ID`, `VENTICODE_USER_ID` | Stable UUIDs allocated once for this instance |
| `VENTICODE_LOGIN_TOKEN`, `VENTICODE_HARNESS_TOKEN` | Separate random tokens of at least 32 characters, generated with a command such as `openssl rand -hex 32` |
| `DATABASE_URL` | PostgreSQL connection string |
| `S3_BUCKET`, `AWS_REGION` | Private transcript bucket and its region |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | S3 credentials, or use the AWS identity available to the service |
| `S3_ENDPOINT` | Optional URL for an S3-compatible endpoint. Use `AWS_ALLOW_HTTP=true` only for a disposable HTTP endpoint |
| `VENTICODE_CONTENT_KEY` | Stable Base64-encoded 32-byte key, generated with `openssl rand -base64 32` |
| `VENTICODE_LLM_PROVIDER`, `VENTICODE_MODEL` | `openai` or `anthropic`, and the upstream model identifier |
| `VENTICODE_LLM_BASE_URL`, `VENTICODE_LLM_API_KEY` | API base ending in `/v1` and provider credential. The official base is used when the URL is omitted. A custom OpenAI-compatible endpoint can omit the key |
| `VENTICODE_LLM_TOOLS`, `VENTICODE_LLM_MAX_TOKENS` | Optional `false` for a model without tools, and the Anthropic output-token limit (default `4096`) |
| `WEB_ROOT` | Browser build directory, default `web/dist` |

Run `target/release/venticode`. For HTTPS, terminate TLS at a reverse proxy that
preserves the external `Host`, supports WebSocket upgrades and streams SSE without
buffering. Keep the content key alongside database and bucket backups. The key
encrypts transcripts and seals standalone bot credentials.

On the machine that has the project files, run the runner as its own OS user,
with access to the projects and any command-line tools it will use. Set
`VENTICODE_SERVER_URL` to the service origin, `VENTICODE_HARNESS_TOKEN` to the
shared runner token, and `VENTICODE_HARNESS_ROOTS` to a JSON object of root labels
and absolute paths, for example `{"work":"/home/coder/projects"}`. Then run
`target/release/venticode-harness run`. A single runner connects to the service
over WebSocket and reconnects after a transport loss. Native commands run with
the runner user's OS privileges. Install any sub-agent executables and MCP
runtimes on the runner's `PATH`. For MCP, configure the servers with
`venticode-harness mcp` and set `VENTICODE_HARNESS_MCP_GRANTS` on the runner to
the JSON mapping of approved server IDs to revisions.

Open the service origin and sign in with the access token. Add a project from
the runner inventory. The terminal client can use the same token via
`venticode-tui --standalone https://code.example.org`.

To use Slack, Telegram or GitLab bots, run the Camel connector in the same host
or network namespace as the service, listening on `127.0.0.1:4190`. Build it
with `mvn -B -f connector/pom.xml package` or the separate connector image.
Open **Bot connections** to add credentials, finish Slack OAuth if applicable,
and link the provider identity. A provider-reachable HTTPS origin is required
for external webhooks and OAuth. The Slack app uses `/oauth/slack` at that
origin and the webhook URL shown in the connection. The connector uses
`https://gitlab.com` by default. Set `GITLAB_ORIGIN` on the connector for a
self-managed GitLab instance. Bot replies and tool approvals continue through
the workspace review flow.

## Development

Requires Node.js 24 or newer and the WebAssembly build tools described under **Browser frontend**.

```sh
make setup   # install web dependencies
make dev     # mock API on :4189, app with HMR on http://127.0.0.1:4175
```

Open http://127.0.0.1:4175 and choose **Accesso di sviluppo**. The mock seeds
three harnesses (one offline), project folders, a finished conversation and an
`@ventic` bot with one reply held for review. Every run is scripted: it lists
and reads files, asks to edit (in "Ask before edits" mode), asks to run
`npm test`, then reports. No model or harness is contacted. State resets when
the mock restarts.

Other targets: `make generate` (types from the contract), `make test`,
`make build`, `make check` (all of them; run it before reporting work).

## Terminal client

From `tui/`, run `cargo run --release --locked` and enter the HTTPS URL of
your Ventic portal. Add `-- --en` for English. Rustup selects the nightly
pinned in that directory for the FrankenTUI runtime. Prompts, tools and
streamed responses use the authenticated remote Venticode workspace.

At startup, select a project with the arrow keys if prompted. The first
message creates a conversation. F2 lists conversations in the selected
project, and F3 switches projects. If there are no projects, selecting a
granted harness folder creates the first one.

### Browser frontend

The workspace homepage, project pages and conversation links use the same
FrankenTUI WebAssembly renderer as the loopback preview. Workspace navigation
and accessible browser controls use Ventic design-system 1.3.1 tokens and
self-hosted fonts from ventic-frontend revision
`ad0bb2d94c70cfa5e37bbdc5853aaaa2450427f3`. The terminal retains its own palette.
The browser follows the Venticode handoff and transcript protocol in
ventic-common and uses this repository's OpenAPI contract.

For local frontend builds, run `make setup`. From `tui/browser/`, run
`rustup target add wasm32-unknown-unknown` and
`cargo install wasm-bindgen-cli --version 0.2.129 --locked`.
The web build generates the bindings and bundles the WebAssembly module.
The Docker build prepares its own compiler and bindings generator.

Run `make demo-tui-web` to serve the compiled workspace UI with the
loopback-only synthetic API. Open `http://127.0.0.1:4287/__dev/login`
for a demo session. Stopping the server discards demo conversations.

## API summary

All routes are under `/api/v1/venticode` on the workspace host, except the
ticket exchange.

- Session: `POST /access` (portal host), `POST /venticode/bootstrap`,
  `GET|POST /venticode/login` (standalone sign-in),
  `GET|DELETE /session`, `GET /models`
- Inventory: `GET /harnesses`, `GET|POST /projects`, `DELETE /projects/{id}`
- Conversations: `GET|POST /conversations`, `GET|PATCH|DELETE /conversations/{id}`,
  `GET /conversations/{id}/events`, `GET /conversations/{id}/stream` (SSE),
  `POST /conversations/{id}/messages` (idempotent on the message id),
  `POST /conversations/{id}/interrupt`, `POST /approvals/{id}`,
  `POST /conversations/{id}/sessions/{call_id}/input` for live terminal or agent input
- Bots: `GET /bots`, `GET|PATCH /bots/{id}`, `GET /bots/{id}/mentions`,
  `POST /bots/{id}/mentions/{mention_id}/review`
- Connections: `GET|POST /connections`, credential rotation and disconnect,
  and member identity linking and unlinking under `/connections/{id}`

Permission modes: `ask` (writes and commands ask), `auto_edit` (file edits run,
commands ask), `plan` (read-only tools). An owner using a YOLO harness can skip prompts in write-enabled modes. Bot runs follow their connection review policy.

## Ventic integration

Ventic integration is the default (`VENTICODE_MODE=ventic`). The distribution
chart enables the service with `venticode.enabled`. The
standalone image includes the browser. A separate connector image runs in
the same pod and listens on loopback. Use the
[installation guide](https://gitlab.com/nettersrl/ventic/ventic-frontend/-/blob/main/docs/venticode.md)
for DNS, TLS, PostgreSQL, S3, feature policy and connection setup.

Run `make check-service` to format-check, lint, test and build the release
binary. Run `make check-connector` to package Camel and exercise it with a
disposable loopback HTTPS provider. The synthetic mock never sends messages
to any external provider.
