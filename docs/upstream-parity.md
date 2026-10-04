# Upstream parity

oh-fx follows [vercel-labs/fx](https://github.com/vercel-labs/fx). This page records how far upstream has been reviewed and where each upstream change lands in oh-fx.

- **Sync point:** `6bdd497` (vercel-labs/fx#1137, merged 2026-10-03). Every upstream pull request merged up to this commit has been classified.
- **Previous sync point:** `34f1ed1` (vercel-labs/fx#1062).
- **Last pass:** 2026-10-04, covering `34f1ed1..6bdd497`: 46 commits and 23 merge commits (69 in all). They arrived in 14 first-parent merges; the other 9 merges bring main into feature branches.

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

## 34f1ed1..6bdd497

Rows marked `defer:shell`, `defer:startup-probe` and `defer:ultrafast-cli` change behaviour oh-fx already ports. Each is synced by its own pull request in this pass, which moves its rows to `ported`.

| PR | Merge | Title | Status | oh-fx | Note |
|---|---|---|---|---|---|
| #1108 | `11d3425` | Make the compactor independent of the session module | `ported` | `ofx-agent` | A refactor: `rawHistoryTurnCount` and `contextHistoryRange` move unchanged into `src/core/shared/history_range.zig`, and behaviour is unchanged. The file map gains the new file. |
| #1113 | `07b4e7a` | Send libfx images as raw bytes | `defer:acp` | future ACP | Without an attachment store, `fx acp` refuses an image block that names an `_meta.fx.attachment`; base64 image `data` is read as before. |
| #1113 | `07b4e7a` | (same) | `n/a` | none | libfx hosts pass prompt images and kernel checkpoints as raw attachments beside their JSON-RPC frames (`host_attachments.zig`, `js_host_attachments.zig`), and kernel checkpoints store image bytes raw. Only the NAPI and WebAssembly entry points supply the store. The JavaScript SDK and its tests. |
| #1116 | `0107787` | Move sessions v2 bodies to blobs and terminal state to its own folder | `defer:sessions-v2` | future session store | Tool results, tool images, command replay, web-fetch downloads and compaction records become blobs of a v2 session; hosted terminal state moves to `terminal/{id}`; an ACP client's system prompt and tool identities become session settings; older v2 sessions move on their first writable open. See [Sessions v2](#sessions-v2). |
| #1120 | `b437987` | Fix compaction note retries, entry checks and damaged checkpoints | `ported` | `ofx-agent` | A reply with entries but no turn notes gets the notes follow-up. A plain-text reply counts as the notes of the newest turn, and the other turns with work are asked for again. Every entry must name its source, and an entry citing one failed tool call may not call it a success. A checkpoint whose turn or tool count passes `1 << 30` is read as its raw text. |
| #1120 | `b437987` | (same) | `defer:sessions` | future session store, `read_tool_result` | After an unreadable checkpoint, `records.zig` leaves a saved record numbered past `1 << 30` out of the numbering. |
| #1120 | `b437987` | (same) | `n/a` | none | The scripted replies of the gateway flow tests and the TUI compaction activity end-to-end test. |
| #1121 | `7e39bf1` | Avoid redundant Base64 decoding buffers | `ported` | `ofx-session` | Decoding stops re-encoding to compare, and the input accepted is unchanged. oh-fx decodes with the `base64` crate's `STANDARD` engine, which rejects non-canonical padding and trailing bits without re-encoding. |
| #1112 | `88e6658` | Add opt-in Ultrafast mode | `defer:ultrafast-cli` | `ofx-cli`, `ofx-app` | See [Ultrafast](#ultrafast). |
| #1112 | `88e6658` | (same) | `defer:ai-gateway` | future Vercel AI Gateway transport | The `openai.serviceTier: "ultrafast"` request, the `ultrafast_mode` setting and `FX_ULTRAFAST`, the model, settings and footer indicators, subagent inheritance, ACP, and the session and recovery-checkpoint fields, which upstream writes only when the mode is on. |
| #1112 | `88e6658` | (same) | `n/a` | none | SDK, `scripts/pgso` corpus and end-to-end test changes. |
| #1122 | `3cc5ddf` | Raise the CI unit test timeout | `n/a` | none | Upstream CI. |
| #1124 | `0a8a391` | Keep a running step's text when fx crashes on sessions v2 | `defer:sessions-v2` | future session store | The running save writes the issuing message's text as an `assistant_running` item with its calls. |
| #1096 | `335eef8` | Let models recover oversized images | `defer:images` | future image attachments | Before each request, images that fit (8000 pixels per side for up to 20 images, 2000 for more, and 5 MiB encoded each) are sent and the rest are replaced by recovery guidance naming the source path, instead of being downscaled; history keeps file-backed originals until request assembly; pasted `/image` paths become attachments; the history replay cache format advances. oh-fx's `read_file` returns no images yet, so its text results are unchanged. |
| #1096 | `335eef8` | (same) | `n/a` | none | SDK image recovery through host tools and request-flow test fixtures. |
| #1127 | `0994a05` | Draw the first frame before slow startup work | `defer:startup-probe` | `ofx-tui` | See [Startup probe](#startup-probe). |
| #1127 | `0994a05` | (same) | `defer:interactive` | future interactive startup | The first frame is drawn before a Keychain login is read, before skills are discovered and before the other sign-in sources are checked; a prompt sent first waits for the login; the full-transcript session details show `skills: loading`. oh-fx's event loop waits on its descriptors instead of polling, so upstream's 1 ms first-input wait has no counterpart. |
| #1127 | `0994a05` | (same) | `n/a` | none | `benchmarks/first_frame.py`, AGENTS.md and CONTRIBUTING.md text, and the single-threaded WebAssembly credential path. |
| #1129 | `d3c28f3` | Title a v2 session whose first turn crashed | `defer:sessions-v2` | future session store | A resumed v2 session without a stored title writes the title its first prompt gives with its next turn end. |
| #1135 | `463663f` | Fix false compaction check marks and close finished open entries | `ported` | `ofx-agent` | A rule may quote the user's answer in an `ask_user_question` result, though not the question. Values are also looked for in what stays in the conversation after the cut, the rest of a turn in progress included. The notes request says a status entry can replace an open entry it answers or finishes, and the open entry then shows as replaced. |
| #1135 | `463663f` | (same) | `defer:sessions` | future session store, `read_tool_result` | Folding the earlier compaction drops an open entry that a status replaced. Without saved compactions, oh-fx carries it forward, marked replaced. |
| #1135 | `463663f` | (same) | `n/a` | none | The README's compaction section, which oh-fx's README does not have. |
| #1139 | `be6d092` | Exit without waiting on MCP servers or the usage ledger | `defer:mcp` | MCP runtime | At exit, every stdio MCP server's process group gets SIGKILL in one pass after launches under way settle, for at most 1 s. A Docker-backed server, a launch that does not settle or a full kill list falls back to the full teardown. |
| #1139 | `be6d092` | (same) | `defer:sessions` | future profile usage ledger and session picker catalog | The first usage publication's ledger parse checks the lock's abandon flag every 256 lines, so exit stops it, and process exit leaves the session picker catalog to the exiting process instead of freeing each cached session. oh-fx has neither yet. |
| #1137 | `6bdd497` | Load shell startup files once per fx process | `defer:shell` | `ofx-tools`, `ofx-app`, `ofx-cli` | See [Shell tool](#shell-tool). |
| #1137 | `6bdd497` | (same) | `defer:shell-snapshot` | future shell startup snapshot | `shell_snapshot.zig`: user-profile commands capture the login shell's PATH, environment, aliases, functions and options once per process and restore them, refresh after a watched startup file changes, and fall back to full startup with a one-time notice when a capture fails, passes 8 MiB or takes over 10 s. In auto mode, a routine command whose words a capture defines as an alias or function goes to review, as does every routine command while captures fail, and remembered approvals are bound to the snapshot. |
| #1137 | `6bdd497` | (same) | `defer:tty` | future TTY sessions | The terminal host daemon, the tmux backend and the `FX_TERMINAL_HOST_*` variables are removed: tty terminals run inside the fx process and end when it exits. A resumed session's terminal answers with the new `TerminalEnded` code, and its tool row reads "ended when fx exited". Job-controlled commands end on exit. |
| #1137 | `6bdd497` | (same) | `n/a` | none | The terminal host end-to-end suite, `terminal_client_fixture.zig`, the shell-path evals and the CI shard weights. |

## d9f7766..34f1ed1

| PR | Merge | Title | Status | oh-fx | Note |
|---|---|---|---|---|---|
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
| pinned | `34f1ed1` | Activate Grok after sign-in | `defer:grok` | future OH-9 request route | Native login stores the authenticated session without changing the active provider, saved models, or `settings.json` bytes. Activation follows the Grok request route in the next OH-9 PR. |
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

## Capability search at the sync point

| Surface | Status | oh-fx | Note |
|---|---|---|---|
| Installed-skill `capability_search` | `ported` | `ofx-text`, `ofx-skills`, `ofx-tools`, `ofx-app` | Pinned schema and description, fresh policy-bound discovery, exact directory locations, intent ranking, five-result pages, UTF-8 description clipping, private cursor-envelope budgeting, and the actual search-to-`skill` consumer. Valid large queries use wider evidence counters as documented in architecture. The tool remains offered with zero skills. Interactive mode retains the empty MCP host and reports no_match for empty results; noninteractive ask reports MCP unavailable. Advertised skill identities remain retained until the next turn, matching upstream when a skill is renamed in place. |
| MCP capability search and schema loading | `defer:mcp` | future MCP host | Without a configured MCP search host, results retain upstream's `mcp_state: unavailable`; an exact server filter skips skill discovery. MCP retrieval, dynamic binding, selection, and schema loading remain pending. |

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

### Shell tool

#1137 changes surfaces oh-fx already ports:

- The `shell` description adds that each call starts a new shell with the user's startup files applied, that `cd`, `export` and alias changes do not carry over, and how zsh treats unquoted globs and words that begin with `=`.
- `shell.run` accepts `reload: true`.
- The turn context's `shell_path` names the login shell the shell tool resolves for the user profile.
- `/shell reload` makes the next command run the startup files again and resets remembered command approvals.

Without the startup snapshot, every oh-fx command already runs the startup files, so `reload` changes nothing until the snapshot is ported.

### Startup probe

#1127 sends the startup OSC 11 background query with a device attributes query behind it and ends the probe when that reply arrives, so a terminal that ignores OSC 11 costs one round trip; the 200 ms wait remains for terminals that answer neither. The primary device attributes parser moves from `theme_monitor.zig` to `theme_protocol.zig`.

### Ultrafast

#1112 changes text oh-fx already ports: the `ask`, `acp` and top-level usage lines gain `[--ultrafast|--no-ultrafast]` and their option tables the two flags, `ConflictingUltrafastFlags` reports `--ultrafast and --no-ultrafast cannot be used together`, the interactive-only model flags hint names `--ultrafast`, `/status` and `status --json` report `ultrafast_requested`, and `/ultrafast [on|off|status]` joins the slash registry. The request itself needs the Vercel AI Gateway (`defer:ai-gateway`).

### MCP OAuth

Port `src/core/mcp/mcp_auth.zig` with the #1101 messages above. Slack's fx-app bridge in that file (`slack_bridge_config` and the fx.sh callback) stays out with the Slack preset.

### Grok model catalog

Port `src/gateway/xai_grok_models.zig` as of #1110 or later with the next OH-9 request route. Native Grok sign-in currently stores the authenticated session without loading the model catalog or changing settings. Activation, model listing, credential refresh, and missing-subscription request guidance remain deferred until a real Grok request consumer is available. Optional modality metadata must add image capabilities without filtering models; #1110 remains deferred. The successful sign-in output is unchanged.

## How to run a parity pass

1. Fetch upstream.

   ```sh
   git clone https://github.com/vercel-labs/fx.git fx-upstream
   git -C fx-upstream fetch origin
   ```

2. List the first-parent merges since the sync point. Each one is an upstream pull request.

   ```sh
   git -C fx-upstream log --first-parent --oneline 6bdd497..origin/main
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
