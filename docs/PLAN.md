# Tracer: plan

Tracer is an API workbench: build, send, test and benchmark requests against
local and remote APIs, by hand or through an AI agent, with the window showing
everything live.

## Why it exists

1. **To strain Rux.** Battle-test its edge cases, find its limits, its poor
   decisions and what nobody thought of.
2. **To show Rux off.** A desktop tool developers judge on feel.
3. **To feed Rux's team.** Every limit goes in `FINDINGS.md`, in the field
   report format the Rux repository already uses.

**Where Rux is limited, Tracer does not work around it: it reports.** Rust in
the `native/` crate for I/O is Rux's designed path and not a workaround.
Polling because Rust cannot push, UI drawn in Rust, faked layout, or logic moved
out of `.rux` because Rux handles it badly, are workarounds.

After Tracer, the team moves on to Mava Studio, which goes deeper. Team
features (shared workspaces, cloud sync, collaborators) are left for it.

## Inspiration

`docs/ref-resonance.jpg` and `docs/ref-hoppscotch.jpg`.

| From Resonance | From Hoppscotch |
|---|---|
| Environment picker heading the sidebar | Protocol rail (REST / GraphQL / Realtime) |
| Coloured method badges + paths, grouped by resource | ⌘K command palette |
| Path Params as its own tab | Send ▾ split button (send & save, send ×N), separate Save |
| Status / TTFB / size chips | Environment peek (eye) |
| Response tabs: Body, Headers, Cookies, Performance, Scripts | Param rows with enable toggle, delete, bulk edit |
| Code / Preview toggle; `>_` copy as cURL | Response toolbar: wrap, JSONPath filter, download, copy |
| Unsaved dot on tabs; draggable split | Count badges on tabs; Test Results tab; JSON folding |
| Status bar with env and version | Footer panel toggles; help |

```
┌─rail─┬─sidebar──────────┬─tabs: ● GET Echo │ POST album │ + ─────── [Env ▾] 👁 ┐
│ REST │ [Env ▾] [⌘K]     │ [GET▾] {{baseUrl}}/photos?albumId=1  [Send ▾] [Save] >_│
│ GQL  │ Collections      │ Params(1) Path Body Headers(2) Auth Scripts Asserts    │
│ WS   │ History          │ ☑ albumId   1                              ✕          │
│ SSE  │ Bench            ├──────────────── drag ─────────────────────────────────┤
│ Bench│ Mocks            │ 200 OK · TTFB 147ms · 10.4KB · ✓ 3/3 asserts           │
│Agents│ ▸ ALBUMS         │ Body Headers Cookies Timing Tests Diff  [wrap][$.][⬇][⧉]│
│ ⚙    │   GET /albums    │  1 ▾ [ {...                                            │
└──────┴──────────────────┴────────────────────────────────────────────────────────┘
 env: local · 🤖 claude-code connected · 3 running                  [◧ panels] [?]
```

## Architecture

The core is a **separate process**. The window is one client of it, beside
the CLI and AI agents.

```
          ┌───────────── Tracer core (separate process) ───────────┐
          │ workspace files · env/secrets vault · history · runner │
          │            command bus  →  ordered event stream        │
          └───▲──────────────▲──────────────▲──────────────▲───────┘
              │              │              │              │
          Window (Rux)   CLI (headless)  MCP server    Local HTTP/WS API
           dev clicks    CI, scripts     any agent      anything else
```

- **Every action is a command on one bus**, whoever sends it. The window
  renders the event stream, so an agent's request opens a tab, fills in,
  sends and streams its response as if the dev had done it, tagged with who
  did it (`🤖 claude-code`, `👤 you`).
- **No window, nothing lost.** Headless runs write the workspace and a
  journal; the window shows them in history when it opens.
- **The window starts the core** if none is running, and stops it on close if
  it started it.
- **Same request edited by the dev and an agent**: the window shows the
  agent's edit live; on a true conflict the dev's edit wins.

```
crates/protocol/   commands + events shared by everyone
crates/core/       the separate process: HTTP engine with timing, workspace
                   files, vault, journal, runner/bench, local WS API, MCP
crates/cli/        headless runner + `tracer mcp` stdio bridge → core
app/               the Rux window
  native/          thin client: connects to core, exposes a Core resource and
                   Signals for the event stream
probes/            Phase 0
FINDINGS.md        Rux limits
```

## AI: a tool AI uses, never a bundled AI

1. **An external agent drives Tracer.** Claude Code, Cursor, an Ollama agent
   or a script connects over MCP (stdio or local HTTP). Tools:
   `list_requests`, `send_request`, `run_flow`, `run_bench`,
   `diff_responses`, `create_mock`, `read_history`, `assert`.
2. **Optional in-window chat, bring your own model**: any OpenAI-compatible
   endpoint, Anthropic, Gemini, or a local Ollama / LM Studio with no key. It
   calls the same tools as option 1. Keys live only in the local vault.

**Guardrails**: per-agent permissions (read-only / local env only / anywhere);
an approval prompt in the window for non-GET or `prod`; agents see
`{{token}}`, never secret values, and secrets are redacted in responses and
history they read; rate and bench-size caps; a kill switch; an audit log.

## Features

**Requests**: REST and GraphQL first, then WebSocket and SSE (live message
stream); gRPC a stretch. Variables with hover preview, secrets masked.

**Benchmarking, local and remote**: timing waterfall per request (DNS, TCP,
TLS, TTFB, download); load runs (N at concurrency C, or a rate for a
duration) with live p50/p95/p99, histogram, error rate, throughput; the same
collection against two environments side by side; saved baselines with
regression flags; rate-limit detection.

**Correctness**: no-code assertions; flows that chain requests
(login → token → next); data-driven runs from CSV/JSON; OpenAPI import,
response validation and drift detection; response diff ignoring noisy fields.

**Dev workflow**: the workspace is plain files (git-friendly, agent-editable);
`tracer run <collection> --env local --report json`; mock server from a
collection or spec; paste cURL/fetch to import, export to cURL / Rux / JS /
Python; "explain this failure" bundle for any AI; localhost port discovery;
re-run on file change; self-signed TLS toggle; searchable, replayable history.

## What each part strains in Rux

| Feature | Rux surface |
|---|---|
| Large JSON viewer (fold, highlight) | long lists, inline text, scroll, perf |
| Request / body editor | text input, selection, highlighting in a field, undo |
| Panes, tabs, panel toggles | layout, `@drag`, persisted layout |
| ⌘K, shortcuts | `@key`, focus |
| Send, cancel, timeouts | `async`, cancelling in-flight work |
| Live sync, bench samples, WS stream | Rust → Rux signals at rate, ordered events |
| Waterfall, histogram, live charts | `<path>` |
| Window owns the core | child process, `@close`, `unmounted` |
| Many tabs and environments | stores at scale |
| Approval prompts mid-run | overlays, async interplay |
| Copy as cURL, import | clipboard, file dialogs, drag-and-drop |
| Themes | CSS, `color-scheme` |

## Phases

0. **Probe.** `probes/`: one page per capability, driven by a person; results
   into `FINDINGS.md`.
1. **Core + command bus + window shell**, the window a client of the core from
   day one.
2. Workspace, environments, history, journal.
3. Local API + MCP + CLI.
4. Response tools → assertions and flows → benchmarking → realtime and
   GraphQL.
5. In-window chat with your own model; guardrails polish.
