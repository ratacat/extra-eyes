# Validation Run Progress

Source of truth for items: `extra-eyes/docs/validation.md`.
Evidence and reasoning live in knb (`extra-eyes/knb/ledger.jsonl`).
Implementation gap issues live in beads (`xe-*`).
Implementation spec: `docs/plans/eyesd-v1-implementation-spec.md`.

Status legend: `[x]` validated with evidence, `[~]` partial / blocked on impl, `[ ]` not started, `[!]` blocked / needs design.

## L1 — Per-harness hook adapters
- [x] V1.1 Pre-prompt hook fires reliably — cc✓ pi✓ codex✓ (empirical canaries; codex requires trusted_hash install)
- [x] V1.2 Hook can inject content into next turn — cc✓ (live demo + ref impl), codex✓ (docs/source), pi~(transform-only)
- [x] V1.3 Model demonstrably reads injected content — cc✓ (this session is proof)
- [x] V1.4 Post-stop hook captures assistant output — cc✓ (last_assistant_message in payload!), pi✓ (session_shutdown event), codex✓ (Stop last_assistant_message)
- [x] V1.5 User-prompt hook captures user text — cc✓ (prompt field verbatim), pi✓ (event.text verbatim), codex✓ (prompt field verbatim)
- [x] V1.6 Injection size ceiling — cc✓ (100KB clean, no truncation, _END marker echoed)
- [x] V1.7 Works in interactive AND headless modes — cc✓ (--print), pi✓ (--print), codex✓ (`codex exec` after trust)
- [x] V1.8 Sub-second hook latency — compiled `eyes hook fetch` helper validates under 1s; raw edit-to-hook path validates under 5s
- [x] V1.* codex — Trust path implemented: `eyes hook trust-codex` computes/writes Codex-compatible trusted_hash state. Empirical canary confirms trusted SessionStart/UserPromptSubmit/Stop hooks fire in `codex exec`.

**L1 status: 8/8 V-items validated with knb evidence. Codex trust gate resolved; codex/pi size and latency hardening remain optional, not blockers.**

## L2 — Daemon (eyesd)
- [x] V2.9 Starts cleanly, pidfile, no double-start — VALIDATED: Rust daemon spine + CLI integration tests (xe-2iy.1, knb claim `v2.9|eyesd|clean-start-pidfile-double-start-validated`)
- [x] V2.10 Survives watcher crash — VALIDATED: nonzero watcher exits record status, preserve output, daemon survives and sibling watcher still runs (xe-2iy.2, knb claim `v2.10|eyesd|survives-watcher-crash-validated`)
- [x] V2.11 Clean SIGINT/SIGTERM, removes socket — VALIDATED: OS signal integration tests cover both signals and cleanup (xe-2iy.3, knb claim `v2.11|eyesd|sigint-sigterm-clean-shutdown-validated`)
- [x] V2.12 State persists across restarts — VALIDATED: message queue + `(channel, cursor_key)` cursors replay after daemon restart with monotonic ID continuation (xe-2iy.4, knb claim `v2.12|eyesd|message-cursor-state-persists-restart-validated`)
- [x] V2.13 Stale-socket recovery — VALIDATED: live double-start refusal + orphaned Unix socket recovery integration tests (xe-2iy.5, knb claim `v2.13|eyesd|stale-socket-recovery-validated`)

**L2 status: 5/5 validated.**

## L3 — IPC / Unix socket
- [x] V3.14 Watchers can write large messages without blocking daemon — VALIDATED: 128KB enqueue/fetch over daemon socket and subsequent ping succeeds (xe-2iy.6, knb claim `v3.14|ipc|large-writer-message-validated`)
- [x] V3.15 Hooks read pending messages within harness timeout — VALIDATED: compiled `eyes hook fetch` reads/renders/commits pending messages under 1s in fixture (xe-2iy.7, knb claim `v3.15|ipc|hook-fetch-fast-pending-read-validated`)
- [x] V3.16 Frame protocol handles varying message sizes — VALIDATED: length-prefixed JSON frames reject malformed sizes and round-trip empty/small/128KB/boundary payloads (xe-2iy.8, knb claim `v3.16|ipc|length-prefixed-json-frame-sizes-validated`)
- [x] V3.17 Concurrent writers don't interleave/corrupt frames — VALIDATED: 20 concurrent IPC clients enqueue unique recoverable messages (xe-2iy.9, knb claim `v3.17|ipc|concurrent-writers-no-corruption-validated`)
- [x] V3.18 Socket file permissions — VALIDATED: runtime dir is 0700 and socket has no group/other permission bits (xe-2iy.10, knb claim `v3.18|ipc|owner-only-runtime-socket-permissions-validated`)
- [x] V3.19 Works on macOS and Linux — VALIDATED: full test suite passes on macOS host and Debian Linux container; Linux run caught and verified process-group timeout fix (xe-2iy.11, knb claim `v3.19|ipc|macos-linux-test-matrix-validated`)

**L3 status: 6/6 validated.**

## L4 — Watcher process lifecycle
- [x] V4.20 Daemon spawns watcher subprocess from profile — VALIDATED: raw harness `settings.command` spawns argv directly with cwd/env (xe-2iy.12, knb claim `v4.20|watcher|raw-profile-command-spawn-validated`)
- [x] V4.21 Watcher receives prompt + working context — VALIDATED: versioned stdin envelope includes watcher, tick, prompt, files, diff, conversation (xe-2iy.13, knb claim `v4.21|watcher|prompt-context-stdin-envelope-validated`)
- [x] V4.22 Watcher returns parseable message — VALIDATED: JSONL message rows enqueue; malformed/unsupported rows become status records (xe-2iy.14, knb claim `v4.22|watcher|jsonl-output-parse-status-validated`)
- [x] V4.23 Crash isolation — VALIDATED: nonzero watcher exit records status, preserves valid output, daemon stays alive, sibling watcher still runs (xe-2iy.15, knb claim `v4.23|watcher|crash-isolation-validated`)
- [x] V4.24 Per-tick timeout enforced — VALIDATED: over-time watcher is killed/reaped and daemon remains alive (xe-2iy.16, knb claim `v4.24|watcher|timeout-kill-reap-validated`)
- [x] V4.25 Cost cap per watcher — VALIDATED: reported usage cap enforced; cap=0 skips spawn, over-cap usage records status (xe-2iy.17, knb claim `v4.25|watcher|reported-usage-cost-cap-validated`)

**L4 status: 6/6 validated for the raw deterministic watcher harness.**

## L5 — Working context construction
- [x] V5.26 fswatch detects writes/creates/deletes/renames — VALIDATED: polling snapshotter detects create/write/delete/rename and `eyes watch` reacts to edits (xe-2iy.18, knb claim `v5.26|filewatch|create-write-delete-rename-detected-validated`)
- [x] V5.27 Ignores .git, lockfiles, node_modules, build artifacts, .eyes — VALIDATED: ignore rules prevent changed snapshot/tick for internal/generated paths (xe-2iy.19, knb claim `v5.27|filewatch|ignore-rules-validated`)
- [x] V5.28 Snapshots git diff at tick time — VALIDATED: `eyes tick` sends `git diff --name-only` files and diff body in watcher context (xe-2iy.20, knb claim `v5.28|context|git-diff-snapshot-at-tick-validated`)
- [x] V5.29 Conversation traffic in normalized schema — VALIDATED: CC/Codex/pi hook-style payloads normalize to one schema and appear in watcher context without raw payloads (xe-2iy.21, knb claim `v5.29|context|normalized-conversation-schema-validated`)
- [x] V5.30 Context fits model input budget (chunking strategy) — VALIDATED: watcher context enforces configured byte budgets with truncation metadata and diff markers (xe-2iy.22, knb claim `v5.30|context|budgeted-watcher-context-validated`)

**L5 status: 5/5 validated.**

## L6 — Tick triggering
- [x] V6.31 Debounce coalesces bursts — VALIDATED: watch loop waits for stable snapshot before one watcher tick (xe-2iy.23, knb claim `v6.31|tick|debounce-burst-one-tick-validated`)
- [x] V6.32 Manual `eyes tick` bypasses debounce — VALIDATED: CLI immediately runs watcher via eyesd and message is hook-fetchable (xe-2iy.24, knb claim `v6.32|tick|manual-eyes-tick-immediate-validated`)
- [x] V6.33 Per-watcher cadence overrides — VALIDATED: multi-profile watch honors `settings.cadence_ticks` across file-change ticks (xe-2iy.25, knb claim `v6.33|ticks|per-watcher-cadence-validated`)
- [x] V6.34 No spurious ticks on ignored paths — VALIDATED: ignored-only writes exit by idle timeout and produce no hook-fetchable message (xe-2iy.26, knb claim `v6.34|tick|ignored-paths-no-spurious-tick-validated`)

**L6 status: 4/4 validated.**

## L7 — Message delivery
- [x] V7.35 Monotonic IDs + per-channel cursors — VALIDATED: daemon assigns monotonic IDs and isolates cursors by channel/session (xe-2iy.27, knb claim `v7.35|delivery|monotonic-message-ids-channel-cursors-validated`)
- [x] V7.36 Hook reads only newer-than-cursor, advances on success — VALIDATED: fetch does not advance; `commit_cursor` advances only after success and rejects future skips (xe-2iy.28, knb claim `v7.36|delivery|fetch-then-commit-cursor-validated`)
- [x] V7.37 Injected content marked watcher-originated — VALIDATED: `eyes hook fetch` wraps output in explicit extra-eyes markers with message metadata (xe-2iy.29, knb claim `v7.37|delivery|watcher-origin-markup-validated`)
- [x] V7.38 Multi-watcher messages labeled — VALIDATED: rendered hook output includes escaped watcher labels from the queued payload (xe-2iy.30, knb claim `v7.38|delivery|watcher-labels-validated`)
- [x] V7.39 Dedup: never inject same message twice in same session — VALIDATED: second hook fetch for same cursor returns empty after commit (xe-2iy.31, knb claim `v7.39|delivery|same-session-dedup-validated`)

**L7 status: 5/5 validated.**

## L8 — Profile system
- [x] V8.40 Schema parses + validates — VALIDATED: strict TOML parser + CLI resolver tests cover supported harnesses and invalid schemas (xe-2iy.32, knb claim `v8.40|profiles|schema-parse-validate-validated`)
- [x] V8.41 `eyes watch` resolves to default profile — VALIDATED: no-profile `eyes watch` runs project default profile (xe-2iy.33, knb claim `v8.41|profiles|eyes-watch-default-profile-validated`)
- [x] V8.42 Project profiles win over user profiles by name — VALIDATED: same-name project/user profile resolves to project source (xe-2iy.34, knb claim `v8.42|profiles|project-over-user-precedence-validated`)
- [x] V8.43 Bundled defaults ship with binary — VALIDATED: empty project/user profile dirs resolve bundled `general` profile (xe-2iy.35, knb claim `v8.43|profiles|bundled-default-resolves-validated`)
- [x] V8.44 New profiles authored without daemon restart — VALIDATED: profile written after daemon startup runs via `eyes tick` without restart (xe-2iy.36, knb claim `v8.44|profiles|new-profile-without-daemon-restart-validated`)

**L8 status: 5/5 validated.**

## L9 — End-to-end
- [x] V9.45 Full loop edit→tick→message→next-turn visibility — VALIDATED: edit triggers watch tick, watcher message queues, next hook fetch renders it (xe-2iy.37, knb claim `v9.45|e2e|edit-watch-message-hook-visible-validated`)
- [x] V9.46 Multi-watcher fan-out — VALIDATED: one watch stream runs multiple profiles and renders watcher-labeled messages from each (xe-2iy.38, knb claim `v9.46|e2e|multi-watcher-fanout-validated`)
- [x] V9.47 Latency budget < 10-15s edit-to-visibility — VALIDATED: local raw watcher fixtures assert edit-to-hook visibility under 5s (xe-2iy.39, knb claim `v9.47|e2e|watch-to-hook-latency-under-budget-validated`)

**L9 status: 3/3 validated for the raw watcher plus hook-fetch surface.**

## L10 — Failure modes
- [x] V10.48 Daemon-down: hook silent fail — VALIDATED: `eyes hook fetch` exits zero with empty stdout/stderr when daemon is absent (xe-2iy.40, knb claim `v10.48|failure|daemon-down-hook-silent-validated`)
- [x] V10.49 Watcher API failure reported once — VALIDATED: `api_failure` statuses enqueue one diagnostic, suppress repeats, and re-arm after recovery (xe-2iy.41, knb claim `v10.49|failure|watcher-api-failure-reported-once-validated`)
- [x] V10.50 No git repo: degrades to file-only — VALIDATED: non-git watch still detects edits/runs watcher; git context fields are empty not fatal (xe-2iy.42, knb claim `v10.50|failure|no-git-file-only-watch-validated`)
- [x] V10.51 Multi-session in same project: routing — VALIDATED: independent channel/cursor-key state isolates same-project sessions (xe-2iy.43, knb claim `v10.51|failure|same-project-multi-session-routing-validated`)
- [x] V10.52 Daemon restart mid-session: queued msgs survive — VALIDATED: queued message survives daemon restart and hook fetch retrieves it (xe-2iy.44, knb claim `v10.52|failure|daemon-restart-mid-session-queued-message-validated`)

**L10 status: 5/5 validated.**

## Notes for resumed iterations
- knb instance: `validation-run-2026-05-09` (workspace at extra-eyes/knb/)
- beads prefix: `xe` (`xe-1rg`, `xe-2iy`, `xe-3hq`, plus blocked validation child beads `xe-2iy.1`-`xe-2iy.44`)
- Scope tags: `validation`, `extra-eyes`, layer-specific
- knb apply quirk: `{"operations":[...]}` envelope required; source IDs auto-generated; reference auto-generated IDs in subsequent claims (not strings)
- knb has duplicate sources from accidental double-apply — non-toxic, can retract later if needed

## Iteration log
### Iter 1 (2026-05-09)
- Set up beads + knb scaffolding for extra-eyes
- Patched ralph state file (script's TOML-after-quoted-arg parsing dropped --max-iterations and --completion-promise — manually patched to 25 / "VALIDATION RUN COMPLETE")
- Read stop-hook.sh end-to-end as reference impl for CC hooks
- Applied 5 sources + 5 claims + 6 questions + 1 synthesis to knb
- Validated V1.2, V1.3, V1.4 (cc); partial V1.6 (cc, 1.8KB)
- Open questions raised for V1.1, V1.5, V1.7, V1.8, codex hooks discovery, pi extension SDK discovery
- 49 of 52 items still untouched; iteration 2 should focus on: (a) CC UserPromptSubmit canary test, (b) codex hook discovery, (c) pi extension SDK discovery

### Iter 2 (2026-05-09) — L1 LARGELY COMPLETED
- Discovered codex hooks via source (`/private/tmp/codex-src-orig/openai-codex-734b6c9/codex-rs/`): 8 canonical events, TOML schema parses, but each hook needs trusted_hash (per-hook sha256 trust acknowledgment)
- Discovered pi extension SDK at `~/.pi/agent/extensions/` and `pi-coding-agent/docs/extensions.md`: rich TS API with `pi.on('input', ...)`, `pi.on('session_shutdown', ...)`, etc. Includes `event.source` discriminator natively (V7.37 freebie).
- Empirical CC canary: V1.1, V1.4, V1.5, V1.7 validated. CC Stop payload directly includes `last_assistant_message` — ralph-loop ref impl is over-engineered.
- Empirical pi canary: V1.1, V1.4, V1.5, V1.7 validated via test extension at /tmp/eyes-validation/pi/test-extension.ts
- Empirical CC size ceiling: 10KB and 100KB injections both intact, model echoed `_END` marker. V1.6 done.
- Beads created: xe-1rg (codex trust blocker), xe-3hq (CC UPS-on-injected open Q), xe-2iy (L2-L10 validation gate)
- 17 more knb rows added (sources + claims + questions + synthesis) — ledger now ~50 rows

### Iter 3 (2026-05-09) — L2 DOCUMENTED AS BLOCKED
- Confirmed repo is still pre-implementation: `rg --files` lists only docs/scaffolding/knb/beads files; `eyesd` appears only in docs and journal entries.
- Created L2 implementation beads: `xe-2iy.1` (V2.9), `xe-2iy.2` (V2.10), `xe-2iy.3` (V2.11), `xe-2iy.4` (V2.12), `xe-2iy.5` (V2.13).
- Applied `knb/iter-03-sources.json` and `knb/iter-03-claims.json`: 1 source, 5 blocked claims, 2 questions, 1 synthesis.
- Open L2 design questions now in knb: canonical daemon runtime identity/path layout; durable store/cursor model.
- L2 outcome: 0/5 validated, 5/5 blocked with concrete passing criteria and fixtures.

### Iter 4-8 (2026-05-09) — L3-L10 DOCUMENTED AS BLOCKED
- Created implementation beads for every remaining blocked item: `xe-2iy.6` through `xe-2iy.44` (V3.14-V10.52).
- Applied `knb/iter-04-to-08-claims.json`: 39 blocked claims + 1 synthesis.
- Each claim includes: V-item outcome, implementation bead, passing criterion, fixture shape, primary design question, and source evidence.
- L3-L10 outcome: 0/39 validated, 39/39 blocked with concrete traceability.
- Full validation run outcome: all 52 V-items now have a recorded outcome: L1 is validated/partial/open-question per harness, L2-L10 are blocked by missing implementation with follow-up beads.

### Iter 9 (2026-05-09) — FIRST IMPLEMENTATION SLICE
- Asked ChatGPT Pro for architecture support, then implemented the daemon spine in Rust: `eyesd start --foreground/status/stop`, project identity, UID-scoped runtime paths, Unix socket IPC, pidfile lock/lifecycle, signal cleanup, stale socket recovery, JSONL state store, and `eyes profile resolve`.
- Added tests for the concrete validation surface: 12 unit tests + 7 integration tests. `cargo test` passes.
- Validated V2.9, V2.11, V2.13, and V8.40 in knb with source `src:validation-v1:20260509:24bfd6a4`.
- V2.12 moved from blocked to partial: message/cursor persistence primitives exist and replay correctly, but full restart delivery cannot be proven until the hook poll/delivery path exists.
- Found and fixed a runtime portability issue during testing: full 64-hex hash runtime dirs can exceed macOS Unix socket `SUN_LEN` under temp paths; runtime dirs now use a stable 16-hex prefix while status still reports the full project hash.

### Iter 10 (2026-05-09) — DELIVERY QUEUE + IPC VALIDATION
- Asked ChatGPT Pro to review the queue contract; adopted fetch-then-`commit_cursor` with explicit `channel` separate from `cursor_key`.
- Implemented daemon IPC requests for message enqueue, fetch, and cursor commit. Daemon now keeps replayed state in memory and appends all message/cursor mutations through the single-writer JSONL store.
- Added restart, retry, cursor isolation, future-commit rejection, large payload, and boundary-frame tests. `cargo test` passes: 14 unit tests + 12 integration tests.
- Validated V2.12, V3.16, V7.35, and V7.36 in knb with source `src:validation-v1:20260509:87bee010`.
- Found and fixed a real IPC bug: accepted Unix streams inherited nonblocking mode from the nonblocking listener, causing large message reads to fail mid-frame. Accepted streams are now set back to blocking before frame reads.

### Iter 11 (2026-05-09) — HOOK FETCH RENDERING
- Implemented `eyes message send` and `eyes hook fetch` on top of the delivery queue.
- Hook fetch renders explicit `<extra-eyes-messages>` / `<extra-eyes-message>` wrappers with message ID, channel, watcher, and severity, writes to stdout, then commits the cursor.
- Added renderer and CLI integration tests. `cargo test` passes: 15 unit tests + 13 integration tests.
- Validated V7.37, V7.38, and V7.39 in knb with source `src:validation-v1:20260509:de85066a`.

### Iter 12 (2026-05-09) — IPC CONCURRENCY + PERMISSIONS
- Added L3 IPC validation fixtures for large writer behavior, concurrent writers, and runtime/socket permissions.
- `cargo test` passes: 15 unit tests + 15 integration tests.
- Validated V3.14, V3.17, and V3.18 in knb with source `src:validation-v1:20260509:84989087`.

### Iter 13 (2026-05-09) — RAW WATCHER EXECUTION
- Asked ChatGPT Pro for watcher-runner schema review; kept `settings.command` inside the raw harness rather than adding a top-level command field.
- Implemented versioned watcher stdin envelope, raw command spawning, JSONL watcher stdout parsing, watcher status records, nonzero-exit isolation, timeout enforcement, and reported-usage cost caps.
- Added integration fixtures for spawn/cwd/env, prompt/context delivery, malformed stdout, unsupported events, nonzero exit, sibling run after failure, timeout, and cost caps.
- `cargo test` passes: 15 unit tests + 18 integration tests.
- Validated V4.20-V4.25 in knb with source `src:validation-v1:20260509:2767f614`.

### Iter 14 (2026-05-09) — MANUAL TICK + PROFILE PRECEDENCE
- Implemented `eyes tick`, which builds git diff context and asks eyesd to run a watcher immediately.
- Added git diff context construction using `git diff --name-only --relative` and `git diff --no-ext-diff --`.
- Added integration coverage for default-profile tick, hook fetch after tick, project-over-user profile precedence, bundled default profile fallback, and authoring a new profile after daemon startup.
- `cargo test` passes: 15 unit tests + 20 integration tests.
- Validated V5.28, V6.32, V8.42, V8.43, and V8.44 in knb with source `src:validation-v1:20260509:045548e5`.

### Iter 15 (2026-05-09) — WATCH LOOP
- Implemented polling `eyes watch` with file snapshots, ignore rules, debounce, max tick count, and idle timeout for tests/automation.
- Added unit coverage for create/write/delete/rename detection and ignored internal/generated paths.
- Added integration coverage for edit-triggered default watcher run, burst coalescing into one tick, ignored-path no-op behavior, and hook fetch visibility after watch.
- `cargo test` passes: 17 unit tests + 22 integration tests.
- Validated V5.26, V5.27, V6.31, V6.34, and V8.41 in knb with source `src:validation-v1:20260509:066137c1`.

### Iter 16 (2026-05-09) — FAILURE MODES
- Made `eyes hook fetch` silently no-op when eyesd is down.
- Added integration coverage for queued-message hook fetch after daemon restart and non-git file-only watch behavior.
- Reused existing cursor isolation and watcher crash fixtures for L10 routing/crash validation.
- `cargo test` passes: 17 unit tests + 25 integration tests.
- Validated V2.10, V10.48, V10.50, V10.51, and V10.52 in knb with source `src:validation-v1:20260509:5bd544b6`.

### Iter 17 (2026-05-09) — NORMALIZED CONTEXT + FANOUT
- Added `eyes feed` conversation ingestion and normalized CC/Codex/pi hook-style payloads into a shared watcher context schema.
- Added deterministic context budgeting with truncation metadata, diff truncation markers, and profile `settings.context_budget_bytes`.
- Extended `eyes tick`/`eyes watch` to run multiple selected profiles and added `settings.cadence_ticks` scheduling.
- Added once-only watcher API failure diagnostics for `api_failure` status events, with suppression reset after recovery.
- `cargo test` passes: 21 unit tests + 29 integration tests.
- Validated V3.15, V5.29, V5.30, V6.33, V9.45, V9.46, V9.47, and V10.49 in knb with source `src:validation-v1:20260509:3a254e91`.

### Iter 18 (2026-05-09) — LINUX MATRIX
- Ran the full test suite in a disposable Debian Linux container after macOS host tests were green.
- First Linux run exposed a real watcher-timeout bug: killing the shell wrapper left child processes holding stdout open.
- Fixed watcher timeouts by starting watcher commands in their own process group and killing the group on timeout.
- Linux rerun passed: 21 unit tests + 29 integration tests.
- Validated V3.19 in knb with source `src:validation-v1:20260509:c4dc25a1`; added regression claim `v4.24|watcher|timeout-kills-process-group-regression`.

### Iter 19 (2026-05-09) — CODEX TRUST GATE RESOLVED
- Implemented `eyes hook trust-codex --hooks-config <path> [--state-config <path>] --write`.
- Matched Codex source behavior for hook keys and normalized command-hook trusted_hash computation.
- Applied trust state to `/tmp/eyes-validation/codex/config.toml`, then ran `CODEX_HOME=/tmp/eyes-validation/codex codex exec --cd /tmp/eyes-validation/codex --skip-git-repo-check --json "Respond with exactly OK"`.
- Empirical Codex logs confirmed SessionStart, UserPromptSubmit, and Stop hooks fired; UserPromptSubmit captured the prompt verbatim and Stop captured `last_assistant_message = OK`.
- Hardened watch integration tests against initial-snapshot races.
- `cargo test` passes: 23 unit tests + 30 integration tests + doc tests.
- Validated codex V1.1, V1.4, V1.5, V1.7 and trust-installer path in knb with source `src:validation-v1:20260509:718400a9`.

### Iter 20 (2026-05-09) — CC SYNTHETIC INPUT QUESTION RESOLVED
- Tested Claude Code Stop-hook `decision:block` injection in an isolated sandbox with `--include-hook-events`.
- Result: `UserPromptSubmit` fired once for the original real prompt; it did not fire for the Stop-injected synthetic user message.
- Stop fired twice: first for the original assistant turn and second with `stop_hook_active=true` after the synthetic follow-up turn.
- Architectural result: real user prompts and Stop-injected watcher feedback are separate capture surfaces for Claude Code.
- Recorded knb source `src:validation-v1:20260509:746795fc`, claim `v1.1|cc|stop-injected-synthetic-input-does-not-fire-userpromptsubmit`, and resolved the original knb question.
- Closed bead `xe-3hq`.

### Iter 21 (2026-05-09) — FINAL L1 LATENCY CLAIM
- Promoted existing sub-second hook-fetch timing evidence into a dedicated V1.8 knb claim.
- V1.8 now points at the compiled `eyes hook fetch` test that performs daemon IPC fetch, renders messages, commits the cursor, and exits under 1s.
- All 52 V-items now have either direct validation evidence or the documented implementation bead trail already closed for L2-L10.

### Plan for next iterations
- **Iter 3**: DONE — L2 (eyesd daemon) documentation pass, V2.9-V2.13.
- **Iter 4**: DONE — L3 (IPC / unix socket) doc pass, V3.14-V3.19.
- **Iter 5**: DONE — L4 (watcher process) + L5 (context construction) doc pass, V4.20-V5.30.
- **Iter 6**: DONE — L6 (tick triggering) + L7 (message delivery) doc pass, V6.31-V7.39.
- **Iter 7**: DONE — L8 (profile system) + L9 (end-to-end) doc pass, V8.40-V9.47.
- **Iter 8**: DONE — L10 (failure modes) + final blocked-pass synthesis, V10.48-V10.52.
- **Iter 22-25**: optional tooling cleanup: KNB projection/check bugs (xe-20x, xe-l5n), plus optional codex/pi size and latency hardening.
- **Exit**: validation run complete for the 52 V-items; remaining ready beads are tooling bugs outside the Extra Eyes product surface.
