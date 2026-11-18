# Venticode

Venticode is a coding agent and a bot agent for Ventic workspaces. It works
like Claude Code in the browser: connected ventiharness machines appear in an
inventory on the left, each with project folders and their conversations. The
agent runs files, commands, terminals, sub-agents and MCP tools on the harness,
asking for permission per tool call. The same agent can answer `@ventic`
mentions in Slack, Telegram and GitLab, holding any write for review.

Status: the browser app and its API contract are implemented against a
synthetic mock. The Rust backend and the Ventic deployment integration are not
built yet (see [Integration plan](#integration-plan)).

## Layout

| Path | Contents |
|---|---|
| `api/openapi.yaml` | Browser API contract (source of truth) |
| `web/` | Vite + React + TypeScript app; `web/src/api/schema.ts` is generated |
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
  `POST /conversations/{id}/interrupt`, `POST /approvals/{id}`
- Bots: `GET /bots`, `GET|PATCH /bots/{id}`, `GET /bots/{id}/mentions`,
  `POST /bots/{id}/mentions/{mention_id}/review`

Permission modes: `ask` (writes and commands ask), `auto_edit` (file edits run,
commands ask), `plan` (read-only tools). A harness in YOLO mode skips prompts.

## Integration plan

Venticode plugs into Ventic the way Open WebUI does, without sharing its code:

1. **Host and tenancy.** One shared deployment serves
   `https://<workspace-uuid>.code.<domain>`. The workspace always comes from
   the host and the session.
2. **Sign-in.** The portal calls `POST /api/v1/venticode/access` and gets a
   one-time ticket bound to the portal session. It posts the ticket to
   `/venticode/bootstrap` on the workspace host, which sets a host-only
   `venticode_session` cookie. Mutations require that host's exact Origin.
3. **Backend (Rust).** Implements this contract. It keeps metadata
   (projects, conversations, approvals, bots, usage) in PostgreSQL and
   transcripts, tool output and diffs encrypted in S3. It runs the agent loop
   server-side through the Ventic gateway and sends tool calls to harnesses
   through control.
4. **Ventic changes still needed:**
   - control routes for the ticket exchange;
   - harness execution that does not depend on Open WebUI (today's harness
     consent and execute path requires an Open WebUI session);
   - a listing of harnesses shared with the user;
   - a `venticode.use` feature flag;
   - export and erasure hooks;
   - WAF, ingress and certificate entries for `*.code.<domain>`;
   - a Helm workload for the image;
   - a portal navigation entry.
