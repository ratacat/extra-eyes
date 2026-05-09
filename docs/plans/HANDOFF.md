# Validation Run — Handoff

**Date**: 2026-05-09
**Run**: ralph-loop, iteration 2 of 25, completion promise `VALIDATION RUN COMPLETE`
**Window**: blinkandyoumissit-scribbler-jdzx
**Project**: `/Users/jaredsmith/Projects/extra-eyes`

## Goal

Walk every item in `docs/validation.md` (52 V-items across 10 layers) and produce one of three outcomes per item:

1. **Validated** — observable evidence the behavior works, captured in knb with sources.
2. **Blocked** — the implementation does not yet exist; document what would need to exist + create a `xe-*` bead so the work is traceable.
3. **Open question** — design-space ambiguity, captured as a `kind:question` row in knb with resolution criteria.

Nothing closes without concrete evidence. Knb is the journal; beads is the issue tracker; `validation-progress.md` is the at-a-glance dashboard.

## Status by layer

| Layer | Items | Status |
|---|---|---|
| L1 hook adapters | V1.1–V1.8 | **substantially done.** 7/8 items validated for ≥2 of 3 harnesses. Codex blocked on trust gate. |
| L2 daemon (eyesd) | V2.9–V2.13 | not started — eyesd does not exist |
| L3 IPC / unix socket | V3.14–V3.19 | not started — depends on L2 |
| L4 watcher process | V4.20–V4.25 | not started — depends on L2 |
| L5 context construction | V5.26–V5.30 | not started — depends on L2 |
| L6 tick triggering | V6.31–V6.34 | not started — depends on L2 |
| L7 message delivery | V7.35–V7.39 | not started — V7.37 has a free path on pi |
| L8 profile system | V8.40–V8.44 | not started — pure config work, partial spec in `extra-eyes/AGENTS.md` |
| L9 end-to-end | V9.45–V9.47 | not started — needs everything else |
| L10 failure modes | V10.48–V10.52 | not started |

L2–L10 are entirely pre-implementation. The validation pass for them is **structured documentation**: for each V-item, write a knb claim describing what "passing" looks like, the test fixture that would prove it, the design-space question(s), and a `xe-*` bead for the implementation work.

## L1 detail — what's actually validated

| | claude-code | pi | codex |
|---|---|---|---|
| V1.1 pre-prompt fires | ✓ empirical | ✓ empirical | **BLOCKED** (xe-1rg) |
| V1.2 inject into next turn | ✓ ralph-loop live | transform-only | ✓ source-level |
| V1.3 model reads injection | ✓ self-ref proof | inferred | inferred |
| V1.4 post-stop captures output | ✓ `last_assistant_message` field | ✓ `session_shutdown` | blocked |
| V1.5 user prompt captured | ✓ `prompt` field verbatim | ✓ `event.text` verbatim | blocked |
| V1.6 size ceiling | ✓ 100KB clean | untested | untested |
| V1.7 headless mode | ✓ `claude --print` | ✓ `pi --print` | untested |
| V1.8 sub-second latency | partial (hook overhead negligible) | ~2s incl LLM | untested |

## Architectural findings to remember

1. **CC Stop payload includes `last_assistant_message` directly.** The ralph-loop reference impl that does jq-extraction on `transcript_path` is over-engineered. Eyesd's CC Stop adapter is one `jq -r .last_assistant_message` away.
2. **CC and codex hook configs are nearly identical** — `{hooks:{EventName:[{matcher,hooks:[{type,command}]}]}}`. Eyesd can ship one template that works for both.
3. **pi `event.source` discriminates `interactive` vs `extension`** — V7.37 (mark watcher-originated content) is a freebie on pi. CC and codex need explicit content markers (e.g. `<eyes:WATCHER_NAME>...</eyes>`).
4. **Codex hooks require per-hook `trusted_hash`** in `[hooks.state.<key>]`. Empirically reproduced: valid config + successful `codex exec` + zero hooks fired. No CLI command for granting trust visible. Eyesd installer for codex MUST handle this or users hit silent failure. (xe-1rg)
5. **CC has `--include-hook-events` for stream-json output** — useful for eyesd debugging.
6. **Pi has `pi.events.emit/on`** — inter-extension event bus. Eyesd extensions can talk to other pi extensions cleanly.
7. **Codex hook handler types**: `command` | `prompt` | `agent`. More expressive than CC's command-only.
8. **codex feature flag** `hooks` is `stable` + `true` by default; `plugin_hooks` is `under development` + `false`. Plugin-bundled hooks may not work yet.

## What's left to do — by iteration

The ralph loop budget is 25 iterations. Used 2 so far.

- **Iter 3**: L2 daemon (V2.9–V2.13). 5 items. Document required behaviors as knb claims + create per-V `xe-*` impl beads. Stretch: dig into codex hook trust path.
- **Iter 4**: L3 IPC/socket (V3.14–V3.19). 6 items. Same pattern.
- **Iter 5**: L4 watcher process + L5 context construction (V4.20–V5.30). 11 items.
- **Iter 6**: L6 tick triggering + L7 message delivery (V6.31–V7.39). 9 items.
- **Iter 7**: L8 profile system + L9 end-to-end (V8.40–V9.47). 8 items.
- **Iter 8**: L10 failure modes + final synthesis (V10.48–V10.52). 5 items + rollup.
- **Iter 9–24**: slack — pick up codex empirical work once trust path is found, latency instrumentation, follow-up issues from L2–L10 docs.
- **Iter 25 or earlier**: emit `<promise>VALIDATION RUN COMPLETE</promise>` once every V-item has either evidence or a documented gap+follow-up bead.

## Artifact map

| Where | What |
|---|---|
| `docs/validation.md` | Source of truth — the 52 V-items, prose form |
| `docs/plans/validation-progress.md` | At-a-glance status dashboard, iteration log |
| `docs/plans/HANDOFF.md` | This file |
| `docs/plans/ralph-validation-prompt.txt` | Cleaned prompt body (ASCII-only, no apostrophes) |
| `knb/ledger.jsonl` | Append-only evidence ledger, ~50 rows |
| `knb/iter-NN-*.json` | Reusable JSON op files per iteration |
| `.beads/beads.db` + `.beads/issues.jsonl` | Issue tracker (prefix `xe`) |
| `/tmp/eyes-validation/{claude-code,codex,pi}/` | Sandboxed test fixtures |
| `/Users/jaredsmith/.claude/ralph-loop.local.md` | Loop state (iteration counter, promise) |

## How to resume

1. Read `docs/plans/validation-progress.md` for current status.
2. Pick the next layer from the iteration plan above.
3. For each V-item in that layer:
   - Open `docs/validation.md` for the canonical statement.
   - Decide: **validate**, **blocked**, or **open question**.
   - Write knb operations to `knb/iter-NN-*.json` (sources first, claims second — knb requires real source IDs).
   - Apply with `knb apply --file <path> --json`.
   - If a `xe-*` bead is needed, `cd extra-eyes && br create "..." --priority N --type task -d "..."`.
4. Update `docs/plans/validation-progress.md` checkboxes + iteration log.
5. End the turn naturally. The Stop hook will fire the next iteration.

## Known traps

- **Ralph state file**: the `/ralph-loop` setup script silently drops `--max-iterations` and `--completion-promise` when the prompt has special characters. Patched manually this run; if state ever shows `max_iterations: 0`, edit `/Users/jaredsmith/Projects/.claude/ralph-loop.local.md` directly.
- **Knb double-apply**: running the same `knb apply --file X.json` twice creates duplicate sources because knb is append-only with auto-generated IDs. Ledger has duplicate sources from iter-01 — non-toxic but worth knowing.
- **Knb apply envelope**: ops go inside `{"operations":[...]}`, not a bare array.
- **Knb cross-references**: source IDs and claim IDs are auto-generated; can't reference them in the same apply call. Two-pass pattern: apply sources, capture IDs, then apply claims that reference them.
- **`br` global DB leak**: before `br init` in extra-eyes, `br ready` was returning beads from a sibling project (Polymarket). Now isolated under `extra-eyes/.beads/` with prefix `xe`.
- **Codex sandbox auth**: `CODEX_HOME=/tmp/...` requires copying `auth.json` from the user's real `~/.codex/`.
- **Codex effort=minimal**: incompatible with `image_gen` and `web_search` tools; use `low` or higher in test configs.

## Beads created so far

- `xe-1rg` — Codex hook trust mechanism (V1.1 codex blocker, P1)
- `xe-2iy` — Validate L2-L10 (currently blocked on impl, P2) — meta-issue for the doc-pass work
- `xe-3hq` — Empirical: CC UserPromptSubmit fires on Stop-injected pseudo-user input? (P2)

## Open questions worth resurfacing

(See knb `kind:question` rows for full details + resolution criteria.)

- How does a user grant trust to a codex hook? No `codex hooks trust` subcommand visible.
- Does CC's UserPromptSubmit fire on Stop-hook-injected pseudo-user input?
- What's the exact JSON schema codex hook commands write on stdout to inject context?
- Per-harness injection size ceiling for pi and codex (CC done to 100KB).
- Real eyesd-loop latency under load (hook → unix socket → daemon → response → next turn).
