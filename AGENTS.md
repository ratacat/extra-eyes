## Project
**Extra Eyes** — a harness-agnostic CLI that puts an extra pair of eyes on a coding agent. The eyes silently observe the working agent's filesystem activity and write short, targeted notes back. The eyes never edit code.

## Vocabulary
- **Working agent** — the primary AI inside a coding harness (Claude Code, Cursor, Codex CLI, Aider, Cline, Continue, Goose, …) doing the work, with a human in the loop.
- **Working context** — the state of the work: files in the tree, recent diffs, optionally a fed-in conversation excerpt.
- **Eyes** — the secondary AI invoked headlessly by `eyes`, watching the working context and posting notes. Never edits code.
- **Brief** — the standing instructions for the eyes (what to watch for: security, performance, drift from the plan, etc.).
- **Notes** — the feedback the eyes write back to the working agent.
- **Glance** — a single observation pass.

## Mechanism (working assumption)
- **Working agent → Eyes:** filesystem watcher on the working tree + git diff snapshots. Universal across harnesses.
- **Eyes → Working agent:** a single notes file (`.eyes/notes.md`) the working agent is instructed to read each turn, plus an MCP server adapter for harnesses that support MCP, so notes can arrive inline as a tool result.
- **Optional accelerants:** harness-native hooks (Claude Code PostToolUse / Stop, etc.) under `adapters/` — never required for core operation.

Design rule: the core must work in any harness with just a shell. Adapters only sharpen the loop.

## Philosophy
- **Harness-agnostic at the core.** No feature may require a specific harness.
- **Filesystem is the bus.** One known location per direction. No ad-hoc channels.
- **Cheap, frequent glances.** Small models, short prompts, narrow briefs.
- **Replace, don't deprecate.** Greenfield. No legacy shims, dual configs, or migration paths.
- **Clarity over cleverness.** Explicit, readable code over dense one-liners.
- **Justify new dependencies.** Each one is attack surface and maintenance burden.

## Status
Pre-implementation. CLI surface still being shaped.
