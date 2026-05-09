# Extra Eyes

Extra Eyes runs watcher agents beside a coding agent. Watchers observe project changes and conversation events, then send short messages back through harness hooks or the `.eyes/inbox.md` fallback.

The project is a Rust CLI with two binaries:

- `eyesd`: project-scoped daemon, Unix socket, state store, message queue.
- `eyes`: profile resolution, watcher ticks, hook helpers, harness installers.

## Install

Prerequisites:

- Rust and Cargo
- macOS or Linux

Install from a checkout:

```sh
cargo install --path . --locked
```

Verify the binaries:

```sh
eyes --help
eyesd --help
```

## Quick Start

Create a raw watcher profile in a project:

```sh
mkdir -p .eyes/watchers
cat > .eyes/watchers/local.toml <<'TOML'
name = "local"
default = true
prompt = "Watch for risky edits. Keep feedback short."
harness = "raw"
model = "local-shell"

[settings]
command = ["sh", "-c", "cat >/dev/null; printf '%s\n' '{\"v\":1,\"type\":\"message\",\"severity\":\"info\",\"text\":\"extra eyes saw this change\"}'"]
timeout_ms = 5000
TOML
```

Start the daemon:

```sh
eyesd start
eyesd status
```

In another terminal, start watching the project:

```sh
eyes watch
```

Edit a file. The watcher runs on the change and writes feedback to two delivery surfaces:

- Native hook delivery, when a harness hook is configured.
- `.eyes/inbox.md`, the universal file fallback.

Stop the daemon:

```sh
eyesd stop
```

## Claude Code

Install Claude Code hooks:

```sh
eyes install claude-code
```

By default this updates `$CLAUDE_CONFIG_DIR/settings.json` or `~/.claude/settings.json`. To target a sandbox settings file:

```sh
eyes install claude-code --settings /path/to/settings.json --eyes-bin "$(command -v eyes)"
```

The installer adds `UserPromptSubmit` and `Stop` hooks. User prompt hooks record conversation context and inject pending watcher messages into the next turn.

## Codex

Install Codex hooks and trust entries:

```sh
eyes install codex
```

By default this updates `$CODEX_HOME/config.toml` or `~/.codex/config.toml`. To target a sandbox config:

```sh
eyes install codex --config /path/to/config.toml --eyes-bin "$(command -v eyes)"
```

The installer adds `SessionStart`, `UserPromptSubmit`, and `Stop` hooks. Hook output is capped before injection so oversized watcher messages are truncated or deferred instead of dropped.

## pi

Install the project-local pi extension:

```sh
eyes install pi
```

By default this writes `.pi/extensions/extra-eyes.ts` in the current project. To target a different project or binary:

```sh
eyes install pi --project /path/to/project --eyes-bin "$(command -v eyes)"
```

The extension records interactive input and session shutdown events, fetches pending watcher messages, and prepends them to the next user input without feeding extension-generated prompts back into Extra Eyes.

## Raw Watcher Protocol

In v1, watcher profiles execute through `harness = "raw"` with an explicit `settings.command`. Claude Code, Codex, and pi are working-agent integrations installed separately with `eyes install <harness>`.

A raw watcher receives one JSON envelope on stdin. It writes JSON lines on stdout.

Minimal message:

```json
{"v":1,"type":"message","severity":"warning","text":"check the auth path before committing"}
```

Optional fields:

- `refs`: array of `{ "path": "...", "line": 12 }`
- `usage`: `{ "units": 3 }`
- `severity`: `info`, `warning`, or `error`

Extra Eyes isolates watcher crashes, malformed output, nonzero exits, timeouts, and repeated API failures. A failed watcher does not stop the daemon or sibling watchers.

## Common Commands

```sh
eyes profile resolve --json
eyes tick --json
eyes message send "manual note" --watcher local --severity info
eyes hook fetch --cursor-key manual-session
eyesd start --foreground
```

Detached daemon logs go to `.eyes/state/eyesd.log`.

## Troubleshooting

Use JSON output when wiring scripts or checking canaries:

```sh
eyesd start --json
eyesd status --json
eyes tick --json
```

If a harness does not show feedback, check the universal fallback first:

```sh
cat .eyes/inbox.md
```

Then check daemon state:

```sh
eyesd status
tail -n 80 .eyes/state/eyesd.log
```

Hooks fail silent by design when `eyesd` is down, so the working harness is not blocked. Restart the daemon and trigger another prompt or file change.

## Harness Status

| Harness | Current user path |
| --- | --- |
| Claude Code | `eyes install claude-code` installs native hooks. |
| Codex | `eyes install codex` installs trusted native hooks. |
| pi | `eyes install pi` installs a project-local extension. |
| Raw | Configure `.eyes/watchers/*.toml` with `harness = "raw"` and `settings.command`. |

## Development

Run the quality gates:

```sh
cargo fmt --check
cargo test
cargo install --path . --locked --root /tmp/extra-eyes-install-check
```
