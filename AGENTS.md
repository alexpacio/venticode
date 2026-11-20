# Repository instructions

Venticode is a standalone, multi-tenant coding and bot agent. It is integrated
into the Ventic distribution the same way Open WebUI is: its own per-workspace
host (`<workspace-uuid>.code.<domain>`), a one-time ticket handoff from the
portal session, and venticode-harness as the remote runtime for tools, terminals,
sub-agents and MCP servers. It shares no code or runtime with Open WebUI.

Inspect git status and preserve user edits. Do not commit, push, publish or
deploy without an explicit request. Never create branches or pull requests
unless asked.

- `api/openapi.yaml` is the source of truth for the browser API. Change the
  contract first, then regenerate `web/src/api/schema.ts` with
  `make generate`; never hand-edit generated types.
- The UI is one terminal app: `tui/src` (`app.rs`, `nav.rs`, `screens.rs`,
  `view.rs`) is the shared core; `tui/src/main.rs` + `platform.rs` drive it in
  a console, `tui/browser/` builds the same files to WebAssembly with a
  browser `platform.rs`. Add features to the core, never to one build only.
  The look follows Claude Code (`●` blocks, `⎿` results, rounded prompt box,
  numbered prompts).
- `web/` only hosts the WebAssembly terminal (Vite, TypeScript, no UI
  framework): `web/src/main.ts` paints cells and forwards input. Do not add
  React, Tailwind or component kits. Every glyph the core draws must be in the
  bundled fonts (`web/src/font.test.ts`).
- `dev/mock.ts` is a synthetic, in-memory stand-in for the backend. It must
  stay loopback-only, never call a model or a harness, and never store real
  credentials or customer data.
- The backend uses Rust, PostgreSQL for metadata, and encrypted S3 objects
  for transcripts and tool output. Keep backend authorization, the exact
  workspace-host Origin check on mutations, idempotent message ids,
  optimistic revisions and unbuffered SSE as invariants.

UI copy is Italian first with an explicit English translation next to it
(`app.msg("italiano", "english")` in the TUI core). Write all
documentation in English. The bot mention handle is always `@ventic`.

Run `make check` (contract generation, type check, mock self-test, build) and
`make check-tui` before reporting work as done, and say exactly which checks
ran.
