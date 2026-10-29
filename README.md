# Tracer

An API workbench whose window is written in [Rux](https://ruxlang.dev):
build, send, test and benchmark requests against local and remote APIs, by
hand or through an AI agent you bring, with the window showing everything
live.

Tracer exists to strain Rux. Where Rux is limited, Tracer reports it in
[FINDINGS.md](FINDINGS.md) rather than working around it.

- [docs/PLAN.md](docs/PLAN.md): what Tracer is, its architecture and phases
- [FINDINGS.md](FINDINGS.md): Rux limits found so far
- [probes/](probes/): Phase 0, one page per capability; see
  [probes/PROBES.md](probes/PROBES.md)

## The core and the CLI

The core is a separate process that owns a workspace; the window, the CLI
and AI agents are all clients of it. Everything they cause is one numbered,
journalled stream of events that every client sees in the same order.

```bash
cargo build -p tracer-cli
tracer core -w <dir>                 # run the core for a workspace
tracer watch -w <dir> [--all]        # follow what every client does, live
tracer send GET https://… -w <dir>   # send a request, print its answer
tracer status -w <dir>               # who is connected
```

- `crates/protocol`: commands, replies, events, and a blocking client
- `crates/core`: the core (`cargo test -p tracer-core`)
- `crates/cli`: the `tracer` command

## Running the probes

```bash
cd probes
rux run
```

Needs `rux` 0.9.0-dev built from the Rux checkout that the workspace
`Cargo.toml` patches `rux-native` to.
