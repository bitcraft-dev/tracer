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
tracer send GET https://… -w <dir>   # send a request, print its answer and
                                     # where its time went (DNS, connect, TLS, wait)
tracer status -w <dir>               # who is connected
```

- `crates/protocol`: commands, replies, events, and a blocking client
- `crates/core`: the core (`cargo test -p tracer-core`)
- `crates/cli`: the `tracer` command

## The local API

Anything else (a script, a web page on this machine, another tool) talks
to the core on its own port, which `tracer status` prints, with the token
from `.tracer/core.json`:

```bash
curl -X POST http://127.0.0.1:<port>/v1/command   -H "Authorization: Bearer <token>" -H "X-Tracer-Name: my-script"   -d '{"cmd":"send","request":{"method":"GET","url":"http://localhost:8080/health"}}'
# {"reply":"accepted","request_id":12}
```

- `POST /v1/command` runs one command of the core's protocol
  (`crates/protocol`): the reply's body comes back, or `{"error": …}` with
  a 4xx. A caller is an agent, held to the guardrails and the policy,
  unless `X-Tracer-Kind: cli` says it is a person's script.
- `ws://127.0.0.1:<port>/v1/ws` is a WebSocket carrying the protocol as it
  is, one JSON message per frame: `hello` with the token first, then
  `subscribe` for the live event stream and any other command.
- Only requests addressed to this machine, and pages served from it, are
  answered.

## Letting an AI agent drive it

`tracer mcp` is an MCP server on stdio. Point any MCP client at it with the
workspace; it starts the workspace's core if none is running. In Claude
Code:

```bash
claude mcp add tracer -- "<repo>/target/debug/tracer.exe" mcp -w <dir>
```

The agent gets `list_requests`, `list_environments`, `send_request`,
`run_saved_request`, `rerun_request`, `read_history`, `get_response`,
`save_request`, `read_policy` and `cancel_request`. What it does shows in the window as
its own, live. The guardrails, held by the core:

- By default, an agent's request other than `GET`, `HEAD` or `OPTIONS`, or
  one using an environment named `prod…`, waits for a person: **Allow** /
  **Refuse** in the window, or `tracer approve <id>` / `tracer deny <id>`.
  Unanswered, it fails after 5 minutes.
- `agents.policy` in the workspace, if there is one, sets that per agent,
  with where its requests may go, how many and how big (below).
- Agents see secret names, never values; a secret's value that comes back
  in a response reads `{{name}}`.
- Setting secrets, deleting environments, approving and stopping the core
  are for a person only.
- The kill switch, **agents on/off** in the status bar or `tracer agents
  off`, stops agents (reading aside) and cancels their running requests. It
  stays off across restarts until switched back on.
- Every agent action is in the journal under its name.

### `agents.policy`

A plain file at the workspace's root, meant to be committed. `[*]` is every
agent; a section named after an agent (the name it connects as, such as
`claude-code`) sets only what it names and takes the rest from `[*]`.

```ini
[*]
access = read-only       # read-only | local | anywhere (the default)
approve = writes, prod   # writes, prod, all, or none (default: writes, prod)
rate = 60/min            # requests it may start, per s, min or h (default: none)
max_body = 1 MiB         # largest request body, variables filled in (default: none)

[claude-code]
access = local
```

- `local` is this machine only (`localhost`, `127.x.x.x`, `::1`), checked
  once `{{variables}}` are filled in, and its redirects aren't followed.
- A request outside the rules fails in history, before anyone is asked to
  approve it: `refused by the workspace's policy: …`. One over the rate is
  refused without a number.
- The core reads the file again within a second of a change, and the
  change is in the journal (`tracer watch`). A file with a mistake isn't
  taken: the rules there were stand, the window's status bar says what's
  wrong, and a core that starts on it lets agents only read.
- `tracer policy` prints the rules; an agent's `read_policy` gives its own.
- The name is the one an agent gives, and any program that can read the
  workspace can read the core's token. The policy keeps a well-meaning
  agent within bounds; it is no wall against one that sets out to get round
  it.

## Running the probes

```bash
cd probes
rux run
```

Needs `rux` 0.9.0-dev built from the Rux checkout that the workspace
`Cargo.toml` patches `rux-native` to.
