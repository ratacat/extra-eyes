## Project
**Second Seat** — a harness-agnostic CLI that runs a "pair" AI (the *spotter*) to silently observe another AI's coding work (the *driver*) and feed back critiques. The spotter never edits code. It only writes feedback the driver picks up.

## Roles
- **Driver** — the primary AI working inside a coding harness (Claude Code, Cursor, Codex CLI, Aider, Cline, Continue, Goose, …) with a human in the loop.
- **Spotter** — a secondary AI invoked headlessly by `seat`, watching the driver's filesystem activity and posting short, targeted feedback. Never edits code.

## Mechanism (working assumption)
- **Driver → Spotter:** filesystem watcher on the working tree + git diff snapshots. Universal across harnesses.
- **Spotter → Driver:** a single feedback file (`.seat/feedback.md`) the driver is instructed to read each turn, plus an MCP server adapter (`seat.check()`, `seat.tick()`) for harnesses that support MCP, so feedback can arrive inline as a tool result.
- **Optional accelerants:** harness-native hooks (Claude Code PostToolUse / Stop, etc.) under `adapters/` — never required for core operation.

Design rule: the core must work in *any* harness with just a shell. Adapters only sharpen the loop.

## Philosophy
- **Harness-agnostic at the core.** No feature may require a specific harness.
- **Filesystem is the bus.** One known location per direction. No ad-hoc channels.
- **Cheap, frequent loops.** Small models, short prompts, narrow briefs.
- **Replace, don't deprecate.** Greenfield. No legacy shims, dual configs, or migration paths.
- **Clarity over cleverness.** Explicit, readable code over dense one-liners.
- **Justify new dependencies.** Each one is attack surface and maintenance burden.

## Status
Pre-implementation. Architecture and CLI surface still being shaped.
