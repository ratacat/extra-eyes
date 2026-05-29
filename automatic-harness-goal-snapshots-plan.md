# Automatic Harness Goal Snapshots

## Goal

When `eyes watch` starts after a working-agent goal already exists, watcher agents should still receive the active task objective for the relevant harness session. This prevents false plan-drift warnings caused by watchers seeing later edits or messages without the goal that authorized them.

## Problem

Extra Eyes currently observes project activity through daemon state, hook/feed events, file changes, diffs, and queued watcher messages. If a watcher starts midstream, it may miss the original user objective or active goal state. In a shared repo with multiple sessions, that can make watcher feedback misleading:

- A watcher may see implementation work after a recent read-only prompt and think the agent violated instructions.
- A watcher may answer or reason from stale `@eyes` mentions without knowing the current goal.
- A watcher may blend context from multiple sessions because the repo is project-scoped.

The fix should make the active objective a first-class session-scoped context item captured from the harness automatically.

## Core Model

Add an objective snapshot record:

```rust
ObjectiveSnapshot {
    harness: String,
    session_id: String,
    source: String,
    text: String,
    captured_at_ms: u64,
}
```

This is not a normal user prompt. It is durable context describing what the harness session is trying to accomplish.

## Phase 1: State And Context

1. Add state storage:
   - `.eyes/state/objectives.jsonl`
   - keyed by `harness + session_id`
   - latest snapshot wins for that session

2. Extend watcher context:
   - include the active objective snapshot for the tick's target session
   - keep objective snapshots separate from conversation events
   - make file-only/project ticks omit session objectives unless explicitly broadcast

3. Render the objective in watcher input as:
   - active objective for this harness session
   - advisory context, not a new user request

## Phase 2: IPC And CLI Surface

Add IPC:

```rust
RecordObjective {
    protocol: u32,
    harness: String,
    session_id: String,
    source: String,
    text: String,
}
```

Add CLI:

```sh
eyes feed objective \
  --harness codex \
  --session-id <id> \
  --source goal \
  --text <objective>
```

The CLI exists for tests, fallback harnesses, and debugging. Real capture should come from hook adapters automatically.

## Phase 3: Harness Capture

### Codex

Add a Codex adapter seam:

```rust
codex_objective_snapshot(payload, project_root) -> Option<ObjectiveSnapshot>
```

Preferred sources, in order:

1. Structured hook payload fields if Codex exposes native goal metadata.
2. Stable local session metadata if Codex writes objective state to disk.
3. Transcript parsing only if the transcript path is provided by the hook payload and the parser can avoid treating arbitrary historical text as the current goal.

### Claude Code

Add a Claude adapter seam:

```rust
claude_objective_snapshot(payload, project_root) -> Option<ObjectiveSnapshot>
```

Preferred sources:

1. Structured hook/session fields.
2. Transcript path from hook payload, if present and safe to read.
3. No snapshot if no reliable source exists.

### pi

The pi TypeScript extension should query session/task state if available and submit it through the new objective feed path.

## Phase 4: Watch Startup Backfill

When `eyes watch` starts:

1. Ensure the daemon is running.
2. Ask available harness adapters for active objective snapshots.
3. Record snapshots before watcher check-in and warm-up.
4. Print a quiet status line when a snapshot is captured:

```text
eyes objective captured harness=codex session=<id>
```

If a harness cannot provide an objective, degrade silently.

## Phase 5: Routing Rules

Session-scoped watcher tick receives:

- same-session objective snapshot
- same-session conversation events
- repo diff/file context when appropriate

Project/file-only watcher tick receives:

- repo diff/file context
- no arbitrary session objective
- only explicitly project-broadcast objective snapshots, if that concept is added later

Direct `@eyes` tick receives:

- the source user event
- the same-session objective snapshot
- no other sessions' objectives

## Acceptance Tests

1. `eyes feed objective` records a snapshot and persists it across daemon restart.
2. A session-scoped watcher tick receives that session's objective snapshot.
3. Two sessions in one repo with different objectives do not see each other's objectives.
4. A project/file-only tick does not inherit a random session objective.
5. Direct `@eyes` context includes only the caller's session objective.
6. Missing harness objective support does not fail hook delivery or watcher ticks.
7. Watch startup captures a harness objective before warm-up/check-in when the adapter provides one.

## Risks And Tradeoffs

- Harnesses may not expose native goal state in hook payloads.
- Transcript parsing can create false positives and privacy concerns.
- Objective snapshots can become stale if the harness goal changes and no new snapshot is recorded.
- Without per-session routing, objective capture can make crosstalk worse by adding more session-specific context to a shared stream.

## Recommended First Slice

Implement the storage, IPC, CLI, and watcher-context path first:

1. Add `ObjectiveSnapshot` state and replay support.
2. Add `RecordObjective`.
3. Add `eyes feed objective`.
4. Add session-scoped context filtering.
5. Add integration tests proving isolation and daemon-restart persistence.

After that, wire one real harness adapter once the reliable Codex or Claude source of active goal metadata is confirmed.
