# Tracer: progress log

Where the work stands, what comes next, and what tripped us up on the way and
how it was solved. Read this first in a new session; update it at the end of
each one. Rux's own limits go in [FINDINGS.md](../FINDINGS.md), not here; this
file is about working on Tracer.

## Where it stands (2026-10-29)

Branch `phase-1-core`, on top of `main`'s two commits. Nothing is pushed.

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

### Next

1. **Waiting on Rux**: AW-017 (blocks every field and request), AW-018, and
   the component patch path and `:d` patching, which the Rux team is working
   on (2026-10-29). When a fix lands, re-drive the probe it names and add the
   numbers to that entry. Probe 2 should also be measured in a release build
   (`rux run --release`).
2. **The window shell as a client of the core** (the rest of Phase 1). Its
   native crate uses `tracer-protocol`'s blocking client, and core events
   reach the page through a `rux::Channel` (now possible: AW-011).
3. Phase 2 and after, on the non-Rux side, in this order:
   - requests and collections saved as plain files in the workspace;
   - an MCP bridge (`tracer mcp`) so an agent can drive Tracer;
   - a per-request timing breakdown (DNS, connect, TLS).

## Gotchas and how they were solved

Things that cost a round trip once. Check here before debugging them again.

### Building and running

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
- `RUX_PROFILE=1` prints its summary every 60 painted frames into `rux run`'s
  own output, so redirect that to a file and read it there. An idle window
  paints nothing, so it prints nothing.

### Driving the window

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
- **While AW-017 is open, an idle window shows stale state.** After a key or an
  `await`, hover a nav link (anything with `:hover`) and wait a second before
  the screenshot, or you'll be reading the frame from before. Hovering empty
  space doesn't force a frame.
- Type with a gap between keys (a loop of single `SendKeys` calls) when
  testing fields. Even so, while AW-017 is open only the last key lands.
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
