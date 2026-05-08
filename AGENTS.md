## Project
**Extra Eyes** — a harness-agnostic CLI that puts extra pairs of eyes on a coding agent. Watcher agents silently observe the working agent's filesystem activity and conversation traffic, then deliver short, targeted messages back. Watchers never edit code.

## Vocabulary
- **Working agent** — the primary AI inside a coding harness (Claude Code, Codex CLI, Cursor, Aider, Cline, Continue, Goose, …) doing the work, with a human in the loop.
- **Working context** — the state of the work fed to watchers: file changes, git diffs, and conversation traffic (user prompts + working-agent outputs).
- **Watcher agent** — a secondary AI invoked headlessly by `eyes`, watching the working context and posting messages. Never edits code. Multiple watchers may run in parallel; each receives the full context fan-out.
- **Prompt** — the watcher's standing instructions (what to watch for: security, performance, drift from the plan, hidden assumptions, etc.).
- **Message** — what the watcher sends back to the working agent.
- **Tick** — one observation pass by a watcher.

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

### Watcher → daemon
- Unix domain socket. Sub-millisecond delivery, large payloads fine.

### Daemon → working agent (delivery surfaces, layered)
The model running inside a harness is a function invoked turn-by-turn — it cannot subscribe to a socket. New messages reach it either by being **injected** by the harness or by being **polled** via a tool call. Three surfaces, picked per harness:

1. **Native hook adapter** *(default for Claude Code and Codex)* — `UserPromptSubmit` hook injects pending messages as additional context before the working agent's next turn. Zero tool-call cost on the working agent.
2. **MCP tool** *(any MCP-capable harness)* — working agent polls `messages.poll(since)` mid-turn. Useful for explicit consultation or for harnesses without hooks.
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
Pre-implementation. CLI surface and watcher prompt/config format still to be designed.
