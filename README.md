# Tracer

An API workbench whose window is written in [Rux](https://ruxlang.dev):
build, send, test and benchmark requests against local and remote APIs, by
hand or through an AI agent you bring, with the window showing everything
live.

Tracer exists to strain Rux. Where Rux is limited, Tracer reports it in
[FINDINGS.md](FINDINGS.md) rather than working around it.

- [docs/PLAN.md](docs/PLAN.md): what Tracer is, its architecture and phases
- [docs/PROGRESS.md](docs/PROGRESS.md): where the work stands, what's next,
  and gotchas already solved
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

## Letting an AI agent drive it

`tracer mcp` is an MCP server on stdio. Point any MCP client at it with the
workspace; it starts the workspace's core if none is running. In Claude
Code:

```bash
claude mcp add tracer -- "<repo>/target/debug/tracer.exe" mcp -w <dir>
```

The agent gets `list_requests`, `list_environments`, `send_request`,
`run_saved_request`, `rerun_request`, `read_history`, `get_response`,
`save_request` and `cancel_request`. What it does shows in the window as
its own, live. The guardrails, held by the core:

- An agent's request other than `GET`, `HEAD` or `OPTIONS`, or one using an
  environment named `prod…`, waits for a person: **Allow** / **Refuse** in
  the window, or `tracer approve <id>` / `tracer deny <id>`. Unanswered, it
  fails after 5 minutes.
- Agents see secret names, never values; a secret's value that comes back
  in a response reads `{{name}}`.
- Setting secrets, deleting environments, approving and stopping the core
  are for a person only.
- The kill switch, **agents on/off** in the status bar or `tracer agents
  off`, stops agents (reading aside) and cancels their running requests. It
  stays off across restarts until switched back on.
- Every agent action is in the journal under its name.

## Running the probes

```bash
cd probes
rux run
```

Needs `rux` 0.9.0-dev built from the Rux checkout that the workspace
`Cargo.toml` patches `rux-native` to.
