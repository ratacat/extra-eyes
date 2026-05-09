# eyesd v1 Implementation Spec

Status: draft started from the validation run on 2026-05-09.

## Readiness

We have a complete traceability chain, not complete empirical validation.

- L1 hook adapters are validated enough to build against. Claude Code and pi have empirical evidence. Codex has source-level support but is blocked on the hook trust gate (`xe-1rg`).
- L2-L10 have no empirical evidence because the implementation does not exist yet.
- Every L2-L10 validation item has a `knb` blocked claim and a follow-up bead (`xe-2iy.1` through `xe-2iy.44`).

That is enough to start implementation specs. The spec must preserve the validation chain: every build slice below maps back to one or more validation beads.

## Goals

Build v1 of Extra Eyes:

1. Run one `eyesd` daemon per project identity.
2. Capture filesystem and conversation context.
3. Run watcher agents from profiles.
4. Queue watcher messages durably.
5. Deliver messages to the working agent through native hooks, MCP polling, or `.eyes/inbox.md`.
6. Prove every validation item in `docs/validation.md` as implementation lands.

## Non-goals

- No Windows support in v1.
- No watcher code edits. Watchers only observe and message.
- No legacy config formats or compatibility shims.
- No silent Codex hook install. Codex must pass the trust gate or report a clear blocker.

## System Shape

```
working tree
  |
  | file events + git diff + hook events
  v
+-------+        unix socket        +----------------+
| hooks | <-----------------------> | eyesd daemon   |
+-------+                           +----------------+
                                          |
                                          | per-tick stdin/stdout
                                          v
                                  +----------------+
                                  | watcher process |
                                  +----------------+
                                          |
                                          v
                                  durable message queue
```

`eyesd` owns all fan-out, queueing, cursors, and delivery state. Hooks and watchers talk to `eyesd`; they do not talk to each other.

## Core Decisions

### Project Identity

Use the canonical project root as daemon identity.

- If inside a git worktree, use `git rev-parse --show-toplevel` and canonicalize the path.
- If not inside git but inside a directory tree containing `.eyes/`, use that `.eyes/` ancestor as the project root.
- If not inside git, use the canonical current working directory.
- Hash the canonical root with SHA-256 for identity. Runtime directories use the first 16 hex chars of that hash to stay below Unix socket path length limits; status output still reports the full hash.

This resolves V2.9, V2.13, V10.50, and V10.51 design ambiguity.

### Runtime and State Paths

Use two path classes:

- Runtime base: `EXTRA_EYES_RUNTIME_DIR` if set; else `$XDG_RUNTIME_DIR/extra-eyes` if set; else `/tmp/extra-eyes-$UID`.
- Runtime dir: `<runtime-base>/<project-hash-16>/`
- Project state: `<project-root>/.eyes/state/`

Runtime contains:

- `eyesd.sock`
- `eyesd.pid.json`
- transient lock files

Project state contains:

- `messages.jsonl`
- `cursors.jsonl`
- `sessions.jsonl`
- `watcher-status.jsonl`

`.eyes/state/` must not be included in watcher context. `.eyes/watchers/` remains profile configuration and may be version-controlled.

### Durable State

Use append-only JSONL for v1 state.

Reason:

- No new database dependency.
- Easy to inspect in validation fixtures.
- Crash recovery is simple: replay logs into memory at daemon start.
- It matches the project preference for explicit, readable state.

Compaction can be added after v1 behavior is proven. It must replace old logs atomically, not introduce a second state model.

### IPC Protocol

Use a length-prefixed JSON frame over Unix domain sockets.

Frame:

- 8-byte unsigned big-endian length.
- UTF-8 JSON payload of exactly that length.

Why not newline-delimited JSON:

- Watcher messages may contain arbitrary newlines.
- Length prefixes make large payload boundaries explicit.
- Hook shell scripts do not need to implement the protocol directly.

Hooks call the `eyes hook` CLI helper. The helper talks framed IPC to `eyesd` and prints harness-specific hook output.

### Watcher Process Model

Spawn watchers per tick in v1.

Reason:

- Crash isolation is straightforward.
- Timeouts kill one process tree.
- Cost and output accounting are per tick.
- Validation fixtures can use small echo watchers.

A future long-lived watcher mode must replace this model only if it proves a material latency or cost win.

### Delivery Semantics

Every message gets a daemon-assigned monotonic `message_id`.

Every delivery surface has an independent cursor:

```
channel = hook | mcp | inbox | ...
cursor_key = project_hash + harness + session_id + surface
```

The hook helper uses fetch-then-commit delivery:

1. Fetch messages newer than `(channel, cursor_key)`.
2. Render the harness-specific injection payload.
3. Print the rendered payload and exit zero.
4. Commit the cursor through the last successfully delivered message ID.

This provides at-least-once delivery. A helper crash or hook failure before cursor commit causes retry on the next fetch; a successful commit suppresses replay. Cursors are contiguous, not sparse: `commit_cursor` means all messages for that `(channel, cursor_key)` through the given message ID were successfully processed.

### Message Markers

Claude Code and Codex injected text must use explicit markers:

```xml
<eyes watcher="security" message_id="42">
...
</eyes>
```

pi uses its native `event.source` distinction where available, and still includes the same visible marker in text to keep cross-harness behavior consistent.

### Profile Resolution

Profile locations:

- Project: `<project-root>/.eyes/watchers/<name>.toml`
- User: `~/.eyes/watchers/<name>.toml`
- Bundled: embedded in the binary

Resolution:

1. `eyes watch <name>` loads project, then user, then bundled.
2. Project profiles win name conflicts.
3. `eyes watch` with no name loads the single project default if present.
4. If no project default exists, load the bundled general reviewer.
5. Multiple project defaults are a config error.

Profiles are read on command invocation. The daemon does not need restart to see a newly created profile.

### Hook Adapters

Claude Code:

- Install as a plugin with `hooks/hooks.json`.
- Use `UserPromptSubmit` for user text capture.
- Use `Stop` for assistant output capture.
- Read `last_assistant_message` directly from Stop payload.
- Inject pending messages through the hook helper output.

Codex:

- Use the same hook config shape as Claude Code.
- Installer must handle `trusted_hash` or stop with a clear error.
- Do not ship a Codex adapter that silently fails the trust gate.

pi:

- Ship a TypeScript extension.
- Use `pi.on("input", ...)` for prompt capture and injection.
- Use `pi.on("session_shutdown", ...)` for assistant output capture.
- Preserve `event.source` handling to avoid extension loops.

Universal fallback:

- Mirror pending messages to `<project-root>/.eyes/inbox.md`.
- The daemon owns this file. Hooks and watchers do not edit it directly.

## Data Model

### Message

```json
{
  "message_id": 42,
  "project_hash": "sha256...",
  "session_id": "optional harness session id",
  "watcher": "security",
  "severity": "info|warning|error",
  "refs": ["src/foo.rs:12"],
  "text": "Short watcher message",
  "tick_id": "tick-20260509-000001",
  "created_at": "2026-05-09T08:00:00Z"
}
```

### Cursor

```json
{
  "cursor_key": "project:harness:session:surface",
  "last_delivered_message_id": 42,
  "updated_at": "2026-05-09T08:00:00Z"
}
```

### Watcher Output

For `harness = "raw"`, watcher profiles use `settings.command = ["argv0", "arg1", ...]`.
The daemon spawns argv directly, with no shell, using the project root as cwd.

Watcher stdin is one versioned JSON envelope:

```json
{
  "v": 1,
  "watcher": "security",
  "tick_id": "tick-20260509-000001",
  "prompt": "Watch for auth bugs.",
  "context": {
    "files": ["src/foo.rs"],
    "diff": "diff --git ...",
    "conversation": ["user: ...", "assistant: ..."]
  }
}
```

Watcher stdout is JSONL. Each line is one candidate event. The only supported
event type today is `message`.

```json
{
  "v": 1,
  "type": "message",
  "severity": "warning",
  "refs": [{"path": "src/foo.rs", "line": 12}],
  "text": "This edit drops the retry path.",
  "usage": {"units": 17}
}
```

The daemon stamps queued messages with authoritative watcher and tick metadata.
Invalid watcher output, nonzero exits, timeouts, and cost cap events become
watcher status records. They must not crash `eyesd`.

## Build Slices

### Slice 1: Daemon Spine

Beads:

- `xe-2iy.1` V2.9 start, pidfile, double-start guard
- `xe-2iy.3` V2.11 SIGINT/SIGTERM shutdown
- `xe-2iy.4` V2.12 durable state restore
- `xe-2iy.5` V2.13 stale socket recovery
- `xe-2iy.32` V8.40 profile schema parsing

Acceptance:

- `eyesd start` creates pidfile and socket.
- A second daemon for the same project refuses to start.
- Signal shutdown removes socket and leaves state readable.
- Restart replays state.

### Slice 2: IPC

Beads:

- `xe-2iy.6` V3.14 large writes
- `xe-2iy.7` V3.15 fast hook reads
- `xe-2iy.8` V3.16 frame protocol
- `xe-2iy.9` V3.17 concurrent writers
- `xe-2iy.10` V3.18 permissions
- `xe-2iy.11` V3.19 macOS/Linux

Acceptance:

- Framed socket round-trips small and large messages.
- Concurrent clients do not corrupt frames.
- Hook helper p95 read latency stays inside the hook budget.
- Runtime dir and socket permissions are owner-only.

### Slice 3: Profiles and Watcher Execution

Beads:

- `xe-2iy.12` V4.20 spawn watcher
- `xe-2iy.13` V4.21 prompt and context delivery
- `xe-2iy.14` V4.22 parseable message
- `xe-2iy.15` V4.23 crash isolation
- `xe-2iy.16` V4.24 timeout
- `xe-2iy.17` V4.25 cost cap
- `xe-2iy.33` V8.41 default resolution
- `xe-2iy.34` V8.42 project wins
- `xe-2iy.35` V8.43 bundled defaults
- `xe-2iy.36` V8.44 no restart for new profiles

Acceptance:

- Echo watcher receives prompt and context.
- Malformed watcher output records one status event.
- Crash, timeout, and cost cap do not kill the daemon.
- Profile resolution matches project, user, bundled precedence.

### Slice 4: Context Construction and Ticks

Beads:

- `xe-2iy.18` V5.26 file event detection
- `xe-2iy.19` V5.27 ignore rules
- `xe-2iy.20` V5.28 git diff snapshot
- `xe-2iy.21` V5.29 normalized conversation schema
- `xe-2iy.22` V5.30 budget and chunking
- `xe-2iy.23` V6.31 debounce
- `xe-2iy.24` V6.32 manual tick
- `xe-2iy.25` V6.33 cadence overrides
- `xe-2iy.26` V6.34 ignored-path suppression

Acceptance:

- File writes, creates, deletes, and renames produce normalized events.
- Ignored paths do not trigger ticks.
- Each tick includes a git diff snapshot.
- Oversize context is ranked and truncated with markers.

### Slice 5: Delivery and Hook Adapters

Beads:

- `xe-2iy.27` V7.35 message IDs and cursors
- `xe-2iy.28` V7.36 cursor advance on success
- `xe-2iy.29` V7.37 watcher-origin markers
- `xe-2iy.30` V7.38 watcher labels
- `xe-2iy.31` V7.39 dedup
- `xe-1rg` Codex trust mechanism
- `xe-3hq` Claude Code injected-input capture question

Acceptance:

- Hook delivery returns only messages newer than the channel cursor.
- Injected content carries watcher name and message ID.
- Repeated hook runs in one session do not duplicate messages.
- Codex installer either configures trust or fails loudly.

### Slice 6: End-to-End and Failure Modes

Beads:

- `xe-2iy.37` V9.45 full loop
- `xe-2iy.38` V9.46 multi-watcher fan-out
- `xe-2iy.39` V9.47 latency budget
- `xe-2iy.40` V10.48 daemon-down silent hook failure
- `xe-2iy.41` V10.49 watcher API failure reported once
- `xe-2iy.42` V10.50 no-git file-only context
- `xe-2iy.43` V10.51 multi-session routing
- `xe-2iy.44` V10.52 daemon restart mid-session

Acceptance:

- One edit produces watcher output visible on the next turn.
- Multiple watchers all deliver labeled messages once.
- Typical edit-to-visibility latency is under 15 seconds.
- Daemon-down, watcher API failure, no-git, multi-session, and restart cases match validation fixtures.

## Open Questions Before Coding

1. Codex hook trust UX: how does a user grant trust, and can the installer automate it safely?
2. Hook success boundary: what exact failure window is acceptable between cursor advance and harness receipt?
3. Context ranking: what content survives first when context exceeds model budget?
4. Cost cap scope: per daemon session, per day, or per profile lifetime?
5. Linux validation target: local container, CI runner, or a named remote host?

## First Implementation Move

Start with Slice 1.

Do not build hook adapters first. The daemon spine and state model define the contracts that hooks, watchers, and tests depend on.

The first PR should deliver:

- `eyesd start`
- project identity resolver
- runtime directory creation
- pidfile and socket ownership
- append-only JSONL state replay
- signal shutdown
- unit/integration tests for V2.9, V2.11, V2.12, V2.13

After Slice 1 passes, every later slice can test against a real daemon instead of a stub.
