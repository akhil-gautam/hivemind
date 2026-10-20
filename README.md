# Hivemind

Run CLI agents (Claude Code, OpenCode, Codex, Gemini CLI, ollama, anything with
a command line) on several Macs joined by the home WiFi, and drive them from
one page in a browser.

Each Mac runs a tiny `hivemind` node. Nodes find each other on the LAN, accept
prompts over HTTP, run the local agent as a child process, and stream the
output back. Any device on the WiFi can open the page and use them.

```
  laptop                     studio                      mini
┌──────────────┐           ┌──────────────┐           ┌──────────────┐
│ browser page │──UDP mcast│ hivemind node│           │ hivemind node│
│ "fix tests"  │──HTTP/SSE─▶  claude -p   │           │  ollama run  │
└──────────────┘           └──────────────┘           └──────────────┘
```

## The page (no jargon needed)

Open `http://<mac-ip>:8787/` and you get three things:

1. **Your Macs** — one card per Mac on the WiFi, with a green dot when it is
   reachable. Click the card to say which Mac should do the work.
2. **What should it do?** — a box to type what you want, and a button.
3. **Recent work** — what was asked, on which Mac, and how it ended.

Output appears live while the agent works. If a Mac has no agent set up yet,
the page shows a short setup card that lists the agent programs it found
(Claude Code, OpenCode, …) and saves your choice — no config files, no
terminal.

## Why it is small

| | hivemind |
|---|---|
| idle RSS per node | **1.8 MiB** (2.1 MiB with 10 tasks in flight) |
| binary | 576 KiB, links only `libSystem` |
| threads | 8 per node (accept, 2× beacon, peer-cache, worker, agent I/O) |
| dependencies | `serde_json`, `socket2` — no async runtime, no TLS stack, no DB |

Memory discipline that keeps it there:

* one thread per connection with a 512 KiB stack and a hard cap of 64 live
  connections; no connection pool, no per-connection buffers that outlive a
  request;
* task output is capped (`max_output_bytes`, 64 KiB default) and the full
  result is appended to `tasks.jsonl` instead of being held in RAM;
* the in-memory task history is a bounded ring (`history`, 256 entries);
* the peer table is a handful of small structs;
* the web page is ~20 KB of HTML/CSS/JS embedded in the binary — no assets to
  download, no framework, no build step.

Measured on macOS 26 (arm64), release build:

```
$ ps -o rss= -p $(pgrep -f "hivemind serve")
 1824 KiB     # idle
 2128 KiB     # after 10 tasks
```

## Install

### Homebrew

```sh
brew tap akhil-gautam/hivemind git@github.com:akhil-gautam/homebrew-hivemind.git
brew install akhil-gautam/hivemind/hivemind
```

Things worth knowing:

* **Use the fully qualified name.** `hivemind` is already taken in
  homebrew-core by an unrelated process manager, so plain `brew install
  hivemind` installs that one instead. `brew install akhil-gautam/hivemind/hivemind`
  installs this project.
* **You need a GitHub SSH key.** The tap and this repository are private, so
  Homebrew clones both over SSH — the same key that clones the tap is used
  for the source. Without one, the install stops with *"could not read
  Username for 'https://github.com'"*. If you use a token instead of a key,
  run `gh auth setup-git` once before installing.
* **Rust is required** to build from source: `brew install rust`.

Upgrades:

```sh
brew update
brew upgrade akhil-gautam/hivemind/hivemind
```

### From source

```sh
git clone <this repo> ~/projects/hivemind
cd ~/projects/hivemind
cargo build --release
cp target/release/hivemind ~/.local/bin/hivemind     # ~/.local/bin is already on PATH
```

Rust 1.75+ is the only build requirement.

### Migrating from `hive` (0.1.x)

The project was renamed; everything user-facing moved with it:

| was | is |
|---|---|
| `hive` (binary) | `hivemind` |
| `~/Library/Application Support/hive/` | `~/Library/Application Support/hivemind/` |
| `io.hive.agent` (launchd) | `io.hivemind.agent` |
| `~/Applications/Hive.app` | `~/Applications/Hivemind.app` |
| `HIVE_DIR`, `HIVE_LOG`, `HIVE_NODE_NAME` | `HIVEMIND_DIR`, `HIVEMIND_LOG`, `HIVEMIND_NODE_NAME` |

```sh
hive uninstall                                  # stop the old service
mv ~/Library/Application\ Support/hive \
   ~/Library/Application\ Support/hivemind     # keep your settings
cp target/release/hivemind ~/.local/bin/hivemind
hivemind install
```

## One click

```sh
hivemind open     # start the node if needed, then show the page in your browser
hivemind app      # create a double-clickable Hivemind.app in ~/Applications
hivemind install  # start the node automatically at login (launchd)
hivemind probe    # check which nodes answer on this network
```

`hivemind open` and the app both work whether or not a node is running: they start
one in the background if needed and then open `http://<mac-ip>:8787/`.

## Set up a node (on every Mac that owns an agent)

```sh
hivemind config init                                   # ~/Library/Application Support/hivemind/config.json
hivemind config set node_name studio
hivemind config set agent_cmd "claude -p {prompt}"    # the agent on THIS Mac
hivemind config set work_dir "~/projects/my-app"      # where the agent works by default
hivemind serve
```

`agent_cmd` is a shell command template:

* `{prompt}` is replaced with the prompt, shell-quoted (a prompt can never
  break out of the command);
* without `{prompt}` the prompt is piped to the agent on stdin.

Different Macs can run different agents:

```sh
# studio
hivemind config set agent_cmd "claude -p {prompt}"
# mini
hivemind config set agent_cmd "ollama run llama3"
# laptop
hivemind config set agent_cmd "codex exec {prompt}"
```

The same choices are available in the page's setup card, so you never have to
open a terminal.

## Use it from any Mac

```sh
hivemind ls                                   # what is on the WiFi right now
NAME               ADDRESS                  AGENT        STATE    VERSION
studio             192.168.0.20:8787        claude       online   0.1.0
mini               192.168.0.21:8787        ollama       online   0.1.0

hivemind run "refactor src/main.rs" --node studio
hivemind run "summarise this repo" --node mini --label review --cwd ~/projects/app
hivemind run "ping" --host 192.168.0.21:8787   # skip discovery, talk to one node
hivemind get  t-1712ab-4f2c                   # inspect a task
hivemind cancel t-1712ab-4f2c                # stop a running agent (kills its process group)
```

`hivemind run` streams the agent's stdout live and exits with the agent's status:
`0` when the task finished, `1` when it failed, was cancelled or timed out.

## How the LAN side works

* **Discovery** — every node sends a 120-byte JSON beacon every 2 s to the
  multicast group `239.255.42.99:4789` (TTL 1, link-local) and to the
  interface broadcast address, so it also works on routers that filter
  multicast. Beacons carry the node name, its LAN IP and its HTTP port. A
  node that starts later sends a `hello` and existing nodes answer
  immediately, so joining the mesh takes under a second.
* **Transport** — plain HTTP/1.1 on port 8787. `POST /v1/tasks` starts a task,
  `GET /v1/tasks/{id}/events` streams it as server-sent events, and the result
  is also readable as JSON from `GET /v1/tasks/{id}`. Responses carry
  `Access-Control-Allow-Origin: *` so the page served by one Mac can drive
  agents on another.
* **No cloud, no accounts, no inbound ports** — everything stays on the local
  network and dies with the machine.

If discovery is blocked on your network, skip it: `hivemind run --host IP:PORT`
always works, and `hivemind serve --no-discovery` turns the beacon off.

## HTTP API

| Method | Path | Purpose |
|---|---|---|
| GET | `/` | the web page (plus `/app.css`, `/app.js`) |
| GET | `/healthz` | liveness probe |
| GET | `/v1/info` | node name, IP, agent, version, uptime, load |
| GET | `/v1/mesh` | this Mac + every peer, with live status |
| GET | `/v1/agents` | agent programs found on this Mac, verified against `--help` |
| POST | `/v1/settings` | change this Mac's agent, folder or name |
| GET | `/v1/peers` | nodes this node can see |
| POST | `/v1/tasks` | `{"prompt": "...", "label": "...", "cwd": "..."}` → task record |
| GET | `/v1/tasks` | recent tasks (bounded history) |
| GET | `/v1/tasks/{id}` | one task: state, exit code, output, error, folder |
| GET | `/v1/tasks/{id}/events` | SSE: `output`, `stderr`, `state`, `final` |
| POST | `/v1/tasks/{id}/cancel` | cancel; kills the agent's process group |

Task states: `queued` → `running` → `done` | `failed` | `cancelled`.

## Configuration

`~/Library/Application Support/hivemind/config.json` (override the directory with
`HIVEMIND_DIR`):

| Key | Default | Meaning |
|---|---|---|
| `node_name` | computer name | name shown in the page and in `hivemind ls` |
| `bind` / `port` | `0.0.0.0` / `8787` | HTTP listener |
| `agent_cmd` | – | shell template for the local agent, `{prompt}` optional |
| `work_dir` | home folder | folder agents work in unless a task names another |
| `concurrency` | `1` | tasks run in parallel on this node |
| `task_timeout_s` | `900` | kill the agent after this long |
| `max_output_bytes` | `65536` | output kept per task in RAM and on disk |
| `max_queue` | `64` | queued tasks before the node says "busy" |
| `history` | `256` | tasks kept in memory |
| `discovery_port` | `4789` | beacon port |
| `discovery_group` | `239.255.42.99` | beacon multicast group |
| `discovery_enabled` | `true` | turn the beacon off entirely |
| `advertise_ip` | auto | IP announced to peers |

`hivemind config show` prints the effective configuration, `hivemind config path` the
file location. Changes made in the page are written back to this file.

## Notes for real WiFi networks

* **Firewall**: macOS asks whether to accept incoming connections for a new
  binary. Allow `hivemind` in System Settings → Network → Firewall, otherwise
  peers cannot reach the node (the page still shows the Mac, but tasks fail to
  connect).
* **Both nodes must be on the same subnet** — multicast and broadcast do not
  cross routers. A guest network isolates clients; use the main network or
  `--host` with a routable address.
* **Sleeping Macs**: a sleeping node stops answering. The page marks it
  offline; tasks sent to it fail fast instead of hanging.
* **Nothing shows up?** Run `hivemind probe` on the Mac that cannot see the others.
  It prints the nodes it hears and exits non-zero if it hears none — that
  separates "the network blocks beacons" from "the other node is down".
  `hivemind run --host IP:PORT` works regardless of discovery.

## Development

```sh
cargo test --release      # 6 unit tests + 12 end-to-end tests
cargo build --release
```

The end-to-end tests start real nodes on loopback: discovery, task round trip,
prompt quoting, agent failure, node-side timeout, cancellation, task history
across a restart, the web page and its assets, the mesh view, settings
validation, running in a chosen folder, and CORS headers.
