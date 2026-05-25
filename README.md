![Extra Eyes watching a coding agent](assets/extra-eyes-hero.png)

# Extra Eyes

Eyes spins up N pair-programmer AIs with custom profiles to give instant feedback to one context window.

The driver's seat is an instance of Codex, Claude, or pi, and can use any model. Watchers are the pair programmers: they can also use any model through those harnesses, each with its own prompting profile.

- Feedback is sent through the fastest next available hook.
- It is non-blocking and written in Rust for speed.
- It requires zero extra tool calls from the driver's seat.
- Messages are small, token-efficient, and sent only when needed by default.

## Install / Update

Run this from any Unix shell:

```sh
cargo install --git https://github.com/ratacat/extra-eyes.git --locked --force
```

Requires Rust and Cargo on macOS or Linux. Run the same command again to update.

Uninstall:

```sh
cargo uninstall extra-eyes
```

## Usage

Run `eyes` from the repo you want it to watch:

```sh
cd /path/to/repo
eyes
```

That is the normal workflow. `eyes` starts the project daemon if needed, watches file and conversation changes, runs the default watcher, prints activity in the terminal, and mirrors notes to `.eyes/inbox.md`.

`eyes` is shorthand for `eyes watch`.

Run it in a separate tmux pane or terminal if you want to keep the watcher output visible while your coding agent works.

When a watcher profile starts, Extra Eyes sends one check-in message asking the agent to confirm that the watcher is connected. After that, the agent should mention watcher input only when it changes what the agent does.

Install a harness hook to inject notes into the agent's next turn:

```sh
eyes install codex
eyes install claude-code
eyes install pi
```

Useful commands:

```sh
eyes status   # show running project daemons
eyes restart  # restart the current project's daemon
eyes stop     # stop the current project's daemon
eyes tick     # run one watcher tick now
```

## Custom Watcher Profiles

Watcher profiles live in `.eyes/watchers/<name>.toml` for a repo or `~/.eyes/watchers/<name>.toml` for personal defaults. Repo profiles win over personal profiles with the same name.

In v1, custom watcher profiles use `harness = "raw"`. A raw watcher receives JSON on stdin and writes JSON lines on stdout.

Example:

```toml
name = "security"
default = true
prompt = "Watch for security regressions. Report only concrete risks."
harness = "raw"
model = "local-shell"

[settings]
command = ["sh", "-c", "cat >/dev/null; printf '%s\n' '{\"v\":1,\"type\":\"message\",\"severity\":\"info\",\"text\":\"security watcher ran\"}'"]
timeout_ms = 5000
```

Run named profiles:

```sh
eyes watch security
eyes watch security design
```

Minimal watcher output:

```json
{"v":1,"type":"message","severity":"warning","text":"check the auth path before committing"}
```

Optional fields:

- `refs`: array of `{ "path": "...", "line": 12 }`
- `usage`: `{ "units": 3 }`
- `severity`: `info`, `warning`, or `error`

Extra Eyes isolates watcher crashes, malformed output, nonzero exits, and timeouts. A failed watcher does not stop the daemon or sibling watchers.

## Troubleshooting

```sh
eyes status
cat .eyes/inbox.md
tail -n 80 .eyes/state/daemon.log
```

Hooks fail silent when the daemon is down, so the working harness keeps moving. Run `eyes` or `eyes restart`, then trigger another prompt or file change.

## Development

```sh
cargo fmt --check
cargo test
cargo install --path . --locked --root /tmp/extra-eyes-install-check
```
