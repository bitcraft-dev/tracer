# Field report: Tracer

Tracer is an API workbench whose window is written in Rux, with its I/O in
Rust behind a `native/` crate and its core in a separate process. Its main
purpose is to strain Rux: every limit it hits is recorded here as it is found,
in the shape of `docs/field-reports/mava-studio-2.md` in the Rux repository,
so entries can be copied there as they are.

**Tracer's rule: where Rux is limited, Tracer does not work around it.** An
entry's workaround is "none" unless the language's own documented path covers
it. A feature that is blocked says so and waits.

Each entry says what Tracer needed, what happened, how to reproduce it, a
suggested fix, and its status. Severity is **blocker** (a Tracer feature
cannot be built), **friction** (it can, but Rux makes it harder or wrong-ish),
or **papercut** (docs, wording, diagnostics).

Rux under test: `rux 0.9.0-dev`, checkout at `C:\Users\USER\Mobile\UI Research`
(`de81f5c`), Windows 11.

## AW-001 `rux::Signal<T>` requires `T: Clone`, undocumented, and the error names a private method

- **Found:** 2026-10-29, Phase 0 probe 1 (ordered events).
- **Severity:** papercut.
- **Limit:** an exported `pub static X: rux::Signal<T>` fails to compile unless
  `T: Clone`. "Signals from Rust" in `docs/07-script.md` shows
  `rux::Signal<Frame>` with no derive and does not mention the bound. The error
  points at the `#[rux::export]` attribute and names `__read`, a generated,
  private method: ``the method `__read` exists for struct `rux_native::Signal<Tick>`,
  but its trait bounds were not satisfied``. rustc's own hint (add
  `#[derive(Clone)]`) is right, but it arrives through macro internals.
- **Reproduction:** `#[rux::export] pub struct Tick { pub seq: u32 }` and
  `#[rux::export] pub static TICK: rux::Signal<Tick> = rux::Signal::new();`,
  then `cargo check`.
- **Workaround:** not needed: `#[derive(Clone)]` is the requirement, not a
  workaround.
- **Suggested fix:** have the macro emit a `const _: fn() = || { fn assert_clone<T: Clone>() {} assert_clone::<Tick>(); };`
  with a span on the type, or a `compile_error!` that says "a value a Rux
  signal holds is copied to Rux on every read, so `Tick` needs
  `#[derive(Clone)]`". Add the derive to the doc's example.
- **Status:** fixed in Rux d11f3bf (local): the error is now `` `Tick` is held by an exported Rux signal, so it needs `#[derive(Clone)]` `` at the static; the doc example derives it.

## AW-002 One reserved word in a component cascades into errors about names that exist

- **Found:** 2026-10-29, Phase 0 probe 5 (child process).
- **Severity:** friction.
- **Limit:** a page component with `fn spawn()` got one correct warning
  (``spawn` is a reserved word``) and four wrong ones:
  ``the `@tap` handler calls `check`, which does not exist``, the same for
  `stop`, ``expression `logPath` failed: `logPath` is not defined``, and, in
  `app.rux:19` (the `<router>`), ``a component's script failed to run: `proc`
  is not defined``. `check`, `stop`, `logPath` and `proc` all existed. Renaming
  `spawn` cleared all five. A reader fixes the errors first and the warning
  last, so the real cause is the one most likely to be ignored.
- **Reproduction:** in any routed page, `use native::<module>;`, a top-level
  `let x = <module>.f();`, `fn spawn() {}` and two other functions bound to
  `@tap`s; `rux check`.
- **Workaround:** none needed beyond renaming.
- **Suggested fix:** when a component's script fails to compile, report that
  one failure as an **error** and suppress diagnostics that depend on the
  script's names (a "script did not compile, so names in this file were not
  checked" note instead).
- **Status:** fixed in Rux f173be0 (local): a component's functions that do not parse fail the load at their line and column, and nothing else is reported.

## AW-003 Today's runtime reserves rhai's words, which the planned language does not, and the reference does not list them

- **Found:** 2026-10-29, Phase 0, following AW-002.
- **Severity:** friction.
- **Limit:** `spawn`, `thread`, `sync`, `shared`, `default`, `match`, `case`,
  `package`, `static`, `var` and `new` are all refused as function names
  (checked one by one with `rux check`). `docs/11-next.md` "Keywords" reserves
  neither `spawn`, `thread`, `sync`, `shared`, `default`, `case`, `package` nor
  `static`; they are rhai's. `docs/07-script.md`, the reference for what runs
  today, does not list reserved words at all. For an API tool, `default`
  (the default environment), `match` (a matcher in an assertion) and `case`
  (a test case) are the natural names.
- **Reproduction:** `<script> fn default() {} </script>` with a `@tap` calling
  it; `rux check`.
- **Workaround:** none; Tracer names things otherwise.
- **Suggested fix:** list what is reserved today in `07-script.md`; release
  the rhai-only words ahead of the full move off rhai if the fork allows it.
- **Status:** fixed in Rux df09e45 + 28399c0 (local): `default`, `case`, `static`, `spawn`, `thread`, `sync`, `shared`, `package`, `module`, `with` are names; `07-script.md` "Reserved words" lists what is left (`match` stays, as `11-next.md` decided).

## AW-004 `module.signal?.field` in a template says the module is not defined

- **Found:** 2026-10-29, Phase 0 probe 4 (live chart).
- **Severity:** friction (blocker for clean `rux check` on any page that shows
  a field of a Rust signal before it is first set).
- **Limit:** in a page with `use native::chart;`, `{{ chart.series }}` checks
  clean, but `{{ chart.series?.seq }}` and `{{ chart.series?.seq ?? 0 }}` fail
  with ``expression `chart.series?.seq` failed: `chart` is not defined``. The
  same happens for a second module (`events.tick?.seq`). A Rust signal is
  typed `T?` and holds `none` until first set, so `?.` is exactly what the docs
  lead an author to write (the doc example is `{{ sim.frame?.tick ?? 0 }}`).
- **Reproduction:** `probes/pages/chart-probe.rux` line 17; `rux check`.
- **Workaround:** none.
- **Suggested fix:** resolve the receiver of `?.` through the module scope the
  way plain `.` already is.
- **Status:** fixed in Rux b19ff4e (local). **Check only**: driven 2026-10-29, the page renders and
  `{{ chart.series?.seq ?? 0 }}` shows `0`, then the live count.

## AW-005 `rux check` reports different errors for a page checked alone and through the app that routes to it

- **Found:** 2026-10-29, Phase 0 probe 4.
- **Severity:** friction.
- **Limit:** `rux check pages/chart-probe.rux` (which checks it "through
  app.rux, where the app's signals and functions are in scope") reports one
  error. The same page under another name, not routed and so checked alone,
  reports that error and a type error at line 57 (AW-006). The routed check
  hides a real type error in the page.
- **Reproduction:** copy `probes/pages/chart-probe.rux` to
  `pages/zz.rux` and `rux check pages/zz.rux`; compare with
  `rux check pages/chart-probe.rux`.
- **Workaround:** none.
- **Suggested fix:** run the same type pass over a routed page as over a
  page alone; the app's scope should add names, not remove checks.
- **Status:** fixed in Rux f173be0 (local): every component and page script gets the type pass through the app, in its own file. It also caught real errors in Rux's own scaffold and examples.

## AW-006 `int | any` is not narrowed to `any`, so arithmetic on it is refused

- **Found:** 2026-10-29, Phase 0 probe 4.
- **Severity:** papercut.
- **Limit:** with `p` of type `any` (from an untyped parameter),
  `240 - p` is accepted but `240 - (if p > 240 { 240 } else { p })` is
  refused: ``-` cannot be applied to `int` and `int | any```. A union with
  `any` in it is `any`.
- **Reproduction:** `fn f(s) { for p in s.points { let y = 240 - (if p > 240 { 240 } else { p }); } }`; `rux check` the file alone.
- **Workaround:** none needed: typing the parameter is better code anyway.
- **Suggested fix:** absorb into `any` when a union is built.
- **Status:** fixed in Rux 1733b84 (local).

## AW-007 The checker asks for an event type that is never named

- **Found:** 2026-10-29, Phase 0 (every `@key` and `@drag` handler).
- **Severity:** papercut.
- **Limit:** `fn onKey(e) { … }` called as `@key="onKey(event)"` warns
  ``e` has no type … Annotate it: `fn onKey(e: T)``. Neither
  `07-script.md` ("What an event hands you"), `10-types.md` nor `11-next.md`
  names the type of `event` for `@key`, `@drag` or `@tap`, so there is
  nothing to write for `T`.
- **Reproduction:** `probes/app.rux:97`.
- **Workaround:** none; the warning stays.
- **Suggested fix:** name the event types (`KeyEvent`, `DragEvent`, …) in the
  prelude and the docs, and put the right one in the warning.
- **Status:** fixed in Rux f173be0 + 28399c0 (local): `TapEvent`, `KeyEvent`, `DragEvent`, `SwipeEvent` need no `use`, and the warning names the one to write (`fn onKey(e: KeyEvent)`).

## AW-008 `setInterval` is documented as working and as absent, on the same page

- **Found:** 2026-10-29, reading `docs/07-script.md`.
- **Severity:** papercut (docs).
- **Limit:** "Intervals" documents `setInterval(ms) { … }` and
  `clearInterval`. "Deliberately absent" lists `setInterval` as "deferred
  rather than declined, and it will not be spelled this way".
- **Workaround:** n/a.
- **Suggested fix:** drop or rewrite the "Deliberately absent" bullet.
- **Status:** fixed in Rux 28399c0 (local).

## AW-009 Script has no clock

- **Found:** 2026-10-29, Phase 0 probe 2 (large response body).
- **Severity:** friction.
- **Limit:** a page cannot time anything: there is no `now()` or monotonic
  clock in script. Tracer shows elapsed time on a running request and a
  benchmark's wall clock.
- **Workaround:** the documented path: `probes/native/src/clock.rs` exports
  `nowMs()` from Rust. Logged because every app that shows a duration will
  write the same five lines.
- **Suggested fix:** a built-in monotonic `now()` (ms, float).
- **Status:** fixed in Rux 5fc614c (local): `now()` gives milliseconds on a monotonic clock, in script, on desktop, Android and the web.

## AW-010 `rux run --route` is ignored for an app with a `native/` crate

- **Found:** 2026-10-29, Phase 0, first run of the probes.
- **Severity:** friction.
- **Limit:** `rux run --route /chart` prints ``rux: `--route` and `--preview`
  are not passed to an app with a native/ crate yet`` and opens on `/`. The
  window still opens, so a script or a person expecting the deep link lands on
  the wrong page. Every Tracer probe and every Tracer page needs Rust, so
  `--route` (and `--preview`) are unavailable for the whole project.
- **Reproduction:** `cd probes && rux run --route /chart`.
- **Workaround:** none; navigate by hand.
- **Suggested fix:** pass both through to the built executable as arguments
  the generated `main.rs` reads.
- **Status:** fixed in Rux b29c7b4 (local): driven, `rux run --route /chart` opens on the Live chart page.

## AW-011 Values Rust sets reach the page coalesced: 999 of 1,000 events are lost

- **Found:** 2026-10-29, Phase 0 probe 1, driven in the window.
- **Severity:** **blocker** for Tracer's live sync, WebSocket/SSE stream and
  benchmark samples.
- **Limit:** `rux::Signal` is latest-value-wins (as MS-003 says). Measured with
  an `effect` counting a numbered tick Rust sets from another thread:

  | Burst | Reached the page |
  |---|---|
  | 1,000, no gap | **1** of 1,000 |
  | 1,000, ~1 ms apart | **89** of 1,000 (about one per painted frame) |
  | 200, 20 ms apart (50/s, under the frame rate) | **199** of 200 |

  Even a stream slower than the frame rate loses events. Tracer's window has
  to replay every agent action, every WebSocket message and every bench sample
  in order; with latest-value-wins it cannot. Polling a Rust queue from
  `setInterval` is the workaround Mava rejected (MS-003) and Tracer will not
  use either.
- **Reproduction:** `probes/` probe 1 (`pages/events-probe.rux`,
  `native/src/events.rs`).
- **Workaround:** none.
- **Suggested fix:** the ordered queue MS-003 deferred: an exported
  `rux::Channel<T>` (or `Signal::push`) whose values are delivered to a handler
  or `effect` one by one, in order, on the UI thread, batched per frame so a
  burst costs one rebuild.
- **Status:** fixed in Rux ad33851 (local): `#[rux::export] pub static EVENTS: rux::Channel<T>`, `EVENTS.push(v)` from any thread. Script reads it as `T[]`, what was pushed since the last frame; an `effect` reading it runs once per non-empty list, so a burst of 1,000 arrives whole and in order as one update. **Verified 2026-10-29** with probe 1 moved to a `Channel`, driven in the window on the installed build:

  | Burst | Reached the page | In lists |
  |---|---|---|
  | 1,000, no gap | **1,000** of 1,000 | 1 |
  | 1,000, ~1 ms apart | **1,000** of 1,000 | 70 |
  | 200, 20 ms apart | **200** of 200 | 179 |

  None skipped, repeated or out of order.

## AW-012 A signal change inside a component rebuilds the whole document, and the cost grows faster than the rows

- **Found:** 2026-10-29, Phase 0 probe 2 (the user's "1k lines freezes it",
  reproduced and narrowed).
- **Severity:** **blocker**. Every Tracer page is a component (a routed page),
  and the response viewer, history and collection tree are lists.
- **Limit:** the same keyed list of plain one-line rows, filled by a tap:

  | Where the list lives | Handled by | Extra memory | CPU |
  |---|---|---|---|
  | 1,000 rows in the `<screen>` | patch, 27 ms | +33 MB | 1 s |
  | 250 rows in a component | rebuild | +117 MB | 1 s |
  | 500 rows in a component | rebuild, **810 ms** in one frame | +414 MB | 3 s |
  | 1,000 rows in a component | rebuild | **+1,565 MB** | 11 s |
  | 1,000 rows in a routed page | rebuild | +1,566 MB | 9 s |

  The probe's own rows (a line number, an indented `<text>` holding two inline
  `<text>`s, `:style`, `:class`, monospace) in its routed page: 300 rows cost
  +300 MB; 1,002 rows took the window to **4.8 GB in 22 s, not responding**,
  in a release build as in a debug one. The person driving it had to end it
  from Task Manager. Markup, CSS, where the data came from (an `async` Rust
  export, a sync one, or script) and debug vs release were each ruled out:
  only "inside a component" matters.
- **Reproduction:** a component holding `r-for="r in rows" r-key="r.no"` over
  `{ no, key, value }` records and a tap that assigns 1,000 of them; place it
  as `<list-page />` in a `<screen>`, then move the same list into the
  `<screen>` itself. `RUX_PROFILE=each` shows `rebuild` against `patch`.
- **Workaround:** none. (Putting every list in the `<screen>` is not one Tracer
  will take: pages are components by design.)
- **Root cause** (read in the Rux source at `de81f5c`, not changed):
  1. **Component state has no patch path.** `patch()` is chosen by which
     *document* signals moved (`rux-runtime/src/lib.rs:5933`: "instance state
     is not among them"). A component handler that moves its own state sets
     `moved` and calls `self.rebuild()` (`lib.rs:5082`: "the rebuild is the
     only thing that reads instance state again").
  2. **A component's rows each deep-copy the component's whole state.** A
     component is built with `locals = entry.state.clone()`
     (`rux-style/src/lib.rs:9384`), so its `locals` hold the `rows` list
     itself. Every row then starts from `locals.clone()` (`lib.rs:10619`),
     `remember_row` keeps another (`10704`), and every text binding stores one
     more (`TextBinding { locals: locals.clone() }`, `3219`). `Value::List` is
     a plain `Vec<Value>` (`rux-reactive/src/lib.rs:155`), so each clone copies
     every record. N rows × (2 + bindings per row) copies of an N-record list
     is quadratic in time and memory: 1,000 rows of 2 bindings is ~4 million
     records copied. In the document the same list lives in the engine's
     global scope and `locals` hold only the loop variable, which is why the
     `<screen>` version costs 33 MB.
- **Suggested fix:** (1) keep instance state out of `locals`: give each
  instance a scope in the engine, as the document's signals have, so a row's
  locals are its loop variables only; or make `Value::List`/`Map` share their
  contents (`Rc<[Value]>`, copy-on-write). Either removes the quadratic term.
  (2) Give instance state the patch path document state has, keyed
  `instance::name`, so a component's list reconciles instead of rebuilding.
- **Status:** fixed in Rux a49cb20 (local): a list is shared rather than copied into every row, and converted once each way. 1,000 rows in a component: 3.15 s to 22 ms (about 140x faster), linear in rows. Not done: the patch path for component state (root cause 2), so a component's list still rebuilds, now at the screen's cost. **Re-driven by Tracer 2026-10-29** (installed build, debug, `RUX_PROFILE=1`), probe 2's own rows in its routed page: 1,002 lines no longer run away (Phase 0: 4.8 GB, not responding, ended from Task Manager). Loaded through the `await`ed export they draw for about 1.3 s of CPU and +50 MB, and the profile shows `rebuild` at 0 calls a frame. Loaded by the synchronous export inside the tap handler, the window is *not responding* for about 9 s and peaks at 1.15 GB; replacing those 1,002 lines with 100 does the same (about 1 GB, not responding). What remains is mostly layout, AW-018. The appearing-only-on-hover part is AW-017.

## AW-013 An `effect` on a Rust signal that keeps changing locks the window up for good

- **Found:** 2026-10-29, Phase 0 probe 4 (live chart), driven in the window.
- **Severity:** **blocker** for live charts and anything streaming from Rust.
- **Limit:** a routed page binding `<path :d="pathOf(chart.series)">` to a Rust
  signal set 30 times a second draws, but costs a whole core in a debug build.
  `RUX_PROFILE=1`: **rebuild 67 ms** average a frame (worst 821 ms) against
  patch 0.5 ms, stylesheets parsed **18 times a frame** (`css` 37 ms), script
  2%. Adding any `effect` that reads that signal and writes a bound signal
  (`effect { let s = chart.series; if s != none { wakes += 1; } }`) makes it
  never paint again, at 30 Hz as at 500 Hz: one core pinned, no hover, no
  taps, and it does not recover after the Rust side stops. The same effect on
  a signal set once runs once and settles; reading a quiet signal while the
  chart runs is fine. Probably AW-012's rebuild path at frame rate, with the
  effect doubling the work past the frame budget so input and paint never get
  a turn.
- **Reproduction:** `probes/` probe 4 with the `effect` above; press 30 Hz.
- **Workaround:** none.
- **Root cause** (read in the Rux source at `de81f5c`, not changed):
  1. **A bound `:d` always rebuilds the whole document.** Its dependencies go
     into `reg.structural` (`rux-style/src/lib.rs:8498`), which makes
     `patch()` refuse (`rux-runtime/src/lib.rs:5572`). The comment gives two
     reasons: an unsized path takes its geometry's size, and the shape
     animator compares against the previous build. So every sample is a full
     rebuild, of every page's state, at the sample rate.
  2. **A full rebuild re-parses every stylesheet.** `build_styled_tree_stateful`
     parses the document's rules (`lib.rs:2899`) and **every** component's,
     on screen or not (`2938`), on each call. The comment above it says
     "merged here, once per load, rather than per build", but it runs per
     build. The partial-rebuild path already caches parsed rules by print
     (`lib.rs:11169`); the full path does not. The probe app's 7 stylesheets
     × 2.4 rebuilds a frame is the measured 18.3 `css` calls a frame.
  3. **The lock-up is the frame budget.** One full rebuild per sample already
     costs a core; an `effect` that writes a bound signal adds a second
     rebuild per sample (`apply_change_depth` runs effects, then applies their
     writes), and the event loop never reaches input or paint again.
- **Suggested fix:** (1) cache parsed rules across full builds, keyed as the
  partial path keys them, and parse only components that are expanded;
  (2) patch `:d` in place when the path has a CSS size (or relayout just that
  node), and let the animator diff against the patched value; (3) coalesce
  signal-driven rebuilds to at most one per frame and let input and paint run
  between them.
- **Status:** fixed in Rux cce6fd6 (local): what Rust sets is read once a frame rather than once a set, and parsed stylesheets are kept across whole builds. Driven: probe 4 with its `effect` paints at 30 and 500 Hz, and `stop` lands. Not done: `:d` still rebuilds rather than patching (root cause 2), about a core in a debug build at 30 Hz. **Re-driven by Tracer 2026-10-29** (installed build, debug): 30, 60 and 500 Hz all paint, the nav stays responsive, and **stop** lands (CPU goes flat). Each rate costs about one core. Frames are about 38 ms (≈26 fps): `rebuild` runs **2 times a frame at about 22 ms**, against layout's 9 ms and script's 3 ms. At 500 Hz, 3,707 samples woke the page 244 times, which is right for a chart. The two rebuilds a frame are the `:d` patch path still to come.

## AW-014 The running app reports problems `rux check` does not

- **Found:** 2026-10-29, the first run of the release build.
- **Severity:** friction.
- **Limit:** the window's overlay listed ``current` may be `none`, so read it
  as `current?.method`` (three places in `pages/shell-probe.rux`) and
  ``r` may be `none`` in a function, counted among **5 errors**. `rux check`
  on the same files had reported 0 errors. A check that passes and an app that
  opens with five errors trains people to ignore the check.
- **Reproduction:** `pages/shell-probe.rux` before the fix
  (`computed current = requests.find(…)`, then `{{ current.method }}`);
  `rux check`, then `rux build --release` and run.
- **Workaround:** n/a; the code was fixed, as it should have been.
- **Suggested fix:** one checker for both; the runtime's none-safety pass
  should run under `rux check`.
- **Status:** fixed in Rux f173be0 (local), with AW-005: `rux check` now runs the same checks over every page, and reports the `current` may be `none` errors.

## AW-015 The window opens larger than the screen

- **Found:** 2026-10-29, first run.
- **Severity:** papercut.
- **Limit:** `[window] width = 1280, height = 800` on a 1920 × 1080 screen at
  150% scaling opened a 1920 × 1200 window at y = 159, so its bottom 280 px
  were off-screen. Desktop apps clamp their first size to the work area.
- **Reproduction:** `probes/rux.toml` on a 1080p screen at 150%.
- **Workaround:** none.
- **Suggested fix:** clamp the initial size to the monitor's work area and
  centre it.
- **Status:** fixed in Rux 70ef191 (local): a window larger than nine tenths of the monitor opens to fit, centred. Driven: 1280 by 800 at 1.5x on 1080p opens inside the screen.

## AW-016 Each `print` reopens the dev overlay over the app and swallows the next tap

- **Found:** 2026-10-29, driving probe 2 with `print` tracing.
- **Severity:** papercut.
- **Limit:** the overlay that shows `print` output opens over the top of the
  window on every print, and the next tap there dismisses it instead of
  reaching the button under it. Debugging a handler that prints makes the app
  under test hard to drive, by a person or a script.
- **Workaround:** none.
- **Suggested fix:** collect prints in a collapsed corner badge, or let taps
  outside the panel through.
- **Status:** fixed in Rux 37783ba (local): prints alone wait in a one-line badge in the bottom right, which a tap opens; a print never brings back a dismissed panel. Driven: three taps on a printing button all reached it.

## AW-017 An idle window does not draw what a key or an `await` changed, and keys typed in the meantime are lost

- **Found:** 2026-10-29, re-running the probes on the build with AW-011 to
  AW-016 fixed. Phase 0, on `de81f5c`, typed and sent with the same driver
  without any of this.
- **Severity:** **blocker**. Every field in Tracer is typing, and every request
  is an `await`.
- **Limit:** with nothing animating, a change made by a key or by an `await`
  coming back stays off screen until the pointer moves over something with a
  `:hover` style, which forces a frame. Typing then keeps only the last key:
  - Probe 3: **Send** stays "Sending…" indefinitely. Moving the pointer over
    empty space doesn't help. Hovering a nav link shows the 200 with all its
    chips at once.
  - Probe 2: the `await`ed 1k load measured itself at 65 ms (`now()` after the
    `await`), yet "building…" stayed on screen until a hover 4 s later.
  - Probe 6: "post" typed into the request filter, 300 ms per key, shows as
    **`t`** after a hover. "send" then "bench" into the palette's field became
    **`dh`**. Three backspaces sent before "bench" were lost too.
  - Taps are fine, and so are `rux::Channel` values (probe 1) and a running
    chart (probe 4): each of those draws without help.
- **Reproduction:** `probes/`, probe 3: press **Send** and leave the pointer
  still. Probe 6: click **Filter requests…** and type, without moving the
  pointer.
- **Workaround:** none.
- **Lead, not a root cause:** cce6fd6 (AW-013) moved `refresh_native_signals`
  from `RuxEvent::Answered` to `render`. `Answered` still calls
  `settle_tasks()` and asks for a redraw, so the `await` continuation runs, but
  what it set isn't drawn. Keys don't go through `Answered`, so the common
  cause may be elsewhere, perhaps in what `render` relies on having been
  applied before it runs. Not traced further.
- **Suggested fix:** anything that changes document state (a key, an edit, a
  settled task) should apply that change and request a frame. And an edit to
  a field should start from the field's current text, not the text last
  drawn, so keys between two frames are never lost.
- **Status:** open.

## AW-018 A long list costs its whole layout every frame, so 1,000 rows scroll at about 2 fps

- **Found:** 2026-10-29, probe 2 re-run after AW-012's fix.
- **Severity:** **blocker** for the response viewer, which has to show large
  bodies. Probably friction once release builds are measured.
- **Limit:** 1,002 rows of probe 2's viewer, in a debug build, with
  `RUX_PROFILE=1`:

  | phase | avg ms/frame | worst ms |
  |---|---|---|
  | layout | **414** | **1,706** |
  | match | 17 (993 calls a frame) | 347 |
  | gpu | 10 | 93 |
  | script | 2.5 | 44 |
  | rebuild | 0.1 (0 calls) | 9 |

  Every row is laid out on every frame, including the ones scrolled out of
  view. Wheel scrolling reaches line 1,002 and the window keeps responding,
  but each wheel notch costs about 0.7 s of CPU (40 notches: 29 s).
- **Reproduction:** probe 2, **1k lines**, then wheel down over the viewer with
  `RUX_PROFILE=1`.
- **Workaround:** none. Tracer won't page or truncate bodies to dodge it.
- **Suggested fix:** keep the layout of rows that didn't change, and lay out
  only what intersects the viewport of a scrolled container (or offer a
  virtualised list). Selector matching has the same per-row shape.
- **Status:** open. Not yet measured in a release build.

## Phase 0 results

Driven in the window on 2026-10-29 with `tools/drive.ps1` (screenshot, click,
drag, keys), dev and release builds:

| Probe | Result |
|---|---|
| 1 Ordered events | **Fails** on `Signal` (AW-011, blocker); **works** on `rux::Channel` since Rux ad33851: every event, in order |
| 2 Large body | **Fails**: 100 and 300 coloured lines render well; 1,000 freezes the window (AW-012, blocker) |
| 3 HTTP | **Works**: async Rust request off the UI thread, 200 in 815 ms, chips, headers, body; window stays responsive |
| 4 Live chart | **Works without an effect**, at a whole core's cost; **locks up with one** (AW-013, blocker). AW-004 is check-only |
| 5 Child process | **Works**: `@close` ran on a window close and stopped the child; no orphan |
| 6 Shell | **Works**: both splitters drag and clamp, tabs open, the filter narrows, Ctrl+K from inside a field, `blur()` + `autofocus` hands focus to the palette, Esc and a pick close it |

### Re-run on the fixed build (2026-10-29)

The installed `rux` 0.9.0-dev at d7d92f9, with AW-001 to AW-016 fixed
locally, in a debug build:

| Probe | Result |
|---|---|
| 1 Ordered events | **Works** (`Channel`): 1000/1000, 1000/1000, 200/200, in order |
| 2 Large body | **No longer runs away**; 1k awaited draws in ~1.3 s CPU, 1k sync is ~9 s not responding; layout 414 ms/frame (AW-018); result shows only after a hover (AW-017) |
| 3 HTTP | **Works once a frame is forced**: 200 in 1,847 ms with chips, headers, body; until a hover it stays "Sending…" (AW-017) |
| 4 Live chart | **Works with its effect** at 30/60/500 Hz, stop lands; ~1 core, 2 rebuilds a frame |
| 5 Child process | **Works**: Alt+F4 ran `@close`, the child was stopped, no orphan |
| 6 Shell | Splitters, tabs, Ctrl+K, Esc **work**; **typing loses keys** and isn't drawn until a hover (AW-017) |

Ruled out on the way, as Tracer's own mistakes and not Rux's: a flex item
growing to its longest unbreakable line (CSS does the same), an overlay not
centred without `display: flex`, and two "silent" name failures (`load`,
`status`) that were the AW-012 freeze seen from another side.

Not probed yet, needed later: a second window (pop-out response), syntax
highlighting inside an editable field, OS keychain, local server sockets in
the core (Rust, so not a Rux question).
