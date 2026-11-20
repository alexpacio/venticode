# Venticode

Venticode is a coding and bot agent that runs independently or as part of the
Ventic distribution. It looks and works like Claude Code, in the terminal and
in the browser: one terminal UI, built natively and as WebAssembly. Machines
running venticode-harness appear in a tree on the left, each with project
folders and their conversations. The
agent runs files, commands, terminals, sub-agents and MCP tools on the harness,
asking for permission per tool call. The same agent can answer `@ventic`
mentions in Slack, Telegram and GitLab, holding any write for review.

The browser uses the Rust service in an installation and a loopback-only
synthetic API during UI development. In Ventic mode, Ventic control authorizes
the workspace and relays member-scoped requests. In standalone mode, Venticode
issues its own sessions, connects directly to its runner, and calls the LLM API.

## License

The [Venticode Source-Available License 1.0](LICENSE) allows self-hosting for
internal business use. Keep the original logo intact and obtain written
permission before distributing source or builds to third parties. Submit any
production-bound changes through a pull request to this repository before
deploying them. A maintainer's merge is optional.

## Layout

| Path | Contents |
|---|---|
| `api/openapi.yaml` | Browser API contract (source of truth) |
| `tui/` | `venticode-tui`: the terminal UI core (`tui/src`) and its console client |
| `tui/browser/` | WebAssembly build of the same core for the browser |
| `web/` | Page that hosts the WebAssembly terminal; `web/src/api/schema.ts` is generated |
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

## Kubernetes computers

Bots can use an automatically enrolled Ubuntu desktop with Helium and
`venticode-cli`, or a disposable Kitesurf browser. See the
[computer build and installation guide](computer/README.md) for runtime,
storage, networking, and Kubernetes verification.

## Development

Requires Node.js 24 or newer and the WebAssembly build tools described under **Browser frontend**.

```sh
make setup   # install web dependencies
make dev     # mock API on :4189, app with HMR on http://127.0.0.1:4175
```

Open http://127.0.0.1:4175 and press `d` for the development sign-in. The mock seeds
three harnesses (one offline), project folders, a finished conversation and an
`@ventic` bot with one reply held for review. Every run is scripted: it lists
and reads files, asks to edit (in "Ask before edits" mode), asks to run
`npm test`, then reports. No model or harness is contacted. State resets when
the mock restarts.

Other targets: `make generate` (types from the contract), `make test`,
`make build`, `make check` (all of them; run it before reporting work).

## Terminal client

The same terminal UI runs in a console and in the browser. Its core
(`tui/src/app.rs`, `nav.rs`, `screens.rs`, `view.rs`) holds every screen and
API call. Only the transport differs: in the console, asupersync HTTP, SSE and
WebSockets; in the browser, fetch, EventSource and WebSocket.

From `tui/`, run `cargo run --release --locked` and enter the HTTPS URL of
your Ventic portal. Add `-- --en` for English. Rustup selects the nightly
pinned in that directory for the FrankenTUI runtime.

The layout follows Claude Code. Each conversation opens with a welcome box.
Replies show behind `●`, tools as `Bash(cargo test)` with `⎿` results and
diffs, and the prompt sits in a rounded box. Permission requests are numbered
prompts: 1 allow, 2 allow for the project, 3 or Esc deny. Questions and
finished plans are numbered prompts too. Shift+Tab cycles the permission mode
(ask, accept edits, plan). `/` opens the command menu, `?` lists shortcuts,
and Esc interrupts a run.

The left pane is a tree with these items:

- Remote browser, remote desktop and bot connections.
- Projects with their conversations, and conversations without a project.
- Harnesses with their granted folders and files.
- Bots with their mentions.

The right pane shows details of the highlighted item. Otherwise it shows the
session: context use, todos, changed files, and the harness with its MCP
servers. Tab moves focus from the prompt to the tree and then to the screen.
F2 jumps to the current project, F3 to the harnesses, and F4 shows or hides
the panes. Ctrl+B collapses or expands the tree and Ctrl+Alt+B the right pane;
clicking a pane's divider (the `‹`/`›` at its top) does the same. In the tree, use the arrow keys (or h/j/k/l) to move, Enter to
open, `/` to search conversations, and these keys:

- `n`: start a conversation in a project, or create a project on a folder.
- `a` on a harness: add a project with a root, subfolder and name.
- `d`: delete a project or conversation (asks first).
- `o` on a harness: open the portal's harness page.
- `e`: pause or resume a bot.
- `a` or `x` on a mention: approve or discard a reply held for review.
- `r`: reload.

The right pane appears at 130 columns or more. Below 90 columns the tree
uses the full screen while it has focus.

Opening a bot shows its mentions and, for administrators, its settings:
state, environment, triggers, tool policies (←→ to change), instructions and
reply limit. **Bot connections** lists Slack, Telegram and GitLab bindings.
From there you can link or unlink your account, replace credentials,
disconnect, or add a connection.

### Browser frontend

The browser shows the WebAssembly build of the terminal UI directly. The page
(`web/index.html`, `web/src/main.ts`) only paints the cells and forwards keys,
paste, mouse and resize events. Sign-in, deep links (`/c/<id>`, `/p/<id>`,
`/bots/<id>`, `/connections`, `/browser`, `/computer`) and sign-out are
handled by the same core. The grid self-hosts JetBrains Mono plus a small Noto
symbol fallback (regenerate it with `web/scripts/build-symbols-font.py`). The
palette mirrors the `Ansi256` constants in `tui/src/view.rs`.
`web/src/palette.test.ts` and `web/src/font.test.ts` pin both.

For local frontend builds, run `make setup`. Then run
`rustup target add wasm32-unknown-unknown` and
`cargo install wasm-bindgen-cli --version 0.2.129 --locked`.
The web build generates the bindings and bundles the WebAssembly module.
The Docker build prepares its own compiler and bindings generator.

Run `make demo-tui-web` to serve the compiled app with the loopback-only
synthetic API, open `http://127.0.0.1:4287` and press Enter to sign in.
Stopping the server discards demo conversations.

### Remote browser and desktop

In a Ventic workspace with the Kitesurf pilot enabled, open **Remote browser**
in the tree and press Enter to start Kitesurf. Before starting, ←→ picks a
saved conversation. The active tab streams as Motion JPEG over the workspace
WebSocket. In the browser the real frames are drawn over the terminal cells.
In a console they are drawn as colored half blocks. Type in the prompt to ask
the browser assistant; its replies appear in the right pane. Tab to the
screen, then:

- `c`: take control.
- `o`: open a URL.
- `n`: open a new tab.
- `1`–`9`: switch tabs.
- `x`: end the session.

With control, keys and clicks go to the remote screen; Ctrl+] or F12 hands
control back. **Remote desktop** works the same way for the Ubuntu computer
runtime.

To try the remote browser locally, run `make demo-tui-web`, open
`http://127.0.0.1:4287/browser` and press Enter. Ask
`What is the status of release 2.18.4?`, then take control and switch tabs.
The demo uses scripted replies and a synthetic screen.

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
