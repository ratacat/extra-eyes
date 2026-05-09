# Validation

Every assumption Extra Eyes makes that needs to be proven before we trust the system end-to-end. Each item is a thing to demonstrate works — not a feature to design.

The running log of what's been proven, how, and what we learned lives in beads (`br`) — see `Tracking` at the bottom.

## Suggested order

1. **Layer 1 first.** If hook injection doesn't work the way we think on any of the three first-party harnesses, the architecture changes. Everything downstream depends on it.
2. **Layers 2–3.** Daemon + socket spine — the bus everything else rides on.
3. **Layers 4–7.** Watcher loop, context construction, tick triggering, message delivery.
4. **Layer 8.** Profile system.
5. **Layers 9–10.** End-to-end + failure modes.

## Layer 1 — Per-harness hook adapter

For each of **Claude Code**, **Codex CLI**, **pi**:

1. Pre-prompt hook fires reliably on every user turn.
2. Hook can emit content that is actually injected into the model's next-turn input (not just printed to terminal).
3. Model demonstrably reads and acts on injected content (write a test prompt that only succeeds if the injection landed).
4. Post-stop hook fires on every assistant turn and can capture the assistant's output text.
5. User-prompt hook can capture the user's submitted text.
6. Injected content tolerates expected payload sizes — find the practical ceiling (1KB normal; test 10KB, 100KB).
7. Hooks work in both interactive and headless (`-p` / `--print`) modes.
8. End-to-end hook latency is sub-second (hook fire → daemon → response → injection delivered).

## Layer 2 — Daemon (`eyesd`)

9. Starts cleanly, writes a pidfile, refuses to double-start in the same project.
10. Survives a watcher crash without dying.
11. Shuts down cleanly on SIGINT/SIGTERM and removes its socket.
12. State (pending message queue, per-channel cursors) persists across daemon restarts.
13. Stale-socket recovery: if the previous daemon died ungracefully, next start cleans up.

## Layer 3 — IPC (Unix domain socket)

14. Watchers can write large messages without blocking the daemon.
15. Hooks can read pending messages and exit fast (no daemon round-trips that exceed the harness's hook timeout — find that timeout for each).
16. Frame protocol handles messages of varying sizes (length-prefixed or newline-delimited JSON, decide and prove).
17. Concurrent writers (multiple watchers + multiple hook reads) don't interleave or corrupt frames.
18. Socket file permissions prevent other users from reading/writing.
19. Works on macOS and Linux. (Windows out of scope for v1.)

## Layer 4 — Watcher process lifecycle

20. Daemon can spawn a watcher subprocess from a profile (correct binary, flags, env).
21. Watcher receives its prompt + the current working context via a defined channel.
22. Watcher returns a message in a parseable format (JSON line on stdout, with severity/refs/text).
23. Crash isolation: a buggy watcher doesn't take down the daemon or sibling watchers.
24. Per-tick timeout enforced (kill the watcher if it takes too long).
25. Configurable cost cap per watcher (token / dollar budget) — prove enforcement.

## Layer 5 — Working context construction

26. fswatch (or equivalent) detects writes, creates, deletes, renames in the working tree.
27. Ignores `.git/`, lockfiles, `node_modules/`, build artifacts, large binaries, and `.eyes/` itself.
28. Snapshots a git diff at tick time and bundles it with the change set.
29. Conversation traffic captured by harness hooks lands in the daemon in a normalized schema (regardless of harness).
30. Context payload to a watcher fits in its model's input budget (chunking / summarization strategy when not).

## Layer 6 — Tick triggering

31. Debounce coalesces a burst of file changes into a single tick.
32. Manual `eyes tick` triggers immediately, bypassing debounce.
33. Per-watcher cadence overrides (some watchers can opt for "every N ticks" instead of every tick).
34. No spurious ticks when only ignored paths change.

## Layer 7 — Message delivery to working agent

35. Daemon assigns monotonic IDs to messages, tracks per-delivery-channel cursors.
36. Hook reads only messages newer than its cursor, advances cursor on success.
37. Injected content clearly marks itself as watcher-originated (so the working agent doesn't confuse it with user input).
38. Multi-watcher messages get labeled with the watcher's name.
39. Dedup: the same message is never injected twice into the same session.

## Layer 8 — Profile system

40. Profile schema (prompt, harness, model, settings) parses and validates.
41. `eyes watch` with no args resolves to the marked-default profile.
42. `eyes watch <name>` resolves correctly across project and user profile dirs (project wins).
43. Bundled defaults (general code reviewer, security-leaning, plan-drift) ship with the binary.
44. New profiles can be authored without restarting the daemon.

## Layer 9 — End-to-end flow

45. Full loop: working agent edits a file → watcher ticks → message returns → working agent sees it on the next turn — provably, with timing measured.
46. Multi-watcher: same edit → all configured watchers tick → all messages visible on next turn.
47. Latency budget: edit-to-visibility under N seconds (set N = 10–15) for a typical change.

## Layer 10 — Failure modes

48. Daemon not running: hook fails fast and silently, doesn't pollute the working agent's input with errors.
49. Watcher API failure (rate limit, missing key): reported once via a status message, not on every tick.
50. No git repo: degrades to file-only context, doesn't error.
51. Multiple harness sessions in the same project: messages routed correctly, no cross-talk or loss.
52. Daemon restart mid-session: queued messages survive, hooks reconnect, no message loss.

## Tracking

Each numbered item above maps to a beads issue (one issue per item, grouped under one epic per layer). Status, evidence, and learnings are journaled there as we go. See `br ready` for what's next.
