# Tracer: progress log

Where the work stands, what comes next, and what tripped us up on the way and
how it was solved. Read this first in a new session; update it at the end of
each one. Rux's own limits go in [FINDINGS.md](../FINDINGS.md), not here; this
file is about working on Tracer.

## Where it stands (2026-11-05, evening)

The user is taking the open findings to the Rux team, and asked for the
housekeeping and the small Tracer items meanwhile. Both branches pushed
(e58c6d0), then done and driven: a dead core's mock closed by the next
(e19a6c0's parent), journal write failures said (ada24b6), tokens and
WebSocket keys from the OS's randomness (d766e99), connections limited, a
hello deadline and slow clients let go (4bb7889, fixed in 40245e5: see the
gotchas), a relative `rux-native` path (40245e5), and the Bench baseline
row (below). Left of the outside review: retention of events (M-L), disk
file-change notifications (M), splitting `app.rux`, and the protocol
questions. Nothing of this session is running.

## Where it stood (2026-11-05, afternoon)

The user picked three things: OpenAPI (import, validation, drift, a spec
mock), the schema-aware query editor, and undo in the editor. All three
are built and driven in the window, committed on `phase-1-core` (889fe9e,
27de06b, f296b3c, 97b2914), **not pushed** (11 ahead of origin). The
user also said the Rux team fixed findings and that they needn't be
re-driven: **AW-041 and AW-042 are marked fixed** from the Rux log
(cabfc45, 29fb783); Tracer had no workaround for either, so its
`margin-left: auto` rules now lay out as written. **AW-043 has no fix
yet.** New findings: **AW-044** (a false `rux check` error on top-level
order; Tracer's API-view signals sit above the event effect because of
it, to move back), **AW-045** (`[]` in an `if` branch typed `any[]`),
**AW-046** (no caret, selection or styled runs for a field from script:
completion at the caret and highlighting in a field are blocked),
**AW-047** (a field's undo: lost on blur, script edits not undoable, an
empty history still swallows Ctrl+Z). Left from the plan: gRPC
(stretch) and Phase 5 (at release). Nothing of this session is running.

## Where it stood (2026-11-05)

**`phase-1-core` pushed** (2dfc0f4; the 15 commits from bb55ab1 on). The
user's core from 2026-11-02 (`tracer-running-2026-11-02.exe`, PIDs 10280,
14864, 15456) is gone: the machine restarted 2026-11-05 04:51, and no
`tracer` process runs. Phases 0 to 4 are done; Phase 5 (in-window chat,
bring your own model) hasn't started. The plan's features not built yet
are listed under Next, item 7. Next is still Show more (item 1).

Later the same day the user said: **leave Phase 5's chat until near
release**, and build the S and M items (Next, items 2 and 7) first.
Done so far, each driven in the window: the TLS switch (bc2ff7e),
export/import (1571661), re-run on file change (cf3a26d), explain
(3271f47), finding local servers (17aba45), data-driven flows (ee28163),
the mock server (d363488), CLI send on an open WebSocket and transcripts
(0bace2c), event-stream reconnect (38dbff7). **master was fast-forwarded
to phase-1-core and pushed** (the user asked, 2026-11-05: it had been at
Phase 0); keep both pushed together. Since: MCP over HTTP (0efbb36),
per-environment jars and the Cookies tab (58f7724), GraphQL
subscriptions (9d0ddb0), benching a flow step by step (7a72bcd), a bench
connection fix (c81b04b), themes and **AW-043** (8622038). Not pushed
since a9cb64b. **Every S/M item is done**; left from the plan: OpenAPI
import/validation/drift and a spec mock (L), gRPC (stretch), undo in the
editor (depends on Rux), the schema-aware query editor (L), and Phase 5
(at release). **The user installed a new `rux` (2026-11-05 05:33; the
checkout is at 29fb783, "An auto margin takes the free space"): AW-042
looks fixed and should be re-driven.** Scratch servers (scratchpad):
`ws_echo.py` 18791, `cookie_server.py` 18793, `gql_ws.py` 18794
(graphql-transport-ws), `tls/server.py` 18443, `www/` on 18765. Scratch setup for driving
(scratchpad): `tls/server.py` (self-signed HTTPS on 18443, `/hop`
redirects to 127.0.0.1), `www/` served on 18765 with an `openapi.json`,
workspace `ws/` (localhost and *.dev.test trusted unchecked).

## Where it stood (2026-11-04, night)

The user went to sleep and asked for development to go on, committing as
it goes. Done since the evening, all on `phase-1-core` (pushed
2026-11-05; a922dfd onwards): AW-036 re-driven, fixed; **AW-042** found (`margin:
auto` is 0 in flex, which is why the Collections `+` and Save don't sit
at their row's end); the small layout fixes (652b9df); Realtime's body
no longer leaks into a GET (808af61); a flow step's re-run gets its
captures (62f1a93); a bench picked later shows its numbers and line
(ea0d60e); **run-until** (02783e2, de4fe95); and **browser behaviour**,
cookies and CORS (6f6a1d1): all three of the user's features. Next: Show
more (item 1).

## Where it stood (2026-11-04, evening)

`phase-1-core` pushed (59f0cd2), then the response viewer merged
(a922dfd, not pushed): **Phase 4 is done.** Driving it found **AW-041**
(a release build fails: a closure capturing a parameter of a handler
that calls an `async fn`; the viewer's `toggleFold` is one), and an open
question on Show more's cost (Next, item 1). The user says **AW-036 is
fixed too**: not yet re-driven. The user asked for three features
(Next, item 5).

Left running by this session: none at the end (check
`Get-Process tracer,rux,python`); the scratch workspace and test bodies
are in the session's scratchpad.

## Where it stands (2026-11-04)

The user installed a new `rux` (built 2026-11-04 00:17 from the Rux
checkout at 7a2be84, which had uncommitted changes) and said the findings
were cleared. **AW-037 to AW-040 are all fixed**, each re-driven from
its reproduction in the window (results in FINDINGS.md). No
notification from the Rux team's session was waiting. Tracer's
workarounds for AW-038, AW-039 and AW-040 are removed, and the result
driven in Tracer's window: a saved GraphQL request opens split and
unedited, and sends 200 with checks 1/1. Next: the response viewer
(Next, item 1), now that AW-037 no longer blocks it.

## Where it stands (2026-11-03, late)

GraphQL (7ff184a) and realtime (c81785a) committed, then benchmarking's
later parts (2026-11-04: environments side by side, baselines, rate-limit
detection; see Done), committed on `phase-1-core`. Not pushed. **Phase 4
is done except the response viewer**, which waited on AW-037 (fixed
2026-11-04).

Left running then, not this session's: the user's `rux run`, window and
core on `~/Tracer` (PIDs 10280, 14864, 15456; the window relaunched itself
at 22:47 when this session edited `app/native`, see the gotchas), and four
Python processes from 2026-10-30, one of them a `SimpleHTTP` server on
port 8765. The core ran from `target/debug/tracer-running-2026-11-02.exe`
(renamed so cargo could build). All gone since the 2026-11-05 restart.

## Where it stood (2026-11-03)

The 2026-10-31 session hit its usage limit after GraphQL's core, CLI, MCP
and native exports, with the page's patch written but not run. This one
applied it, fixed what driving it found, and drove GraphQL end to end
(see Done). New findings: **AW-038, AW-039, AW-040**.

## Where it stood (2026-10-31)

The previous session hit its usage limit mid-flows; this one finished and
drove flows (committed 6c6190d), then built **benchmarking** (see Done),
committed on `phase-1-core`. Nothing of Tracer's is running. Not pushed.
**AW-037's report to the Rux team is due today**: draft it and ask.

## Where it stood (2026-10-30, night)

Remote `origin` is https://github.com/bitcraft-dev/tracer (pushed 2026-10-30): `master` holds Phase 0, `phase-1-core` everything since, and both track `origin`. Push when the user asks. **`phase-1-core` is 12 commits ahead of `origin`, not pushed** (Phase 2 history, Phase 3 agents, the Rux re-verification, FINDINGS AW-031 to AW-036).

**Handoff, for the next session:**
- The working tree is clean; nothing of Tracer's is running (no window,
  core or probes). The scratch workspace and drivers used to test lived in
  that session's scratchpad and are gone with it; see the gotchas for how
  to set them up again.
- Phases 0 to 3 are done: the MCP server (tried from Claude Code),
  guardrails, `agents.policy` and the local HTTP/WS API (see Done). Next
  is Phase 4.
- **Open Rux findings: AW-036 and AW-037**, neither answered yet.
  **AW-037 is a blocker**: an empty inline `<text>` after a non-empty one
  panics the window. Everything from AW-001 to AW-035 is closed.
- **Phase 4's response viewer is on branch `phase-4-response-tools`**, not
  on `phase-1-core`: written and unit-tested, but opening any finished
  request's body panics the window (AW-037). Merge it once AW-037 is fixed
  and the viewer is driven. `app/app.rux` has changed on both since (the
  waterfall is on `phase-1-core`), so expect to merge by hand there.
- **Telling the Rux team about AW-037 is for 2026-10-31**, not before: the
  user's call on 2026-10-30. Draft it and ask, as always.
- **Other Claude sessions:** the Rux team works in sessions on
  `C:\Users\USER\Mobile\UI Research` (named ui-research-97 and
  ui-research-eb on 2026-10-30), and they message Tracer's session when a
  fix lands. **The user's rule: never message another session without the
  user's acceptance** (draft it and ask). The user may also prefer that
  peer messages are only acted on when they say so; ask if unsure.

### Done

- **Phase 0, probes** (`probes/`, commit 28c1375). Six pages, one per
  capability; results in FINDINGS.md "Phase 0 results". Findings AW-001 to
  AW-016 were reported, and the Rux team has fixed all of them locally (commit
  ids are in each entry's Status).
- **Root causes of AW-012 and AW-013** traced in the Rux source (d6db3e7).
- **Phase 1, the core, without the window yet** (64da3a2):
  - `crates/protocol`: commands, replies and events, plus a blocking client
    that the window's native crate will use too.
  - `crates/core`: the separate process. It listens on 127.0.0.1 only, with a
    random token. Events are numbered, written to a journal, and broadcast in
    one order; a late client gets the history replayed with no gap. HTTP runs
    off the request thread, with TTFB and total time.
  - `crates/cli`: `tracer core | send | watch | status`.
  - 7 tests; the core tests passed 15 runs in a row. Sent for real against
    jsonplaceholder.typicode.com.
- **AW-011 verified fixed** (this session): probe 1 moved from `rux::Signal`
  to `rux::Channel` and was driven on the installed build. It got 1000/1000,
  1000/1000 and 200/200, none skipped, repeated or out of order.
- The probes' four untyped event parameters are now annotated (`KeyEvent`,
  `DragEvent`), so `rux check` is clean and the dev overlay no longer opens on
  launch.
- **All six probes re-run on the fixed build** (FINDINGS.md, "Re-run on the
  fixed build"). AW-012 no longer runs away, AW-013 no longer locks up, and
  probes 1, 4 and 5 work. Two new blockers: **AW-017** (an idle window doesn't
  draw keys or `await` results until a hover, and keys typed meanwhile are
  lost) and **AW-018** (layout of every row every frame: 414 ms a frame at
  1k rows, debug).

- **Re-run on the pushed Rux build (a27366f)**: all six probes work with the
  pointer parked. AW-011, AW-012, AW-013, AW-017 and AW-018 are closed for
  Tracer. AW-017 turned out not to be a Rux bug at all: the earlier run built
  against a Rux checkout with half-written work in it (see the gotchas).
  Release build: 1k lines draw in 0.33 s of CPU, and 100 scroll notches cost
  0.34 s.

- **Phase 1's window shell** (`app/`, uncommitted on 2026-10-30, along with
  the core's `Shutdown` command, `tracer stop` and the protocol's `Shared`
  client from the session before):
  - `app/native/src/tracer.rs`: finds or starts the workspace's core, follows
    its events from seq 0 onto a `rux::Channel` (`tracer.events`), the link
    state on a signal (`tracer.link`), `send`, and `close`, which stops the
    core if this window started it.
  - `app/app.rux`: rail, history (newest first, who sent each), request bar
    with Body/Headers tabs, response pane (status, TTFB, total, size, body,
    headers), status bar. All of it is built from the core's events.
  - Driven: a `tracer send` from the CLI appears live in the window; a send
    from the window shows up as `window · USER`; closing the window stops its
    core and removes `core.json` (0 processes left). 9 tests pass.
  - Findings filed: **AW-019** (a `computed` or top-level `let` can't read a
    native signal; root cause traced), **AW-020** (missing script helpers, and
    `reverse`/`replace` silently mutate and return nothing), **AW-021**
    (`&quot;` not decoded inside `{{ }}`).

- **Committed** (6dd0497, af47b0b). Then, the same day:
  - **History survives a core restart**, checked: a fresh core replays the
    old journal and the window shows the old requests with their full
    responses; numbering carries on. No change needed.
  - **Resizable panes**: the sidebar's width and the request editor's height
    drag (`@drag`, `totalX/Y`), and are kept with the built-in `settings`
    store (`%APPDATA%\dev.ruxlang.tracer\settings.json`), restored at launch.
    Driven: dragged, closed, reopened, same layout. Filed **AW-023** (the
    docs' `@drag` example uses a field that doesn't exist).

  - **Ctrl+K palette** (59983ca): arrows and Enter on the palette's own
    field (its `@key` runs before the field's editing). Filed **AW-024** (an
    effect re-runs with its signal unchanged right after a key handler,
    eating the first ArrowDown after typing; minimal repro) and **AW-025** (no
    clipboard for script, so no Copy as cURL).
  - **Cancel**: `Command::Cancel`, `request_cancelled`, `tracer cancel <id>`
    (and `tracer send` prints its id), a Cancel button in place of Send while
    the request on show runs, and a palette command. Anyone can cancel
    anyone's request; the event names who did. The core keeps a table of
    running requests: cancel and finish race to take the entry under the
    hub's lock, so a request ends exactly once. Bodies stop between 64 KiB
    reads; the wait for the status line can't be interrupted with `ureq`.
    Two tests. Driven: window cancels its own drip; window cancels the CLI's,
    and the CLI prints "cancelled by USER". Filed **AW-026** (script can't
    cancel or time out an awaited native call).
  - **Fixed, found while driving cancel:** every hot reload stopped and
    restarted the core (`unmounted` called `close`), and closing the window
    while `connect` was starting a core orphaned that core. `close` now waits
    for `connect`, and only `@close` stops the core. Driven: reload keeps the
    core's PID; 8 closes at 0 to 1000 ms after the window appears leave no
    core.

- **Phase 2, started (2026-10-30, afternoon):**
  - **Saved requests** (000d752): one `.http` file per request under
    `requests/`, folders as collections (the REST Client / JetBrains format).
    `ListRequests` (with the event number it's true at), `SaveRequest`,
    `DeleteRequest`, published as events. CLI `ls`, `save`, `run`, `rm`.
  - **Dangling requests** (d3581a8): a core opening a journal closes requests
    a dead core left running, once.
  - **Environments** (8c3b98b): `environments/<name>.env`, `KEY=value`.
    `Send` takes an env; the core fills `{{name}}` just before sending and
    fails the request naming any variable that's missing. Events keep the
    request as written plus the env's name. CLI `env`, `--env`.
  - **Disk watching** (36f0640): the core rescans `requests/` and
    `environments/` every second and publishes changes as actor kind `disk`;
    its own writes aren't echoed; half-written files wait until they parse.
  - **Window** (c2b710c), driven against Rux a2c986b: Collections with
    open/save (Ctrl+S, save-as prompt)/new/delete and an unsaved dot;
    environment picker heading the sidebar, env on send, env tags in history,
    env in the status bar; an outside save to the open request is followed
    with no local edits ("updated by disk · disk") and kept out with a notice
    when there are (dev wins). Hot reload re-subscribes from 0; events at or
    below the last seq are dropped. `color-scheme: dark` on the root.
  - Findings: **AW-027** (an effect that empties a signal it reads is never
    woken by it again: only the first outside change reaches the editor per
    page load), **AW-028** (a reloaded page re-reads the channel's last
    list). **AW-019 and AW-020 verified fixed at runtime** (800250d,
    a2c986b); the installed `rux check` is older and still refuses the
    AW-019 `computed`.

  - **Secrets vault** (e7e4473): values in `.tracer/secrets/<env>.env`;
    `.tracer/.gitignore` (`*`) keeps them, the journal and the token out of
    git. Clients get secret names only; `SetSecret` is write-only. Values of
    4+ characters that come back in a response are replaced by `{{name}}`
    before publishing. CLI `secret set` (stdin) / `secret rm`.
  - **Environments panel** (3934571): ⚙ beside the picker; edit variables,
    set/remove secrets by name, create, delete. Lists afresh on open and
    after its own changes. **URL preview** under the URL bar: filled from the
    chosen env, secrets as dots, a warning for undefined variables.
  - Findings: **AW-029** (an `async fn` an effect starts when a channel woke
    it never returns from its `await`: an open panel misses changes made
    elsewhere), **AW-030** (no fallback to Segoe UI Symbol: ✕ ▾ ▸ ⧉ ⌫ draw as
    boxes; Tracer uses ×). The Rux team fixed AW-021 meanwhile (73ff9a7),
    unverified.

- **Phase 2 finished: searchable, replayable history (2026-10-30, evening).**
  - Core and CLI (1550ac8): `History { filter }` gives past requests newest
    first, as written, with how each ended (status, total, size, or the
    error, or who cancelled), filtered by text (method, URL as written, env,
    sender, status, `#id`), actor kind and status class (`2xx`…`5xx`,
    `failed`, `cancelled`, `running`), up to a limit (50). `Rerun
    { request_id, env }` sends one again as written, with its env unless
    told otherwise (`""` for none); `request_started` carries `rerun_of`
    (defaulted, so old journals read). CLI `tracer history [TEXT] --actor
    --status -n` and `tracer rerun <ID> [--env | --no-env]`. Two core tests
    and two protocol tests; 27 in all, passing. Built for Phase 3's
    `read_history` agent tool too.
  - Window: a search field and actor/status selects under History, "n of
    m" and a clear link; rows tagged `↻ #n` for re-runs; the response pane
    shows `#id`, the env, a "re-run of #n" chip that jumps to the original,
    and **Edit** (puts the request, headers and body into the editor,
    unsaved, and picks its env if it still exists) and **Re-run**. Palette:
    re-run, edit, clear the filter. Driven against Rux 4906409 (checkout
    clean): search, 5xx filter, Re-run (sent with the original's env, not
    the picker's), the chip, Edit of a POST with body and header, a CLI
    re-run arriving live, the palette's re-run.
  - `tools/drive.ps1 -Do print`: PrintWindow capture, works with the
    display asleep.
  - Findings: **AW-031** (focus is kept by position: an element appearing
    before the focused one takes the ring and Tab's place; root cause in
    `rux-shell`), **AW-032** (a select's dropdown runs off the window's
    bottom; the options below can't be reached by wheel or keys),
    **AW-033** (no wall clock or `Date`: history rows can't say when).

- **Phase 3, agents (2026-10-30, evening).** Started without waiting on
  the Rux fixes, which it doesn't need.
  - Core and CLI (a3a44d9): `tracer mcp`, an MCP server on stdio (JSON-RPC,
    a line each; versions 2024-11-05 to 2025-11-25). It connects as an
    agent named by the client's `clientInfo.name` (or `--name`), and starts
    a detached core if none runs. Nine tools: `list_requests`,
    `list_environments`, `send_request`, `run_saved_request`,
    `rerun_request`, `read_history`, `get_response`, `save_request`,
    `cancel_request`. A send waits up to `wait_seconds` (60) and returns
    `awaiting_approval` at once when a person must allow it; bodies are cut
    at 32 KiB (`get_response` `max_bytes` up to 1 MiB).
  - Guardrails (`crates/core/src/guard.rs`): an agent's non-GET/HEAD/OPTIONS
    or `prod…` request publishes `approval_asked` and waits (5 min) for
    `Decide` from a person; refused, it fails "refused by <person>".
    `SetAgents` is the kill switch: off, agents may only read and their
    running requests are cancelled; it is read back from the journal at
    start. Secrets, deleting environments, approving, the switch and
    stopping the core are refused to agents. `Response` gives any past
    request with its whole response. CLI `approve`, `deny`, `agents on|off`;
    `status` shows the switch. 31 tests pass (2 new core, 2 guard).
  - Window: an approval card per waiting request at the top of the work area
    (who, method, URL, env, why; Show, Refuse, Allow), rows marked
    "approval?", the pane saying nothing was sent, Cancel offered while it
    waits; **agents on/off** in the status bar; palette commands for both.
    Driven: an MCP agent's POST waited, Allow sent it (201); a DELETE
    waiting was cancelled by switching agents off, and the agent's next
    send was refused while reads still worked.
  - Fixed on the way: a core started by `tracer mcp` inherited the agent's
    stdio pipes, so they never closed after the server exited (the agent
    would hang); the server now marks its standard handles not
    inheritable before spawning. A BOM at the start of stdin is skipped.

- **Every open finding re-driven on Rux 4deca46 (2026-10-30, evening)**,
  installed `rux` 17:17, checkout clean. AW-019 to AW-033 all verified
  fixed (each status in FINDINGS.md says how), except AW-020's spread,
  which is still missing (documented). Workarounds removed: `fn connected()`
  is a `computed` again; `clamp` uses `Math`; the env panel's reloads after
  its own changes (AW-029); × back to ✕ (AW-030); the probes'
  `native/src/clock.rs` (AW-009, `now()` instead). Unlocked: **Copy as
  cURL** (variables filled, secrets left `{{name}}`, POSIX quoting; response
  pane and palette), **sent times** on history rows and the response pane
  (`Date`), and probe 3's **Cancel and time limit** (AW-026). The palette's
  list now scrolls within the window.
  - New: **AW-034**, the probes window paints only its top part (layout
    and hit-testing are fine below the line). Suspected: the AW-022 vello
    buffer sizing. Tracer's window is not affected.

  - **Rux 08c314f (installed 17:52):** AW-034 fixed (b604acc) and re-driven
    (probes 1, 2 with 1k lines, 6, at three sizes); AW-020's spread works.
    Both closed. New: **AW-035**, probe 2's wrapped lines overlap the next
    row at a narrow window (a flex row shrunk below its text's width keeps
    one line's height); minimal pages didn't reproduce it.

  - **Rux b4fe20a (local, not pushed):** AW-035 fixed and re-driven
    (probe 2 at 978, 820 and 1360 px wide with scrolling; Tracer's Headers
    tab with a 105-character header). New: **AW-036**, an `overflow:
    hidden` flex item keeps its content width as its minimum, so a long
    history URL squeezes the method column and overflows the row
    (pre-existing: the installed 08c314f does it).

- **Phase 3, from a real client, and the policy file (2026-10-30, night).**
  - `tracer mcp` driven by Claude Code (the VS Code extension's
    `claude.exe -p --strict-mcp-config --mcp-config`, Haiku, under 1 cent a
    run): all nine tools listed, `list_*`, `run_saved_request`, a POST that
    returned `awaiting_approval`, `read_history`; allowed afterwards with
    `tracer approve`, it was sent (201) under `agent:claude-code` although
    the agent had exited. No Tracer change was needed.
  - **`agents.policy`** at the workspace root (`crates/core/src/policy.rs`):
    `[*]` and `[agent-name]` sections with `access` (read-only, local,
    anywhere), `approve` (writes, prod, all, none), `rate` (`N/s|min|h`)
    and `max_body`. A named section takes its gaps from `[*]`; no file is
    the old fixed rules. `local` is checked on the URL with variables filled
    in, and turns off redirects. Out of bounds: a failed request, before
    any approval; over the rate: a refused command, unnumbered. Re-read with
    the disk scan; `policy_changed` events; a mistake keeps the old rules
    (and a core starting on one lets agents only read). `Command::Policy`,
    `tracer policy`, MCP `read_policy` (an agent sees only its own rules).
    Window: the status bar shows a policy mistake in red until it's fixed.
    39 tests (7 unit, 1 integration new), three runs clean.
  - Driven: Claude Code under `[claude-code] access = local` read its
    policy, and its run of a jsonplaceholder request failed "refused by the
    workspace's policy: jsonplaceholder.typicode.com is not this machine";
    the window showed it, and the red notice for a broken file, gone once
    fixed.

- **Phase 3 finished: the local API (2026-10-30, night).** Policy work
  committed first (7717163). `crates/core/src/api.rs`: the core's port
  tells its protocol (`{`) from HTTP (a method) by peeking. `POST
  /v1/command` (Bearer token; `X-Tracer-Kind`, default `agent`;
  `X-Tracer-Name`, default `http`) runs one command through the same
  `conn::serve`, now transport-free (lines in, a line writer out).
  `GET /v1/ws` is the protocol over a WebSocket, hand-written (RFC 6455
  framing, SHA-1 and base64 for the handshake, no new crates). Host and
  Origin must be this machine. `tracer status` prints the addresses. 42
  tests, three runs clean. Driven: curl (policy, an agent's POST waiting,
  a foreign Origin refused 403) and Node 24's built-in `WebSocket` (hello
  as a person, subscribe with replay, allowed curl's request).

- **Phase 4 started: the response viewer** (branch
  `phase-4-response-tools`). `app/native/src/view.rs`: `body_view(id,
  folded, limit)` fetches the whole body from the core (`Response`; events
  carry 64 KiB), lays it out once per request on show, and gives the lines
  the folds leave visible, up to a limit (1,000 more per "Show more").
  JSON is pretty-printed from its own text by a small parser, so keys keep
  their order and numbers read as sent (`1.50`, `-2e3`); a fold gives
  `{ … }` with "3 keys" / "12 items"; anything else is its own lines. Fold
  numbers are the unfolded line numbers, so inner folds stay put. Page:
  numbered lines, ▾/▸ per foldable line, colours by kind. 4 unit tests.
  Driving it found **AW-037** (the window panics on the first body shown).

- **Phase 4: the timing waterfall** (on `phase-1-core`, not blocked by
  AW-037). `crates/core/src/http.rs` builds `ureq`'s default connector
  chain itself with marks after TCP and TLS, and wraps its resolver, so
  each request reports `dns_ms`, `connect_ms`, `tls_ms` (none for `http:`)
  beside TTFB and total; `Timing::wait_ms` is what's left of TTFB. Old
  journals read (the fields are optional). With redirects, the phases are
  the first connection's. CLI: `200 · TTFB 1649 ms (DNS 1079 · connect
  186 · TLS 191 · wait 193) · total …`; MCP: `phases_ms`; window: a
  segmented bar and its key under the chips, none for older requests.
  44 tests, three runs clean. Driven: https://example.com spent 15,205 ms
  of 16,646 in connect, which is this network's lost SYNs in one line.

- **Phase 4: response diff** (`crates/core/src/diff.rs`). `Command::Diff
  { a, b, ignore, no_defaults }`: status, headers by name, and the body,
  JSON as values (key order and spacing don't count; arrays by position;
  each change by path, `$.items[2]`), anything else by line (LCS, two
  lines of context, `gap` between hunks; past 4M line pairs, "too large").
  Ignore patterns: `$.path[*].key` (and what's under it), a bare key
  anywhere, `header:name*`; defaults for Date, ETag, request/trace ids,
  `timestamp`, `requestId` and the like; every difference ignored is
  listed. CLI `tracer diff A B [--ignore P]... [--all]` (exit 1 when they
  differ, for CI); MCP `diff_responses`. 48 tests. The window's view of it
  waits for the viewer branch (AW-037).

- **Phase 4: checks (assertions)** (`crates/core/src/expect.rs`).
  `HttpRequest.expect` (optional on the wire): a request's expectations
  as written, saved as `# @expect` lines above its request line,
  journalled with it, kept on re-run. The core checks them on what
  clients see (secrets out) and publishes `request_checked` right after
  the end under the same lock; history carries passed/failed; `Response`
  carries the results. CLI `--expect`, a ✓/✗ list and exit 1; history
  `✓2` / `✗1/5`. MCP `expect` on send and save, `checks` back. Window: a
  Checks tab in the editor (one per line, saved with the request) and in
  the response (✓/✗, what was found). 52 tests. Also fixed: a remembered
  environment the workspace doesn't have is dropped when the list loads
  (it made a send fail).

- **Phase 4: flows** (`crates/core/src/flows.rs`, 2026-10-31). A folder
  of saved requests runs in path order; `# @capture name = $.path |
  header <name> | status` fills `{{name}}` in later steps. `RunFlow`,
  `flow_started`/`flow_finished` (each step passed/failed/skipped with a
  note), a `flow` id on requests and history. A failed step skips the
  rest unless `keep_going`; each step goes through the same rate limit,
  policy and approvals as a send (`take_rate`, `run_one`). CLI: `tracer
  run <folder> [--keep-going] [--report json]`, exit 1 on failure. MCP
  `run_flow` waits and returns each step. Window: a `flow N` chip on a
  step, the latest flow's state in the status bar (red when failed), a
  palette command running the open request's folder; the editor's Checks
  tab holds `capture …` lines too. 53 tests. Driven: CLI pass, fail,
  `--keep-going` + JSON report; MCP `run_flow`; the window's palette run
  and status bar, against jsonplaceholder.

- **Phase 4: benchmarking** (`crates/core/src/bench.rs`, 2026-10-31).
  `RunBench { request, env, plan }` with `BenchPlan { total, concurrency,
  duration_secs, rate }`; `StopBench`. The first request is an ordinary
  one (history, waterfall, policy, approval; `bench` on `request_started`
  and history); then `concurrency` workers on kept connections
  (`http::keep_alive`/`execute_with`) send the rest, counted, not kept.
  `bench_started`, `bench_progress` (every 250 ms) and `bench_finished`
  (`stopped` says why/who) carry `BenchStats`: done, failed, bad_status,
  failed_checks, statuses, top 5 errors, min/mean/p50/p95/p99/max,
  per_sec, histogram over `BENCH_BUCKETS_MS`. Policy: `max_bench`,
  `max_concurrency`, and `approve = bench` (in the defaults, so an agent's
  bench waits for a person); a bench counts once towards `rate`; the kill
  switch stops agents' benches; a core restart closes open ones. Hard caps
  for everyone: 1,000,000 requests, 256 at a time, 1 h. CLI `tracer bench
  <PATH> [-n] [-c] [-t 30s] [--rate] [--env] [--report json]` (live line,
  text histogram, exit 1 if any went wrong) and `tracer bench stop <ID>`;
  MCP `run_bench` (says `awaiting_approval` while a person decides) and
  `stop_bench`. Window: a **Bench** rail item; the panel benches the
  request in the editor: plan fields, Run/Stop, progress bar, tiles,
  requests-a-second line (`<path>`), latency histogram, statuses and
  errors; a "first of bench N" chip on the first request; the latest
  bench in the status bar; palette commands. 60 tests. Driven against a
  local server (`slow_server.py`, 5–80 ms, 2% 503s): CLI 500 at 20; the
  window 200 at 40/s held 39–40/s live, and Stop; an MCP agent's bench
  asked, allowed in the window, and ran under the agent's name.

- **Phase 4: GraphQL** (`crates/core/src/graphql.rs`, 2026-10-31 and
  2026-11-03). Method `GRAPHQL` as JetBrains writes it: the body is the
  query, then after a blank line its variables (a JSON object); kept so in
  the file, journal and history, sent as the JSON POST. Every GraphQL
  request gets the check "no GraphQL errors" (servers send errors with a
  200), so `HttpRequest::is_checked` now decides whether a client waits
  for `request_checked`. `Command::GraphqlSchema { request_id }` reads an
  introspection answer (`GRAPHQL_INTROSPECTION`; its `ofType` nesting is
  cut to fit an ~8-level depth limit, so lists of non-null items show
  their innermost type as `?`). CLI `tracer schema <URL|PATH>` (SDL); MCP
  `send_graphql`, `graphql_schema`. Window: a **GraphQL** rail item; the
  method shows as a label; Query and Variables tabs (edited apart, joined
  into `body`, so saving, history and Edit work unchanged); a Response /
  Schema switch under the editor; Schema loads by introspection (a request
  in History), lists types with roots first, filters by type or field
  name; Copy as cURL gives the JSON POST; palette commands. 66 tests.
  Driven against countries.trevorblades.com: saved request opened, sent
  (200, check 1/1), schema (10 types, filter "lang" 4 of 10), cURL run in
  a shell (Uganda), REST and back (body joined; back, it matches the file
  again). Found while driving: **AW-038** (a computed read in the handler
  that set its input is stale: the editor opened blank and edited),
  **AW-040** (a template string keeps `\n`: the window sent a literal
  backslash-n and got 400; the 64 KiB note had the same bug since Phase 1,
  fixed too), and **AW-039** from `rux check` (`try`/`catch` that both
  return "returns none").

- **Phase 4: realtime** (`crates/core/src/realtime.rs`, `ws.rs`,
  2026-11-03). A stream is a request that stays open, so history, policy,
  approvals, variables and secret redaction hold unchanged. `WEBSOCKET
  ws(s)://…` (JetBrains' method; body = messages to send on open, split by
  `===` lines, `=== wait-for-server`) does its own handshake over TCP and
  `rustls` (already in the lock through ureq, ring + webpki roots), timed
  like any request; a ping after 25 s quiet keeps it up. Any answer of
  `text/event-stream` is read as SSE (spec parser: CR/LF/CRLF, multi-line
  data, event, id, comments), on a reader thread so a close is prompt.
  Events `stream_opened` (head), `stream_message` (n both ways, dir, text
  cut at 64 KiB, binary as size + hex, SSE event/id), `stream_closed`
  (reason, counts), then `request_finished` (101/200, total time open,
  bytes in). `Cancel` on an open stream closes it ("closed by X"),
  including the kill switch; `StreamSend`, `StreamMessages {after,
  limit}`. A refused handshake (404…) is an ordinary answer. Benching a
  WebSocket is refused. `ws.rs` now holds the framing, SHA-1 and base64
  for the local API too. CLI `tracer ws URL [-m]…` (stdin lines sent, EOF
  closes unless `--for`/`--count`), and `send`/`run` print a stream live
  with `--for 10s` / `--count N`. MCP `open_stream`, `read_stream`
  (waits for one still connecting), `send_stream_message`,
  `close_stream`. Window: a **Realtime** rail item (method WEBSOCKET, URL
  to ws(s)://, Body tab becomes Messages), a Messages tab on any stream
  (↓/↑, n, time, event, sender; newest first, 500 shown), a composer on an
  open WebSocket (Enter), Close in place of Cancel, the closed reason and
  counts, palette commands. 73 tests, three runs clean. Driven: CLI and
  window against wss://echo.websocket.org (TLS, echoes, 70 s idle held
  with the ping; without it the server dropped it at 35–49 s, Node's
  WebSocket too), Wikimedia's recentchange SSE (CLI), a local ticker at
  5/s and a 1,000-event burst in the window (responsive, 0.6 s CPU),
  and an MCP agent: open asked for approval, allowed with `tracer
  approve`, read, sent, closed "closed by test-agent".

- **Phase 4: benchmarking finished** (`crates/core/src/baselines.rs`,
  2026-11-04). **Rate limits:** a bench counts 429s (and 503s with
  `Retry-After`) and reads `X-RateLimit-*` / `RateLimit-*` / `Retry-After`:
  `BenchStats.rate_limit` says how many, after how many requests and how
  far in the first came, the rate until then, the stated limit, the least
  remaining. **Comparison:** `BenchStats::deltas` (protocol) judges mean,
  p50, p95, p99 (worse past the tolerance, default 20 %, and 2 ms),
  throughput (worse past the tolerance), errors and limited (worse past 1
  point); `baselines::compare` adds notes (different request, env, plan;
  stopped early; held to a rate; rate limited, whose fast refusals flatter
  throughput). **Baselines:** `baselines/<name>.json` (request, env, plan,
  stats), `SaveBaseline`, `ListBaselines`, `CompareBench { baseline |
  other_bench, tolerance_pct }`, `baseline_saved` event. CLI `tracer bench
  PATH --env a --env b` (side by side), `--save-baseline`, `--baseline
  [--tolerance]` (exit 1 on a regression), `bench compare ID NAME|#ID`,
  `bench baselines`, a `rate` line in the summary. MCP `compare_bench`,
  `save_baseline`, `list_baselines`; `run_bench` takes `baseline`,
  `save_baseline`. Window: Bench panel's rate line, Save as baseline (name
  field), Compare with (baselines and earlier benches) and the comparison
  table; the panel and the Schema view now scroll (their parts are
  `flex-shrink: 0`; before, the panel's lower part was cut off and the
  histogram squashed to nothing). 77 tests, three runs clean. Driven
  against `bench_server.py` (fast 5 ms, slow 25 ms, limited 50 per 10 s):
  fast vs slow side by side (+300 % latency, -60 % throughput flagged);
  baseline from fast, same env "no regression" exit 0, slow exit 1;
  limited "first after 50 requests"; the window's compare and save; an MCP
  agent's list and compare.

- **Phase 4: the response viewer, merged and driven** (a922dfd,
  2026-11-04). The body pane was merged by hand (the template had been
  reindented). Driven on a local server (`python -m http.server 18765`
  over generated bodies): `small.json` numbered, coloured, folds give
  `[ … ],  2 items` / `{ … }  7 keys` with line numbers skipping, inner
  folds kept while the root folds and unfolds; a 3,000-record body
  (58,505 lines, 686 KB) shows "1000 of 58505 lines · Show 1000 more";
  an HTML page as its own lines. Scrolling 1,000 lines: 30 wheel steps,
  0.45 s CPU, no stall over 17 ms. **Show more grows with what is shown**
  (release build: 1.5, 2.3, 3.4, 4.5 s of CPU for 2k, 3k, 4k, 5k lines;
  about 110 MB per 1,000 lines); see Next, item 1.
- **Small layout fixes** (652b9df). The sidebar's remembered width gives
  way (to 200 px) before the work area (420 px); the method picker
  shrinks to 96 px; the title row's notice and the URL preview wrap; a
  response header's name column is `flex: 0 1 220px` down to 110 px.
  Driven at 1000 px wide with the sidebar at its 560 px. The Collections
  `+` and Save placement is AW-042, left as it is.
- **Follow-ups fixed.** Realtime keeps its messages and REST its body
  across the switch (`otherBody`), and the URL goes back to http(s)
  (808af61). A flow step's re-run is sent with the captures it had: the
  hub keeps them by request id, in memory only (62f1a93, with a test).
  A bench picked later (replayed, or from a request's chip) shows its
  tiles and its rate line: the window keeps each bench's last report and
  each report's count and time (ea0d60e).
- **Run-until** (02783e2 core/CLI/MCP, de4fe95 window). `RunUntil {
  request, env, plan: { streak, matching, max, gap_ms } }`: one at a
  time until `streak` answers in a row count (failures, or with
  `matching` an expectation they pass, so `status == 401`); any other
  answer resets; at most 1000 by default (100,000 cap); every send is an
  ordinary request in history. Events `until_started`, `until_progress`
  (names each request, `counted`, `streak`), `until_finished` (`reached`
  or `stopped`). Stop: `StopUntil`, and the kill switch stops an agent's;
  a restarted core closes a run it left. CLI `tracer until <PATH> -n 4
  --match 'status == 401' --every 500ms` (exit 0 when the streak came);
  MCP `run_until`/`stop_until`; window: "Until n in a row" under the
  bench. A test (`a_run_until_stops_at…`), and driven from the CLI, an
  MCP agent and the window against `status_server.py` (scratchpad;
  port 18766, `/reset` restarts its 200, 401, 401, 500, 401 x4).
- **Browser behaviour, opt in** (6f6a1d1). `Browser { cookies, origin }`
  in `.tracer/browser.json`, set by a person (`SetBrowser`; window chip
  in the status bar, `tracer browser cookies on origin URL`). Cookies:
  `cookies.rs`, an RFC 6265 jar in `.tracer/cookies.json`; with it on,
  `run_one` follows redirects itself a hop at a time
  (`send_keeping_cookies`, 20 at most) so a 302's `Set-Cookie` is kept.
  Values stay out of the journal and away from agents (`Cookies` gives a
  person values; `list_cookies` over MCP gives names). CORS: `cors.rs`;
  with an origin, cross-origin requests carry `Origin`, ones a browser
  would preflight get their `OPTIONS` first as a request of their own
  (it takes the next number, so it lists above the request it preceded),
  a refusal fails the request "blocked by CORS: preflight #n: why", and
  each cross-origin answer gets a "CORS from <origin>" check. Tested
  (`browser_behaviour_…` and unit tests) and driven against httpbin.
  Not done: SameSite isn't enforced (no notion of a top-level site), and
  a browser's own headers (User-Agent) count as script-set ones.
- **Unchecked TLS for named hosts** (bc2ff7e, 2026-11-05).
  `Browser.unchecked_tls` (`localhost`, `*.dev.test`, `*`), set by a
  person: those hosts' certificates aren't checked (ureq's
  `disable_verification`; WebSockets get a rustls verifier that still
  checks signatures). With any set, redirects go a hop at a time
  (`send_hop_by_hop`, was `send_keeping_cookies`) so each host is judged
  on its own; a bench to such a host doesn't follow redirects. Answers
  say `tls_unchecked`. CLI `tracer browser tls HOSTS|none`, a line after
  the response; MCP `tls_unchecked`; window: field in the Browser panel,
  amber `… · TLS unchecked` chip, "TLS not checked" on the response.
  Driven against a self-signed `localhost:18443` (scratchpad `tls/`):
  refused before, 200 after, a redirect to 127.0.0.1 still refused
  ("redirected to …: invalid peer certificate").
- **Export and import** (1571661). `export.rs`: curl, fetch, Python
  (requests), Rux (a native export with ureq plus the page's call: Rux
  script has no HTTP of its own, by design). Variables from the env,
  secrets stay `{{name}}`, a trusted-unchecked host gets `--insecure` /
  `verify=False` / `disable_verification`. `import.rs`: curl in bash or
  cmd quoting (cmd is read as cmd then the C runtime's argv rules), and
  fetch calls via a small JS-literal parser that keeps key order (no
  `preserve_order` in serde_json here). Notes name what's left out
  (`-F`, `-T`, `-k`, `credentials: "include"`). CLI `tracer export
  <PATH|#ID> --as …`, `tracer import [FILE] --save`; MCP
  `export_request`, `import_request`; window: **Copy as…** menu (the
  page's own curl builder is removed) and the palette's **Paste a curl
  command or fetch call**. Driven: a Chrome-style curl imported, exported
  and run as curl, Node fetch and Python (a stand-in `requests` over
  urllib: `requests` isn't installed here) to 201; the Rux export
  compiled inside `app/native` (then removed).
- **Re-run on file change** (cf3a26d). `tracer run PATH --watch` (the
  request, or a folder's flow, again on each change to its file, a file
  under the folder, or its env); the window's **↻ re-run on save** chip
  per open request (a save here, or an outside one it follows).
- **Explain** (3271f47). `explain.rs`: one request as Markdown, a verdict
  line, request as written, env vars (secrets named only), answer with
  phases and body (8 KiB), checks, browser settings, and the last time
  the same method + URL answered < 400 with checks passing (`tracer diff
  A B`). CLI `tracer explain [ID]` (newest gone wrong by default), MCP
  `explain_request`, window **Explain** link (copies) and palette.
- **Local servers** (17aba45). `discover.rs`: listening ports from
  `netstat -ano` (or `/proc/net/tcp`; the usual dev ports otherwise),
  below 1024 skipped but 80/443, process names from `tasklist`, each
  tried HTTP then HTTPS (1.5 s budget), OpenAPI/Swagger at 8 usual paths.
  URLs use the address that answered: `localhost` costs 2 s here (::1
  first). CLI `tracer servers`, MCP `local_servers`, window palette
  **Find servers running on this machine**.
- **Data-driven flows** (ee28163). `data.rs` reads a CSV (header line,
  RFC 4180) or a JSON array of objects inside the workspace; `RunFlow
  { data }` runs the folder once per row, the row's columns as starting
  variables (captures reset per row; a failure skips only its row).
  `FlowStep.row`, `FlowStarted { data, rows }`. CLI `tracer run DIR
  --data FILE` (progress lines now name each request as written: a
  skipped step starts none, so names by order were wrong), MCP
  `run_flow data`, window palette **Run the open request's folder once
  per row…** with a file prompt; status bar `flow N passed (2 rows)`.
  Variables go into URLs as written, not encoded (a value with a space
  fails "invalid uri character"). Also: `status 200` now reads as
  `status == 200`; status-bar chips are `flex-shrink: 0`.
- **Mock server** (d363488). `mock.rs`: a folder's saved requests as
  routes on 127.0.0.1 (path after scheme+host or a leading `{{var}}`;
  `{{var}}` pieces match 1+ chars inside a segment; most literal text
  wins), each answering with its last real answer (last < 400 first,
  `hub.last_answer`), 501 with none yet, 404 listing routes; CORS open
  and OPTIONS answered; looked up per hit; `mock_hit` events. CLI
  `tracer mock DIR [--port]` / `tracer mock stop ID`, MCP `create_mock`
  / `stop_mock`, window palette **Mock the open request's folder** and a
  chip (`mock :PORT · N hits · last … · stop`).
- **Streams** (0bace2c, 38dbff7). `tracer ws send ID MSG`; `Command::
  Transcript` (`transcript.rs`, Markdown both ways), `tracer ws
  transcript ID`, MCP `stream_transcript`, window **Transcript** link.
  An event stream whose server sent `retry:` reconnects with
  `Last-Event-ID` inside the same stream (`Link.resume`), ending on 204,
  a non-stream answer, 5 failures in a row, or a close.
- **MCP over HTTP** (0efbb36). `mcp_http.rs`: `tracer mcp --http PORT`,
  Streamable HTTP on 127.0.0.1/mcp (POST, JSON answers, 202 for
  notifications, sessions by `Mcp-Session-Id`, foreign Origin 403).
  Driven with curl and Claude Code's http MCP client.
- **Cookies** (58f7724). `Browser.jar_per_env` (opt in); `cookies.json`
  holds `{"by_env": …}` and still reads the old single jar;
  `CookieSeen.env`, `ClearCookies { env }`. Response **Cookies** tab
  (Set-Cookie parsed; the request's own Cookie header; jar cookies are
  added at send time and not in history). `shownTab()`: a chosen
  response tab that doesn't apply falls back to Body.
- **GraphQL subscriptions** (9d0ddb0). A `subscription` query goes over
  a WebSocket with `graphql-transport-ws` (http→ws, `connection_init`,
  subscribe after the ack via `=== wait-for-server`), pings answered,
  `complete` closes it, the errors check runs over its messages. Export
  refuses one.
- **Flow benches** (7a72bcd). `RunBench { vars_of }` (a past request's
  captures); `tracer bench FOLDER` runs the flow once, benches each
  passing step with its captures, table side by side, baselines per step
  `NAME--<step>`.
- **Bench connections** (c81b04b). A worker starts a fresh connection
  after an HTTP/1.0 answer without keep-alive or `Connection: close`;
  before, half a bench against Python's http.server failed "Peer
  disconnected".
- **Themes** (8622038). All page colours are `var(--c-<darkhex>)`; light
  set generated by `theme.py` rules (scratchpad; regenerate if colours
  are added). Palette Theme: dark/light/system, kept in settings. Switching
  from the palette hits **AW-043** (root background one change behind);
  started in a theme, it's right.
- **OpenAPI** (889fe9e core, CLI, MCP; 27de06b window). `openapi.rs`
  reads 3.0, 3.1 and Swagger 2 (JSON or YAML via serde_yaml; YAML number
  keys become text) and keeps the document under `specs/<name>.<ext>`
  with `<name>.from`. Import saves a request per operation that has none
  under `requests/<name>/<tag>/<summary>`, URL `{{base}}…`, required
  query/header params, security as `{{token}}`/`{{apiKey}}`, a body made
  from the example or schema, `# @expect status …` and `# @expect spec
  <name>`; with an env, `base` = the first server (a relative server
  taken against the document's URL). Imported again: what changed, kept
  requests, ones no operation matches. `schema.rs` checks JSON against
  the schema subset (refs, types, nullable, enum, const, all/any/oneOf,
  not, object/array/string/number keywords, formats, `pattern` via
  regex-lite); fields the schema doesn't name are drift, not failure.
  The `spec` check runs wherever checks run (sends, benches, run-until).
  Drift reads the whole history in one pass (`Hub::answered`). The spec
  mock (`mock.rs` now has a `Source`) checks params and body (400/422),
  answers from examples, `Prefer: code=` / `?__code=`; the server's base
  path is taken off. CLI `tracer spec import|ls|check|drift`, `tracer
  mock --spec`; MCP `import_spec`, `list_specs`, `check_spec`,
  `spec_drift`, `create_mock` with `spec`. Window: rail **API** (import
  form, spec tabs, drift tiles and rows; a row opens its saved request,
  `#N` shows the failing one), palette import/check/mock. Driven against
  a drifting local API (scratchpad `api/server.py`, port 18796).
- **Schema-aware GraphQL query** (f296b3c). `gqlcheck.rs` parses the
  query and checks it against the loaded schema; problems at line:col
  with suggestions, nearest first; a misspelt name's suggestion replaces
  its bytes (`tracer.replaceRange`). Introspection now asks for
  `defaultValue`, so an argument with a default isn't required. The
  parsed schema is cached by request id (`graphql::cached_schema`): the
  window checks on every change. MCP `check_graphql`. Driven against
  countries.trevorblades.com. Completion at the caret and highlighting in
  the field are AW-046.
- **Editor undo** (97b2914). Tracer's own history of the request
  (method, URL, headers, checks, body, query, variables), a step per
  change, typing in one place within a second one step; Ctrl+Z / Ctrl+Y
  / Ctrl+Shift+Z on the screen's `@key` (only reached with no field
  focused), palette Undo/Redo, a notice naming what changed; `load` and
  `newRequest` start a new history. Field-level undo is Rux's (AW-047).

### Next

1. **Why Show more costs more each time** (not settled, 2026-11-04).
   Each click re-sends every shown line from `view.bodyView` and replaces
   `bodyLines`. Measured so far: the cost is the same in debug and
   release, and the same in probe 2 (`probes/pages/json-probe.rux`, now
   with 5k buttons) whether the 5,000 lines come from script, a sync
   export or an async one (2 to 4 s each). A bare page in `rux.exe`
   (scratchpad `rows/`) creates 5,000 rows in about 1.6 s of CPU, but
   replacing them with an equal list costs nothing (no frame) and
   appending 1,000 costs 0.3 to 0.5 s; probe 2's equal replace instead
   re-runs every row's bindings (25,000 calls) and costs 1.9 s.
   `RUX_PROFILE=each`: the frame that adds 5,000 rows is ~1.15 s (patch
   0.36 to 0.54 s, layout 0.54 to 0.78 s), and the next two or three
   frames each lay the whole list out again (210 to 240 ms) with nothing
   patched. In Tracer, frames account for only ~0.65 s of a 4.9 s click.
   `content-visibility: auto` on the rows changed nothing (tried,
   removed). Ruled out since (2026-11-04 night, on the no-native copy of
   probe 2, scratchpad `p2/`, bisected by `bisect.sh`): where the pointer
   rests (hover), the list box's radius, border, padding, background and
   overflow, the per-row `:style` and `:class`, and the `note` writes.
   What moves it: text elements a row (one fewer text, about 40 % less).
   Still unexplained: on `rows/`, replacing 5,000 rows with equal ones
   costs 0.3 s and clearing them 0.23 s; on `p2/` the same costs 2.1 s and
   1.5 s, and its profile shows every row's bindings re-run (25,000 to
   75,000 `run` calls) on those frames. Also ruled out: the window's size
   (rows/ costs the same at 900x700 and 1600x900), rows/ clearing through
   a function rather than inline (still 0.22 s), and the row markup: p2
   with rows/'s exact rows still costs 2.9 s to replace and 2.1 s to
   clear. So it's the rest of p2's page (heading, paragraph, the button
   row holding `note`; `.page` padding and gap) or its data (every row's
   `key` the same text; built in a nested loop from a parameter). Next:
   move those into rows/ one at a time, then write the finding. Also seen: in
   `rows/`, a row whose fold text is "" is ~3 px taller than one with ▾.
2. Realtime follow-ups, untried: GraphQL subscriptions (graphql-ws over a
   WebSocket). GraphQL follow-ups:
   a schema-aware query editor (needs the viewer's line work).
   Benchmarking follow-up: comparing a flow's steps.
4. MCP over local HTTP (the plan's "stdio or local HTTP"), if a client
   wants it.
5. The user's three features (2026-11-04) are done (see Done). My
   reading of run-until, told to the user: with a named kind, any other
   answer, a 500 too, resets the streak. Follow-ups: a per-environment
   jar; a response's own Cookies tab (the plan's).
6. ~~The Bench baseline name field taller than its row~~: done
   2026-11-05; the row's name and compare fields share a 28 px box.
7. **Not built yet from PLAN.md** (2026-11-05, afternoon): gRPC (a
   stretch). Phase 5's chat waits for release time (the user's call,
   2026-11-05). Waiting on Rux: completion at the caret and highlighting
   in the query field (AW-046); when AW-044 is fixed, move the API view's
   signals back beside their functions.
9. ~~**A mock from a dead core shows as serving**~~ (done 2026-11-05): the journal replays
   `mock_started` and nothing says it stopped when that core went (seen
   2026-11-05: the status bar's mock chip after a restart). Close mocks
   as dangling requests are closed when a core opens a journal. S.
8. **Borrowed from an outside review** (a ChatGPT read of `phase-1-core`
   at 2dfc0f4, source only, nothing run; noted 2026-11-05, not
   scheduled, the user's call). It brings no new features; its feature
   gaps are the ones above, most now done. What to carry, each checked
   against the code:
   - **Journal write errors are dropped**: `let _ = writeln!(inner.
     journal, …)` in `publish_locked` (hub.rs), so a client can see an
     event the journal never got, despite the comment above it saying
     otherwise. Make the failure loud (an event, or refuse to publish). S.
   - **Nothing is ever let go**: `Inner.events` keeps every event,
     bodies included, and history, `last_answer` and `since` scan it
     whole. Later: segment the journal, page old events, keep a
     compacted snapshot, and set a retention rule for large bodies. M-L.
   - **Slow clients**: each subscriber's outbox is an unbounded
     `mpsc::channel()` (hub.rs ~726, ~839), so a stuck client grows the
     core's memory without limit. Bound it, and drop or resync a client
     that falls behind (it can replay with `since`). S-M.
   - **A thread per connection** (conn.rs) with no cap or idle timeout.
     Add a connection limit and idle timeouts. S.
   - **Tokens through `RandomState`** (lib.rs ~168, ws.rs ~108). std
     seeds it once per thread and then steps the key, so it's weaker
     than the comment claims. Read OS randomness directly (getrandom /
     `BCryptGenRandom`). S.
   - **Disk rescans every second** (`DISK_SCAN`, lib.rs ~117) instead of
     file-change notifications. It's fine at today's sizes; revisit if
     workspaces get big. M.
   - **Show more** re-walks the layout from the top on each page
     (view.rs): the same thing as item 1, from the other side.
   - **app.rux** (3,593 lines now) wants splitting into modules. Check
     first what Rux's module support allows; a missing piece is a
     finding.
   - **The root `Cargo.toml` patches `rux-native` to an absolute
     `C:/Users/USER/…` path**, so no other checkout builds. Use a
     relative path, a git dependency, or a documented override.
   - Open questions it raised: `PROTOCOL_VERSION` is reject-only with no
     negotiation; policy types live in the wire crate; the protocol and
     the local HTTP/WS API are two surfaces that must stay in sync.
   - For Rux (stress tests, not Tracer work): async native calls,
     channel delivery under sustained traffic, effects over large
     lists, live SVG charts, large-response memory, and native process
     shutdown. Drive the ones the findings don't already cover.
   - For Mava (not Tracer): reuse the patterns (a single state owner,
     explicit commands, ordered events, many clients, a native boundary,
     state that can be rebuilt), not `Hub` itself.

## Gotchas and how they were solved

- **On Windows a read timeout set through `TcpStream::try_clone()` doesn't
  reach the socket** (the clone is a duplicated handle with its own
  options): clearing the hello timeout that way left every client cut off
  10 s after connecting, the window showing "core lost: the core hung up".
  A watchdog that calls `shutdown` (which acts on the socket) replaced it.
  Test quiet clients for longer than any deadline.

- **Bash heredocs with apostrophes in the text fail** in this tool ("unexpected
  EOF while looking for matching `''") even with `<<'EOF'`. Write the
  patch script with the Write tool into the scratchpad and run it.
- **A textarea's tap target is its first line** when the box around it is
  taller: tap on the placeholder's line, not the middle of the box.
- **A `.rux` edit hot-reloads the page**, which resets script state (the
  rail goes back to REST): re-open the view before the next tap.
- `now()` is a `float`; a signal holding it needs `signal(0.0)`.

Things that cost a round trip once. Check here before debugging them again.

### Building and running

- **`rux run` in an app with a `native/` crate compiles the Rux crates from
  the checkout as it is right now**, including uncommitted, half-done work.
  That is what produced the false AW-017 (2026-10-29). Before measuring a Rux
  fix, run `git -C "C:/Users/USER/Mobile/UI Research" status --short`. If it
  isn't clean, say so in the result, or wait. The log's `Compiling rux-…`
  lines show it rebuilt them.
- **`rux` on PATH can be older than the Rux checkout.** `~/.cargo/bin/rux.exe`
  was a day behind the Channel commit while the checkout's
  `target/release/rux.exe` had it. Compare dates (`ls -la` both) before
  testing a Rux fix. The user installs new builds; when they say a new build is
  in, the PATH `rux` is the one to use.
- **`rux-native` is patched in the workspace `Cargo.toml`** to the local Rux
  checkout. Do not add a path or git dependency in `probes/native/Cargo.toml`:
  that gives the app a second `rux-native`, and so a second export registry.
- The first `rux run` compiles `native/`, which takes a minute or two. After
  that, `.rux` edits hot reload.
- Stopping the app (`Stop-Process -Name tracer-probes`) makes a backgrounded
  `rux run` exit with code 1. That is expected, not a crash.
- Git warns "LF will be replaced by CRLF" on edited files. It is harmless.
- **Git Bash rewrites `/json` into `C:/Program Files/Git/json`**, so
  `rux run --route /json` silently opens on `/`. Prefix the command with
  `MSYS_NO_PATHCONV=1` (or run it from PowerShell).
- A release build is `rux build --release`, which writes
  `probes/dist/tracer-probes.exe`; `rux run` has no `--release`. On desktop the
  profiler is read when the app starts, so launch it as
  `RUX_PROFILE=1 ./dist/tracer-probes.exe` or it prints nothing.
- `RUX_PROFILE=1` prints its summary every 60 painted frames into `rux run`'s
  own output, so redirect that to a file and read it there. An idle window
  paints nothing, so it prints nothing.

- **The window's exe is `tracer.exe` too** (named after `rux.toml`'s app
  name), the same as the CLI. On 2026-10-30 `tracer_bin()` found the window
  itself "beside this program" and started it as the core, which did the
  same: ~19 copies at 600 MB each before it was caught. Fixed: `tracer_bin()`
  skips the running exe, and a core the window starts carries
  `TRACER_STARTED_BY_WINDOW`, which refuses to start another. When launching
  the window, set `TRACER_BIN` to `target/debug/tracer.exe` and run a
  watchdog that kills `tracer` past 3 processes.
- **`target/` was emptied around 09:03 on 2026-10-30** (not by a session
  command), which removed `target/debug/tracer.exe`; the window then said
  "could not start …tracer.exe". `cargo build -p tracer-cli` before
  launching.
- The display is scaled 150%: screenshot pixels are 1.5x layout pixels, so a
  300px sidebar ends at x = 96 + 450 in a screenshot.
- A `rux run` started with `( … &)` inside a Bash call does **not** die with
  the call: its window lives on, unseen, holding ~565 MB (one from 08:05 was
  found at 09:40 on 2026-10-30). Use the tool's background mode, and check
  `Get-Process rux` when done.
- `rux run` sometimes exits with code 127 and no message while its window
  keeps running; hot reload still works (the window watches the files).
- **`rux run` launches the window even after a failed build**: it waits for
  a change under `native/`, rebuilds, and launches. If Rux's checkout is
  mid-edit the first build fails; check `git status` there first.
- **A dev build's hot reload runs `unmounted` and then `mounted`.** Rust
  statics survive it. Don't put "the window is closing" work in `unmounted`;
  use `@close`.
- **Closing a window from a script: refresh the handle first.** For its first
  few hundred milliseconds a Rux window's `MainWindowHandle` is a temporary
  window, and a `CloseMainWindow` sent to it is lost. `early3.ps1` in the
  2026-10-30 scratchpad waits for the handle to change.
- Force-killing the window (`Stop-Process`) skips `@close`, so its core
  lives on. Clean up with `tracer stop -w <ws>`, or kill the
  `target\debug\tracer.exe` processes.
- An app with no `native/` crate runs inside `rux.exe`, so its process is
  named `rux`, not after the app.

- **A second window while another runs:** `rux run` builds, then fails
  copying over the locked `app/dist/tracer.exe` and waits. Stop it and
  start the build it made, `app/.rux-build/desktop/target/debug/tracer.exe`,
  with `-WorkingDirectory app` and `TRACER_WORKSPACE`/`TRACER_BIN` set (no
  hot reload: restart it after `.rux` edits). Drive it with `drive.ps1 -Id
  <pid>`, which otherwise picks the first `tracer` window. A watchdog must
  skip PIDs that were already running.
- **The user's `rux run` rebuilds and relaunches its window when
  `app/native` changes**, even one started days ago: editing `tracer.rs`
  replaced the user's window (2026-11-03 22:47). Expected, but say so.
- **A running core of the user's holds `target/debug/tracer.exe`**: rename
  it (`mv target/debug/tracer.exe target/debug/tracer-running-<date>.exe`;
  Windows allows renaming a running exe) rather than stopping their core.
  This session's own cores must be stopped before each `cargo build`
  (`tracer stop -w <ws>`; `tracer mcp` starts one too).
- **`drive.ps1 -Do move` resizes the window** (to `-X2`×`-Y2`, so 0×0 if
  left out); `hover` moves the pointer, `wheel -X -Y` scrolls at a point.
- **The window launched straight from `.rux-build` still hot reloads**
  `app.rux`, which resets page state (mode, scroll): re-navigate after an
  edit before clicking.
- **Port 8765 is taken** by a `SimpleHTTP` server from 2026-10-30, which
  answers 127.0.0.1 although the scratch server bound it too; test servers
  use 18765.
- Public SSE for a check: `sse.dev` doesn't resolve on this network;
  `https://stream.wikimedia.org/v2/stream/recentchange` (with a
  User-Agent) works and is fast, so use `--count`.
- **Patch scripts against `app.rux` break when a formatter reindents it**
  (2026-11-03: the template went from 2 to 4 spaces a level). The
  fix used then: match anchor lines by trimmed text (the last one as a
  prefix), and scale the replacement's indent by the file's indent step
  over the anchor's.
- **Scripted edits: many files here are CRLF.** A Python replace written
  with `\n` silently doesn't match (that cost two rounds on 2026-10-30).
  Read with `newline=''` and swap `\n` for the file's own line ending, or
  use the Edit tool.
- **Trying `tracer mcp` with Claude Code without touching its settings:**
  `claude` isn't on PATH; the extension's binary is
  `~/.vscode/extensions/anthropic.claude-code-<ver>-win32-x64/resources/native-binary/claude.exe`.
  Run it from the scratchpad with `-p --model haiku --max-budget-usd 0.5
  --strict-mcp-config --mcp-config <json> --allowedTools mcp__tracer
  --output-format stream-json --verbose`, and `< /dev/null` (it waits 3 s
  for stdin otherwise). Stop the core it starts with `tracer stop`.
- **Slow or failing sends here are the network, not Tracer** (2026-10-30
  night). Measured from a bare Rust program: the system resolver fails
  after 11 s ("No such host is known") or takes 3 s, and TCP connects to a
  known IP take 3 s or 17 s (lost SYNs, retried). IPv6 lookups fail for
  every host (no IPv6 here). curl meets the same (connects of 2.4 and
  3.2 s); a few quick curl runs had first misled me into blaming Tracer's
  resolver. Measure resolve and connect apart before blaming anything.

- **A Rux app with no native crate runs inside the `rux` process**: its
  window belongs to `rux.exe` (title = the app's name), not to an exe of
  its own. Find it with `Get-Process rux | ? MainWindowTitle -eq <name>`.
  A minimal page for a finding needs only `rux.toml` and `app.rux`.

### Driving the window

- **Bench against a local server, not jsonplaceholder**: the network here
  is slow and a public API shouldn't be hammered. Python's
  `ThreadingHTTPServer` has a listen backlog of 5, so 20 workers
  connecting at once see ~1 s connects (SYN retries); that's the server,
  not Tracer.
- A running core holds `target/debug/tracer.exe`: `cargo build` fails
  "Access is denied" until it's stopped.

- **When the display is asleep, `drive.ps1 -Do shot` gives a black image**
  (it copies the screen). Use `-Do print` (PrintWindow, flag 3), which asks
  the window for its own pixels. Clicks and keys still work with the
  display asleep.
- There's no `~/Tracer` workspace on this machine. Make one in the session's
  scratchpad, `tracer core -w <ws>` in the background, seed it with
  `tracer send`, and launch the window with `TRACER_WORKSPACE` set to it;
  `tracer stop -w <ws>` at the end.
- A select's dropdown near the window's bottom is cut off (AW-032): open
  selects high in the window when testing their last options.
- **Acting as an agent:** the scratchpad's `mcp_once.py TRACER WS TOOL
  'JSON'` (2026-10-30) makes one MCP tool call through `tracer mcp`. Run
  Python with `-u` when another process reads its output, or it sits in a
  buffer. `mkfifo` doesn't work in Git Bash; use a flag file to sequence.
- The Bash tool breaks a heredoc whose text has an apostrophe ("unexpected
  EOF while looking for matching `'`"). Write the script to the scratchpad
  and run it from there.
- A running core holds `target/debug/tracer.exe`, so `cargo build` fails
  with "Access is denied" removing it: `tracer stop` first.

- Drive it yourself with `tools/drive.ps1` (`move`, `shot`, `click`, `drag`,
  `keys`, `rect`); don't hand the user a checklist. Process name:
  `tracer-probes`.
- Run `-Do move -X 0 -Y 0 -X2 1280 -Y2 800` first. Coordinates are
  window-relative physical pixels, as the screenshot shows them.
- **The dev overlay covers the app when there are warnings or errors**, and
  the first click only dismisses it. Keep `rux check` clean (it now runs the
  same checks as the running app) and tap the panel once if it shows.
- Probe 1's burst buttons sit at about (458, 207), (662, 207) and (881, 207)
  at 1280×800. Probe 2's buttons **move** when its note text changes width,
  so take a screenshot before each click there.
- **Park the pointer** (`-Do hover -X 1240 -Y 720`) after each click when
  testing redraws. A hover over anything with `:hover` forces a frame, and
  that can hide a missed redraw.
- To type with gaps, send one key per `SendKeys` call, but treat `{BS}`,
  `{ESC}` and the like as one token: splitting `{BS}` into characters types
  the letters `BS` (that was the false "lost backspaces" of 2026-10-29).
  `-Do keys -Text "{BS 30}"` clears a field.
- `drive.ps1 -Do wheel` with a negative `-Text` (scroll down) failed before
  2026-10-29: in PowerShell 5.1 `0xFFFFFFFF` is the Int32 `-1`, so masking
  doesn't make it unsigned. It now reinterprets the bytes.
- Watch memory and `Responding` while driving: `Get-Process tracer-probes`,
  `WorkingSet64` and `CPU`, polled every 0.5 s with a 3 GB cap that kills it.
- `rux run --route /x` works for apps with a `native/` crate since Rux
  b29c7b4. Before that it was ignored (AW-010) and you had to navigate by
  clicking.

### Writing Rux

- **A value Rust sends that must each arrive → `rux::Channel<T>`, not
  `rux::Signal<T>`.** A signal keeps only the newest value. A channel reads in
  script as `T[]`, the values pushed since the last frame, and an `effect`
  that reads it runs once per non-empty list. Between arrivals it still holds
  the last list, so loop over it inside an `effect` and keep what you need;
  don't bind to it and expect history. See `docs/07-script.md`, "Channels from
  Rust", in the Rux checkout.
- An exported signal's `T` needs `#[derive(Clone)]`.
- **Don't read a native signal in a `computed` or a top-level `let`**
  (AW-019); read it in a `fn` called from the template or a handler, or in an
  `effect`.
- `reverse()` and `replace()` return nothing and change their receiver
  (AW-020). Use `sort((a, b) => b.id - a.id)` for newest first, and `a + [x]`
  for spread. (Fixed at runtime since Rux a2c986b; the installed `rux check`
  may still disagree.)
- **Since Rux a2c986b, `/` always gives a `float`**, as in JavaScript:
  `503 / 100` is `5.03`. Inside a template string nothing complains, so
  `${n / 1024}` silently prints `1.0742…`. Use `intDiv(a, b)` for whole
  numbers (cost a round trip on 2026-10-30: the 5xx filter matched nothing,
  and `size()` had been printing long decimals).
- A `computed` read in the handler that just wrote its input is current
  since Rux bba2b04 (AW-038), and a template string decodes `\n`, `\t`
  and the rest since f752761 (AW-040). Tracer's workarounds for both are
  gone.
- **Writing a backslash through the tools:** the Bash tool's heredocs
  (even quoted `'EOF'`) and `printf` turn `\\` into `\`, and a backslash-u
  escape typed in any tool's input (Edit, Write, a command) arrives as the
  character it names (the original AW-040 entry lost its example that
  way). Write files with backslashes using the Write or Edit tool, and
  build a backslash-u sequence from parts (`printf '\134'` + `u0041`).
  A `\b is not an escape` from `rux check` on a test page was this, not Rux
  (2026-11-04).
- In text, write `{{ x ?? "" }}` with plain quotes; `&quot;` is only for
  attributes (AW-021).
- Annotate event handler parameters (`fn onKey(e: KeyEvent)`,
  `DragEvent`, `TapEvent`, `SwipeEvent`; no `use` needed) or `rux check` warns.
- Ruled out in Phase 0 as Tracer's own mistakes, not Rux's: a flex item grows
  to its longest unbreakable line (CSS does the same); an overlay is not
  centred without `display: flex`. Ruled out 2026-10-30: probe 2's button
  row overflowing at 978×984, with a horizontal scrollbar on the stage once
  the note shows. Edge with the same CSS overflows by 58 px, Rux by about
  61; items don't shrink below their longest word.

### Reporting

- Tracer reports Rux limits; it doesn't work around them. Each one is an
  `AW-0xx` entry in FINDINGS.md (Found / Severity / Limit / Reproduction /
  Workaround / Suggested fix / Status), following the field-report format in
  the Rux repo's `docs/field-reports/`.
- The Rux team writes the Status lines in FINDINGS.md itself. When it reports
  a fix, re-drive the probe and add the measured result to that entry.
