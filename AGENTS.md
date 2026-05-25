## Project
**Extra Eyes** — a harness-agnostic CLI that puts extra pairs of eyes on a coding agent. Watcher agents silently observe the working agent's filesystem activity and conversation traffic, then deliver short, targeted messages back. Watchers never edit code.

## Vocabulary
- **Working agent** — the primary AI inside a coding harness, doing the work with a human in the loop. v1 first-party harnesses: **Claude Code**, **Codex CLI**, **pi**. Others (Cursor, Aider, Cline, Continue, Goose, …) supported via the universal file fallback.
- **Working context** — the state of the work fed to watchers: file changes, git diffs, and conversation traffic (user prompts + working-agent outputs).
- **Watcher agent** — a secondary AI invoked headlessly by `eyes`, watching the working context and posting messages. Never edits code. Multiple watchers may run in parallel; each receives the full context fan-out.
- **Prompt** — the watcher's standing instructions (what to watch for: security, performance, drift from the plan, hidden assumptions, etc.).
- **Message** — what the watcher sends back to the working agent.
- **Tick** — one observation pass by a watcher.

## Watchers

A **watcher** is a named profile bundling everything needed to spawn a watcher agent:

```toml
# .eyes/watchers/harold.toml
name      = "harold"
default   = true              # at most one profile per project may set this
prompt    = """
You watch a coding agent's work for security mistakes...
"""
harness   = "claude-code"     # claude-code | codex | pi | (raw model)
model     = "claude-sonnet-4-6"
settings  = { thinking = "low", max_tokens_per_tick = 1500 }
```

- Profiles live under `.eyes/watchers/<name>.toml` (project, version-controlled) or `~/.eyes/watchers/<name>.toml` (user, personal). Project profiles win on name conflict.
- `eyes watch` with no argument starts the project's default watcher.
- `eyes watch <name>` starts the named watcher.
- Typical workflow: run `eyes` from the project folder before or after starting the working agent, then give that agent work in the same folder.
- Multiple watchers may run concurrently. Each receives the full context fan-out; each contributes its own messages, labeled with its name.
- A small set of bundled defaults ships with the binary (general code reviewer, security-leaning, plan-drift, etc.) — installable into a project with `eyes init`.

## Architecture

```
        ┌──────────┐    unix socket    ┌──────────┐
watcher │ watcher1 │ ─────────────────▶│          │
agents  │ watcher2 │ ─────────────────▶│  eyesd   │ ◀─── fswatch + git diff
        │ watcherN │ ─────────────────▶│ (daemon) │ ◀─── conversation feed
        └──────────┘                   └────┬─────┘
                                            │
                                            ├──▶ harness hook   (Claude Code, Codex)
                                            ├──▶ MCP tool       (any MCP-capable harness)
                                            └──▶ .eyes/inbox.md (universal fallback)
```

### Working agent → watcher (context fan-out)
- **File changes:** fswatch on the working tree + git diff snapshots. Universal, automatic.
- **Conversation traffic:** captured via harness hooks where available (Claude Code / Codex `UserPromptSubmit` + `Stop`), or via `eyes feed` calls from the working agent on harnesses without hooks.
- All watchers receive the same context. Fan-out happens inside the daemon.

### Multi-session caveat
Extra Eyes is currently project-scoped, not window-scoped. Multiple harness sessions in one repo may share conversation context and watcher message queues, so direct `@eyes` replies, quoted watcher notes, or assistant summaries can crosstalk or recursively bloat the context. Intended default remains one `eyesd` and one `eyes watch` per project; multiple working-agent sessions need explicit session/event routing before this is considered clean.

### Watcher → daemon
- Unix domain socket. Sub-millisecond delivery, large payloads fine.

### Daemon → working agent (delivery surfaces, layered)
The model running inside a harness is a function invoked turn-by-turn — it cannot subscribe to a socket. New messages reach it either by being **injected** by the harness or by being **polled** via a tool call. Three surfaces, picked per harness:

1. **Native hook adapter** *(default for v1 first-party harnesses)* — pending messages are injected as additional context before the working agent's next turn. Zero tool-call cost on the working agent.
   - **Claude Code:** shell-command hook on `UserPromptSubmit` (and `Stop`), emitting `additionalContext`.
   - **Codex CLI:** TOML-configured hook on `UserPromptSubmit` (and `Stop`), turn-scoped, behind a config flag.
   - **pi:** TypeScript extension subscribing to `input` / `before_*` lifecycle events, injecting context into the next turn.
2. **MCP tool** *(any MCP-capable harness)* — working agent polls `messages.poll(since)` mid-turn. Useful for explicit consultation or for harnesses without hook systems.
3. **`.eyes/inbox.md`** *(universal fallback)* — daemon mirrors messages to a file; the working agent is system-prompted to read it each turn.

### Latency
- Watcher → daemon: ~ms.
- Daemon → working agent: bounded by the next turn (hook injection) or next tool call (MCP poll). Practically a few seconds.
- The watcher's own model inference is the real bottleneck, not the channel.

### Optional: synchronous consult
Working agent may call `eyes ask "<question>"` mid-turn to consult a watcher inline. Useful for "second opinion before I commit." This is opt-in; the default mode is silent, async observation.

## Philosophy
- **Harness-agnostic at the core.** No feature may require a specific harness. Hook adapters sharpen the loop where they exist; the file fallback always works.
- **The daemon is the bus.** Watchers and harnesses talk to `eyesd`; never to each other.
- **Cheap, frequent ticks.** Small models, short prompts, narrow watcher briefs.
- **Replace, don't deprecate.** Greenfield. No legacy shims, dual configs, or migration paths.
- **Clarity over cleverness.** Explicit, readable code over dense one-liners.
- **Justify new dependencies.** Each one is attack surface and maintenance burden.

## Status
Pre-implementation. **v1 scope:** `eyesd` daemon + first-party hook adapters for Claude Code, Codex CLI, and pi. CLI surface and watcher prompt/config format still to be designed.
