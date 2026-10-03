# Upstream parity

oh-fx follows [vercel-labs/fx](https://github.com/vercel-labs/fx). This page records how far upstream has been reviewed and where each upstream change lands in oh-fx.

- **Sync point:** `34f1ed1` (vercel-labs/fx#1062, merged 2026-10-01). Every upstream pull request merged up to this commit has been classified.
- **Previous sync point:** `d9f7766` (vercel-labs/fx#1094).
- **Last pass:** 2026-10-01, covering `d9f7766..34f1ed1`: 60 commits and 12 merge commits (72 in all). They arrived in 8 first-parent merges, plus #1091, which was merged into #1082's branch.

## Statuses

| Status | Meaning |
|---|---|
| `ported` | oh-fx behaves as upstream does after the change, or as a recorded deliberate difference describes. |
| `defer:<area>` | The change touches an area oh-fx has not ported yet. Port that area from the latest upstream, not from the sync point, and move its rows to `ported`. |
| `omitted` | oh-fx deliberately leaves the change out. The reason is in the row and in the deliberate differences in [architecture.md](architecture.md). |
| `n/a` | Upstream-only: the Zig build, CI, release scripts, test harness code, or process docs. |

A pull request that lands in more than one place has one row per status.

Syntax-highlighting profile storage uses pointerless tables as recorded in [architecture.md](architecture.md). This representation change preserves the existing tokenizer behavior and does not complete a deferred behavior in this ledger.

## Source file map

`parity/files/` records every Git-tracked `.zig` file in the pinned upstream repository, grouped by directory. Each entry names its status and any corresponding Rust modules. Partial ports identify missing behavior; exclusions explain host entrypoints, fixtures or Zig-specific ownership machinery that does not require a Rust module. A file marked `ported` matches upstream or a deliberate difference recorded in [architecture.md](architecture.md).

The full checkout commit is read from `parity/UPSTREAM`. With that checkout available locally, run `cargo xtask parity --upstream <path>` or set `OH_FX_UPSTREAM` and run `cargo xtask parity`. The command fetches nothing and rejects a different checkout commit, missing or stale entries, duplicate entries, invalid statuses, missing required notes and nonexistent or escaping Rust module paths. It prints a count for each status. `cargo xtask lint` validates the local schema, statuses, notes and module paths without an upstream checkout, so pre-push CI includes that validation. Full coverage reads the pinned Git tree rather than dirty or untracked files. CI fetches that tree without checking out source blobs and requires coverage through Repository checks. The CI coverage gate depends on the upstream repository and pinned commit remaining reachable.

The file map measures structural coverage. Re-audit entries and missing-behavior notes when main gains an implementation. The question tool and answer codec remain partial because ignored out-of-range JSON numbers and duplicate result keys differ from upstream. Byte-exact schema, help and prompt goldens remain a separate follow-up.

## d9f7766..34f1ed1

| PR | Merge | Title | Status | oh-fx | Note |
|---|---|---|---|---|---|
| baseline | `34f1ed1` | Streamed tool lifecycle in `ask` | `ported` | `ofx-gateway`, `ofx-agent`, `ofx-contract`, `ofx-tools`, `oh-fx` | Validated Responses tool identities publish provisional progress before arguments finish; read, rejected read, parallel calls and partial failures are covered. Rejected provisional calls count no executed step. Plain transcript and retry differences are recorded in architecture.md. |
| baseline | `34f1ed1` | Streamed tool lifecycle in the interactive session | `defer:interactive` | `ofx-tui` | The interactive session ignores provisional lifecycle and assistant-boundary events; provisional rows and their replacement remain deferred. |
| #1092 | `7087733` | Keep steering typed after a tool result when a turn is cancelled | `defer:steering` | `ofx-agent` | `buildInterruptedExecutionMemory` keeps steering messages typed right after a tool result when a turn is cancelled. oh-fx has no steering yet; see [Steering](#steering). |
| #1099 | `07f4e4d` | Keep the FIFO test's reader open until its writer joins | `n/a` | none | This fixes a race in a Zig test's blocked-writer thread. The Rust port of "linked metadata FIFO is rejected before descriptor open" starts no writer thread. |
| #1072 | `d44cd84` | Add an experimental v2 session store to fx ask | `ported` | `ofx-cli`, `ofx-session`, `oh-fx` | `--sessions-v2`, before any command or inside `ask`, and `OH_FX_SESSIONS_V2` (`1`, or `true` in any letter case) are parsed. `ask` fails as not available yet unless `--no-save` is given. The ask usage and options show the flag. `v2`, in any letter case, is not a valid session id, so session listing skips the `v2` folder. |
| #1072 | `d44cd84` | (same) | `defer:sessions-v2` | future session store | Append-only v2 log (`session_manager/*`, `session_adapter.zig`), saving and resuming `ask` on v2, the per-request `append_turn_piece` hook, usage recovery, and side files under `session-files/<id>`. |
| #1072 | `d44cd84` | (same) | `n/a` | none | `build.zig` session-manager module, `scripts/pgso` corpus entry, e2e shard weights. |
| #1082 | `14893f6` | Run session commands and doctor on sessions v2 | `ported` | `ofx-cli` | The top-level `Flags:` list shows `--sessions-v2`. |
| #1082 | `14893f6` | (same) | `defer:sessions-v2` | sessions, interactive resume, ACP, subagents, doctor, terminal store | See [Sessions v2](#sessions-v2). |
| #1091 | `d02c63e` | Run more end-to-end tests against sessions v2 | `defer:sessions-v2` | future session tests | Fault-grid end-to-end tests (torn tail, flipped byte, missing blob, read-only folder, full disk, second process) and the `wiring` trace lines for host sessions. |
| #1101 | `7330832` | Add one-command Slack MCP setup | `omitted` | none | `mcp add slack`, `/mcp add slack`, `slack_preset.zig`, the MCP menu's `s` key and hint rows, and the README section. The preset depends on fx's Slack app (its public Client ID and Vercel's fx.sh OAuth bridge), so it waits until oh-fx has its own Slack app. `oh-fx mcp add slack` keeps the usage error upstream printed before the preset. |
| #1101 | `7330832` | (same) | `defer:mcp-oauth` | future MCP OAuth | `authentication_error_message` gains `McpAuthorizationDenied` ("Authorization was declined. Run the connection command again to retry") and `McpAuthorizationCallbackTimedOut`. Both apply to every MCP server. |
| #1111 | `3c89455` | Share sessions v2 history replay | `defer:sessions-v2` | future session replay | `session_adapter.zig` shares one history replay between resume, ACP load, and `fx session`. |
| #1110 | `dcf9287` | Load all Grok subscription models | `defer:grok` | future Grok model catalog | `xai_grok_models.zig`: modality metadata only adds image support; it no longer filters the list or fails it. A failed fetch, a non-200 answer, or invalid JSON is traced and ignored, and models without metadata stay with vision off. |
| #1062 | `34f1ed1` | Replace context compaction with a turn-by-turn ledger | `ported` | `ofx-agent`, `ofx-app`, `ofx-config`, `ofx-contract`, `ofx-gateway`, `ofx-text`, `ofx-tui` | The compactor, `text_completion.zig`, the `auto_compact_percent` setting, each model's context window, and automatic and provider-overflow compaction in the turn loop. `/compact` runs manual compaction through `Agent::compact`, and the footer shows its activity and feedback. See [Compactor](#compactor). |
| #1062 | `34f1ed1` | (same) | `ported` | `ofx-agent`, `ofx-session` | `ask` saves each checkpoint behind the `fx-compactor-v1` marker and a resumed session renders it again. |
| #1062 | `34f1ed1` | (same) | `defer:sessions` | future session store, `read_tool_result` | Saved `M<n>`, `T<n>`, and `L<n>` records, `read_tool_result` search, and folding earlier compactions. |
| #1062 | `34f1ed1` | (same) | `defer:interactive` | future interactive shell | The compaction activity line during automatic and provider-overflow compaction. |
| #1062 | `34f1ed1` | (same) | `defer:acp` | future ACP | ACP applies the auto compaction percent. |
| #1062 | `34f1ed1` | (same) | `defer:ai-gateway` | future Vercel AI Gateway transport | The fallback model for a failed or empty summary. |
| #1062 | `34f1ed1` | (same) | `defer:trace` | future trace log | Compaction trace lines and the `/trace` ring. |
| #1062 | `34f1ed1` | (same) | `omitted` | none | The credential check behind `ContextCompactionUnavailable`: the summary request uses the turn's own provider connection and credential. |
| #1062 | `34f1ed1` | (same) | `n/a` | none | `scripts/check-compactor-boundary.sh` and its CI steps, AGENTS.md and CONTRIBUTING.md process text, the SDK compaction test. |

The `slack` command (`slack install`, `slack status`, and `slack refresh`) predates this range and is omitted for the same reason as the Slack MCP preset. `oh-fx slack` fails as any unknown command does.

## Deferred areas

Port each area from the latest upstream. These notes list what changed in the range above, so the port can be checked against it.

### Steering

Upstream lets a prompt typed while a turn runs steer that turn. oh-fx's interactive shell queues such a prompt and sends it as the next turn instead. Steering, and with it #1092, is ported as its own change, including:

- `src/core/agent/runtime/execution_memory.zig` `buildInterruptedExecutionMemory` with `isSteering`;
- the test "interrupted execution memory keeps steering typed right after a tool result";
- the end-to-end check in `gateway-stream-lifecycle.test.ts`, where `STEERING_FIRST` appears once in the saved turn.

### Tool-result store

The private result reader supports bounded raw pages, and storage no longer applies the 8 MiB replay cap. Saved-result replay consumes the held reader. The `defer:sessions` compaction rows remain deferred until `read_tool_result`, saved M/T/L records and earlier-summary folding land.

### Sessions v2

#1072, #1082, #1091, and #1111 build an append-only v2 store beside the v1 store.

- **Module:** port `src/core/session_manager/*` and `src/core/session/session_adapter.zig`.
- **Turn loop:** `ofx-agent` needs a turn-progress sink. Upstream calls `AgentRuntimeDeps.append_turn_piece` at every model-request boundary. It calls it again with `running_calls` before tool calls run, so a crash mid-tool keeps the call.
- **`ask`:** `ask --json` reports `session_id` only after the first turn is saved, and keeps `recovery.durable` false on v2.
- **Commands:** `sessions`, `session`, `session recover`, and `doctor` run on v2. `session migrate` refuses on v2.
- **Titles:** `/rename` and title generation (`Session.rename`, `installGeneratedTitle`) write a `set title` record, and a fresh session's first turn sets the derived title.
- **Interactive, ACP, and subagents:**
  - The interactive app and ACP save and load v2 sessions.
  - Subagents become v2 children, and `subagentFailureLabel` reads `child_lost` as `Interrupted`.
  - The resume hint and the upgrade relaunch keep `--sessions-v2`.
- **Files and storage:**
  - Side files live in `session-files/<id>` under the data directory.
  - The terminal store reads v2 side folders.
  - The durable replace can report the error that stopped it before the rename (`pre_rename_cause`).

### Compactor

#1062 replaces upstream's compactor with `src/core/compactor/*`. oh-fx ports `compactor`, `window`, `summarize`, `ledger`, `lint`, `checkpoint`, `model`, and `settings` into `ofx-agent::compactor` and `ofx-contract::AutoCompactPercent`, with `text_completion.zig`, the turn reading of `execution_memory.zig`, and the request measurement of `prompt_context.zig` beside them. The turn loop compacts once a request reaches `auto_compact_percent` of the usable input and recovers once from a provider overflow, and the Codex catalog and configured `model_metadata` give each model its context window. Still to port:

- **Sessions:** compacted turns are saved as `M<n>`, tool calls as `T<n>`, and earlier compactions as `L<n>` (`records.zig`). `read_tool_result` opens them by handle and finds them with a new `request.search` alternative (one to three phrases), and its descriptions change accordingly. With a store, `summarize` folds the earlier checkpoint into an `Earlier:` summary and the notes request names the saved records; `<context_handoff>` checkpoints from older sessions are read with their state files.
- **Interactive:** the footer shows the compaction activity line (`activity_status.zig`) during automatic and provider-overflow compaction, with the turn's clock, as it already does for `/compact`.
- **ACP:** ACP applies the auto compaction percent.
- **Vercel AI Gateway:** `model.zig` sends a failed or empty summary once more to another model family.
- **Trace:** `trace.zig`'s ring and the compaction trace lines.

### MCP OAuth

Port `src/core/mcp/mcp_auth.zig` with the #1101 messages above. Slack's fx-app bridge in that file (`slack_bridge_config` and the fx.sh callback) stays out with the Slack preset.

### Grok model catalog

Port `src/gateway/xai_grok_models.zig` as of #1110 or later.

## How to run a parity pass

1. Fetch upstream.

   ```sh
   git clone https://github.com/vercel-labs/fx.git fx-upstream
   git -C fx-upstream fetch origin
   ```

2. List the first-parent merges since the sync point. Each one is an upstream pull request.

   ```sh
   git -C fx-upstream log --first-parent --oneline 34f1ed1..origin/main
   ```

   For each merge, read the pull request's commits and its changed files:

   ```sh
   git -C fx-upstream log --format='%h %s%n%b' <merge>^1..<merge>^2
   git -C fx-upstream diff --stat <merge>^1 <merge>
   ```

   A pull request merged into another pull request's branch shows up inside that merge. Give it its own row.

3. Classify each pull request. Read the diff against the oh-fx module that ports each upstream file; [architecture.md](architecture.md) maps upstream directories to crates. Then decide:
   - **ported now:** it changes code already on `main`, such as help text, flags, the turn loop, or the gateway. Port it with its upstream tests in a small pull request, and mark the row `ported`.
   - **`defer:<area>`:** it touches an area oh-fx has not ported. Add a note above so the area is ported from the latest upstream.
   - **`omitted`:** oh-fx deliberately leaves it out. Record the reason here and in architecture.md.
   - **`n/a`:** it is upstream-only.

   Check pull requests that are still open in oh-fx as well. A change to an area that exists only on an open branch belongs in that branch before it merges.

4. Add the rows under a new `<old>..<new>` heading. Move the sync point to the last merge reviewed, update the date and `parity/UPSTREAM` to its full commit, and reconcile every file-map row against the new tree. The checker requires the documented Sync point to match that pin.

When a deferred area is ported, move its rows to `ported` and update its file-map statuses in the same pull request. Partial implementations must retain concrete missing-behavior notes.
