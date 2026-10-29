# Phase 0 probes

One page per question Tracer has to answer about Rux before the real app is
built. Each one is small on purpose: when it fails, the page is the
reproduction for a `FINDINGS.md` entry.

```bash
cd probes
rux run                       # opens on probe 1
RUX_PROFILE=1 rux run         # with a frame-cost summary every 60 frames
rux run --route /json         # straight to one probe
```

The first run compiles `native/` (a minute or two); after that `.rux` edits hot
reload.

For each probe, write down what you saw next to the question, then turn
anything wrong into a `FINDINGS.md` entry.

## 1 · Ordered events (`/`)

Press each burst. Rust pushes numbered ticks onto a `rux::Channel`; the card
counts what the page's `effect` received, and in how many lists.

- [x] **1,000 · no gap**: 1000 of 1000 in 1 list
- [x] **1,000 · 1 ms apart**: 1000 of 1000 in 70 lists
- [x] **200 · 20 ms apart**: 200 of 200 in 179 lists

Anything under the total, or any repeat, fails it: Tracer's live sync has to
replay every agent action in order. Until Rux ad33851 this probe used a
`rux::Signal`, which kept only the newest value (1, 89 and 199 arrived:
AW-011).

## 2 · Large response body (`/json`)

Press **100**, **300** and **1k** lines (awaited Rust), then **1k · sync Rust**
and **1k · script**, and scroll each to the bottom with the wheel. Run with
`RUX_PROFILE=1`; the summary lands in `rux run`'s output.

- [ ] Time from press to lines on screen: 100 ___ 300 ___ 1k ___ (after an
      `await`, a hover may be needed first: AW-017)
- [ ] Scrolling smooth? 1k ___ (2026-10-29: about 0.7 s CPU a notch, AW-018)
- [ ] Profile at 1k (layout / match / rebuild): ___
- [ ] Memory in Task Manager at 1k: ___
- [ ] Colours right (keys blue, strings orange, numbers green)?
- [ ] Can you select and copy a line?

## 3 · Real HTTP request (`/http`)

- [ ] **Send** the default URL: status chip, time, size, headers and body appear
- [ ] Window stays responsive while it is in flight (hover the nav)
- [ ] Switch the method dropdown to POST: the body box appears; send to
      `https://httpbin.org/post`
- [ ] A 404 (`https://httpbin.org/status/404`) shows as a red chip, not an error
- [ ] A bad host (`https://nope.invalid`) shows the error line from `catch`
- [ ] **Ctrl+Enter** inside the URL field sends
- [ ] Typing a long URL: does the field scroll horizontally to the caret?

## 4 · Live chart (`/chart`)

- [ ] The page renders at all (AW-004: `chart.series?.seq` in its template)
- [ ] 30 Hz / 60 Hz / 500 Hz: line moves smoothly? ___ / ___ / ___
- [ ] "page woken N times" vs "samples produced" at 500 Hz: ___ / ___
- [ ] Nav hover and clicks still responsive at 500 Hz?
- [ ] Navigate away and back: chart stopped (`unmounted` ran)?

## 5 · Child process (`/proc`)

1. **start child**, note the pid, **is it alive?** should say true.
2. Close the window with its **X**.
3. In a terminal: `tasklist /FI "PID eq <pid>"` and read the log at the path
   shown on the page.

- [ ] Log says `stopped … (@close)`: ___
- [ ] Log says `stopped … (unmounted)` (it should not, if `@close` already stopped it, but shows the hook ran if `@close` did not): ___
- [ ] Process gone after close: ___
- [ ] Repeat, closing with **Alt+F4**: ___

## 6 · Workbench shell (`/shell`)

- [ ] Drag the vertical splitter: sidebar resizes, clamped 180–520
- [ ] Drag the horizontal splitter: request pane resizes, clamped 120–520
- [ ] Dragging fast: does the splitter keep up, or lose the pointer?
- [ ] Cursor changes to col-resize / row-resize over the splitters
- [ ] Type in **Filter requests…**: list narrows (try `album`, `post`)
- [ ] Click requests: tabs open, active tab follows
- [ ] **Ctrl+K** with nothing focused opens the palette
- [ ] **Ctrl+K** while the filter field has focus also opens it
- [ ] Palette overlay covers the whole page, dims what is under it
- [ ] Typing filters commands; clicking one closes the palette and shows
      "last command" at the bottom
- [ ] **Esc** closes the palette; clicking the dimmed area closes it; clicking
      inside the palette does not
- [ ] Does the palette's field get focus on open, or do you have to click it?
