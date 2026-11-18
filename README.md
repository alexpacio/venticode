# Venticode

Venticode is a coding agent and a bot agent for Ventic workspaces. It works
like Claude Code in the browser: connected ventiharness machines appear in an
inventory on the left, each with project folders and their conversations. The
agent runs files, commands, terminals, sub-agents and MCP tools on the harness,
asking for permission per tool call. The same agent can answer `@ventic`
mentions in Slack, Telegram and GitLab, holding any write for review.

The browser uses the Rust service in an installation and a loopback-only
synthetic API during UI development. Ventic control authorizes the workspace
and relays member-scoped requests to the gateway and connected harnesses.

## Layout

| Path | Contents |
|---|---|
| `api/openapi.yaml` | Browser API contract (source of truth) |
| `web/` | Vite + React + TypeScript app; `web/src/api/schema.ts` is generated |
| `server/` | Rust HTTP service, PostgreSQL schema and encrypted S3 event storage |
| `connector/` | Apache Camel loopback transport for Slack, Telegram and GitLab |
| `dev/mock.ts` | Synthetic in-memory API used for development and tests |

## Development

Requires Node.js 24 or newer.

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

## API summary

All routes are under `/api/v1/venticode` on the workspace host, except the
ticket exchange.

- Session: `POST /access` (portal host), `POST /venticode/bootstrap`,
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

The distribution chart enables the service with `venticode.enabled`. The
standalone image includes the browser. A separate connector image runs in
the same pod and listens on loopback. Use the
[installation guide](https://gitlab.com/nettersrl/ventic/ventic-frontend/-/blob/main/docs/venticode.md)
for DNS, TLS, PostgreSQL, S3, feature policy and connection setup.

Run `make check-service` to format-check, lint, test and build the release
binary. Run `make check-connector` to package Camel and exercise it with a
disposable loopback HTTPS provider. The synthetic mock never sends messages
to any external provider.
