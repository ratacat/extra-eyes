![Extra Eyes watching a coding agent](assets/extra-eyes-hero.png)

# Extra Eyes

Eyes spins up N pair-programmer AIs with custom profiles to give instant feedback to one context window.

The driver's seat is an instance of Codex, Claude, or pi, and can use any model. Watchers are the pair programmers: they can also use any model through those harnesses, each with its own prompting profile.

- Feedback is sent through the fastest next available hook.
- It is non-blocking and written in Rust for speed.
- It requires zero extra tool calls from the driver's seat.
- Messages are small, token-efficient, and sent only when needed by default.

## Harness interaction surface

Extra Eyes touches a harness in exactly two directions, and never lets the watcher agent talk to the harness directly. The watcher is spawned headless by the daemon, reads its context on stdin, and writes JSON Lines back — it issues no harness tool calls of its own. All harness contact happens through the `eyes` hook helper and the daemon bus.

**1. Working agent → Extra Eyes (capture).** A harness hook runs the `eyes` binary on turn boundaries. `eyes` forwards the payload to the daemon, which records it as a conversation event (and, on delivery-eligible events, also fetches pending notes — see direction 2). File changes are captured separately by the daemon's own filesystem watch + git diff, with no harness involvement.

| Harness | Hook / mechanism | Events captured | Code |
| --- | --- | --- | --- |
| Claude Code | shell-command hook (`UserPromptSubmit`, `PreToolUse`, `Stop`) | `UserPromptSubmit` + `Stop` carry conversation | `src/claude_install.rs:9`, handler `src/bin/eyes.rs:1826`, `extract_hook_session_id` `src/bin/eyes.rs:2040` |
| Codex CLI | TOML-configured hook (`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `Stop`) | `UserPromptSubmit` + `Stop` carry conversation | `src/codex_install.rs:11`, handler `src/bin/eyes.rs:1916` |
| pi | TypeScript extension | `input` / `before_*` lifecycle, tagged with `sessionManager.getSessionId()` | `src/pi_install.rs:59` |
| Any other | `eyes feed` call | whatever the working agent forwards | universal fallback |

Capture lands in the daemon via the `RecordConversation` request (`src/daemon.rs:599`), normalized by `normalize_hook_payload` (`src/conversation.rs:30`).

**2. Extra Eyes → working agent (delivery / injection).** On delivery-eligible hook events the same `eyes` helper asks the daemon for pending messages and prints them as `additionalContext`, so they are injected into the agent's next turn with zero tool-call cost. Each receiving session has its own delivery cursor so a note is injected once.

| Harness | Injection point | Delivery cursor key | Code |
| --- | --- | --- | --- |
| Claude Code | `additionalContext` on `UserPromptSubmit` and `PreToolUse` (`is_delivery_hook_event`, `src/bin/eyes.rs:1991`) | `claude-code:{session_id}:hook` | `src/bin/eyes.rs:1826`, `:1867` |
| Codex CLI | `additionalContext` on `PreToolUse` (plus an `@eyes` fast-path on `UserPromptSubmit`) | `codex:{session_id}:hook` | `src/bin/eyes.rs:1954`, `:1957` |
| pi | context injected into the next turn via the extension | per-session | `src/pi_install.rs` |
| Any other | `.eyes/inbox.md` mirror, read each turn | n/a | `render_inbox_mirror` `src/daemon.rs:1562` |

Delivery is served by the daemon's `FetchMessages` request, with the cursor advanced through `CommitCursorIfCurrent` (`src/daemon.rs:504`, `:536`). The `<extra-eyes-messages>` block you see prepended to a turn is the rendered output of this path.

**Where the watcher agent itself runs.** The daemon's `RunWatcher` handler (`src/daemon.rs:613`) calls `watcher::run_profile`, which spawns the watcher process: `codex exec` for `harness = "codex"` (`src/watcher.rs:407`) or the configured `settings.command` for `harness = "raw"` (`src/watcher.rs:587`). The watcher brief plus the context envelope go in on stdin; the model's reply comes back as JSON Lines. This process is invisible to the working agent's harness.

### Per-tick execution model (no pre-warming)

Each tick is a cold, one-shot process. Extra Eyes does **not** keep a warm watcher session between ticks: it passes no `--resume`, `--continue`, or session id, and the Codex path runs `codex exec … --ephemeral` (`src/watcher.rs:407`), which explicitly persists no session. Every tick re-sends the full context envelope on stdin, and the harness re-reads `AGENTS.md` / the repo from scratch. Nothing the watcher "learned" on tick N is carried into tick N+1 inside Extra Eyes — any continuity is only whatever provider-side prompt cache the model backend happens to apply to a repeated prefix.

By default that envelope is the working set (changed files, git diff, recent conversation), not the whole tree. A profile can opt into attaching a whole-repo structural map per tick — see [Repo context](#repo-context-repo_context) under Custom Watcher Profiles.

The optional `warm_start` profile setting (off by default; the bundled default does not set it — `src/bin/eyes.rs:2802`, `src/profiles.rs:445`) does not change this. When enabled in long-running watch mode, it just fires one extra startup tick in a background thread (`start_warm_context_thread`, `src/bin/eyes.rs:2806`) so the watcher produces a baseline pass immediately instead of waiting for the first file or conversation change. It is still a normal, stateless tick — not a reusable warm session.

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
eyes doctor   # check daemon, profile, and harness hook health
eyes restart  # restart the current project's daemon
eyes stop     # stop watching and shut down the daemon
eyes gc       # reap dead or stale daemons
eyes tick     # run one watcher tick now
```

## Custom Watcher Profiles

Watcher profiles live in `.eyes/watchers/<name>.toml` for a repo or `~/.eyes/watchers/<name>.toml` for personal defaults. Repo profiles win over personal profiles with the same name.

Profiles use TOML. `harness = "codex"` runs Codex directly from the profile. `harness = "raw"` runs a custom command that receives JSON on stdin and writes JSON lines on stdout.

Example:

```toml
name = "security"
default = true
prompt = "Watch for security regressions. Report only concrete risks."
harness = "codex"
model = "gpt-5.5"
reasoning_effort = "high"

[settings]
timeout_ms = 180000
context_budget_bytes = 65536
```

Raw command example:

```toml
name = "local-security"
default = false
prompt = "Watch for security regressions. Report only concrete risks."
harness = "raw"
model = "local-shell"
reasoning_effort = "low"

[settings]
command = ["sh", "-c", "cat >/dev/null; printf '%s\n' '{\"v\":1,\"type\":\"message\",\"severity\":\"info\",\"text\":\"security watcher ran\"}'"]
timeout_ms = 5000
```

### Repo context (`repo_context`)

By default a watcher sees the working set — changed files, the git diff, and recent conversation — not the whole tree. Set `repo_context = "codedb"` in `[settings]` to also attach a structural map of the repository (file tree with language, line, and symbol counts from [`codedb`](https://github.com/justrach/codedb)) to every tick, giving the watcher orientation of what exists and where:

```toml
[settings]
repo_context = "codedb"
repo_context_timeout_ms = 10000   # optional, default 10000
repo_context_budget_bytes = 32768 # optional, default 32768
```

- Requires the `codedb` binary on `PATH` (or set `CODEDB_BINARY` to its path). Install it from [codedb releases](https://github.com/justrach/codedb/releases); Extra Eyes only shells out to `codedb tree` and does not need codedb's MCP/hook integration.
- Paths Extra Eyes already ignores (`.git`, `.eyes`, `node_modules`, `target`, `dist`, lockfiles) are stripped from the map.
- The map is **orientation only** — a digest, not file contents. A `codex` watcher can read full files itself through its read-only sandbox; the map just tells it where to look.
- Best-effort: if `codedb` is missing, errors, or exceeds the timeout, the tick still runs without the map and a one-time `repo_context_failed` diagnostic is surfaced (then suppressed until recovery).
- It is re-sent each tick and is the **first** thing dropped if the per-tick context budget is exceeded, so the live diff and conversation keep their share.

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
