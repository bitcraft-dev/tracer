# Tracer: progress log

Where the work stands, what comes next, and what tripped us up on the way and
how it was solved. Read this first in a new session; update it at the end of
each one. Rux's own limits go in [FINDINGS.md](../FINDINGS.md), not here; this
file is about working on Tracer.

## Where it stands (2026-10-30, evening)

Remote `origin` is https://github.com/bitcraft-dev/tracer (pushed 2026-10-30): `master` holds Phase 0, `phase-1-core` everything since, and both track `origin`. Push when the user asks.

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

### Next

1. When AW-035 is fixed: re-drive probe 2 at about 1000×1040. Tracer's
   response body is one text, but its Headers tab is flex rows (`.hdr`), so
   a long header value in a narrow pane may hit it: check that too.
2. Phase 3 remainder: try `tracer mcp` from a real client (`claude mcp add`,
   README); the local HTTP/WS API for anything that isn't MCP; per-agent
   permissions (read-only / local env only / anywhere) and rate and size
   caps, from a workspace policy file rather than the fixed rules in
   `guard.rs`.
3. Small: the Collections `+` doesn't sit at the right edge; the title row's
   notice is cut short; the preview cuts long URLs; the env picker shows a
   remembered env the open workspace doesn't have (seen: "typicode" in a
   fresh workspace), and Send would use it.

## Gotchas and how they were solved

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

### Driving the window

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
- In text, write `{{ x ?? "" }}` with plain quotes; `&quot;` is only for
  attributes (AW-021).
- Annotate event handler parameters (`fn onKey(e: KeyEvent)`,
  `DragEvent`, `TapEvent`, `SwipeEvent`; no `use` needed) or `rux check` warns.
- Ruled out in Phase 0 as Tracer's own mistakes, not Rux's: a flex item grows
  to its longest unbreakable line (CSS does the same); an overlay is not
  centred without `display: flex`.

### Reporting

- Tracer reports Rux limits; it doesn't work around them. Each one is an
  `AW-0xx` entry in FINDINGS.md (Found / Severity / Limit / Reproduction /
  Workaround / Suggested fix / Status), following the field-report format in
  the Rux repo's `docs/field-reports/`.
- The Rux team writes the Status lines in FINDINGS.md itself. When it reports
  a fix, re-drive the probe and add the measured result to that entry.
