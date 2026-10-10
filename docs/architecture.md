# Architecture

oh-fx is a behavior-faithful Rust port of [vercel-labs/fx](https://github.com/vercel-labs/fx). Upstream behavior is the specification: tool schemas, prompts, help text, and terminal output match upstream unless a [deliberate difference](#deliberate-differences-from-upstream) is recorded.

## Goals

- **A complete port.** Every upstream feature is ported. A feature is left out only for a concrete reason recorded under deliberate differences, such as the Slack app, which depends on credentials that belong to Vercel. Being hard to port is not a reason.
- **Upstream's footprint.** oh-fx is as fast, as small, and as frugal as upstream. Its release builds are held to upstream's on binary size, startup time, time to first request, memory use, and rendering cost, and a change that regresses any of them needs a stated reason.

The shipping target is the native terminal binary. Upstream WASM, Node-API addons, browser runtimes and other JavaScript-host surfaces are outside this target. The parity inventory includes their source files and marks them as not applicable with this reason. Upstream Zig build machinery, benchmark programs and test-only harness files are also not shipped; excluding those harnesses does not mark the product behavior they exercise as ported. Implementation rows track that behavior independently.

## Principles

- **Port leaf behavior, not upstream coupling.** Escape parsing, markdown, syntax highlighting, glob matching, git-backed file listing, the frame renderer, and MCP transports are ported from upstream, and their upstream tests become ours. Upstream's large shared structs (`DispatchContext`, `AgentRuntimeDeps`) are replaced by narrow traits and constructor injection.
- **Contracts first.** The agent and the TUI depend only on `ofx-contract`, so they can be built and tested with fakes while transports and tools land in parallel.
- **Module names follow upstream files.** A Rust module that ports `src/core/tooling/tool_dispatch.zig` is named `tool_dispatch.rs`, which keeps the parity ledger mechanical.
- **Every boundary is a compile-time fact.** Crates are layered and acyclic. Crate dependencies remain acyclic: composition owns sessions and configuration, and transports cannot reach product state.

## Crates

| Layer | Crate | Owns | Upstream |
|---|---|---|---|
| 0 | `ofx-contract` | Ids, messages, model requests and stream events, tool traits and metadata, tool context targets, tool result errors, permission modes and decisions, modes and their tool policy, reasoning effort, subagent requests and results, lifecycle hooks, UI events and commands | `core/shared/types`, `agent/stream_provider`, `core/tooling` contracts, `core/modes`, `core/subagent` model contract, `core/workspace/context_contract` targets, `core/hooks`, UI contracts |
| 0 | `ofx-text` | Display width, graphemes, terminal-safe text, token estimates, lexical relevance, language scripts | `core/shared/*` text utilities |
| 0 | `ofx-trace` | The opt-in trace log, its trace ids, and the bounded ring the `/trace` diagnostics share | `core/shared/debug_trace`, the rings behind `core/workspace/diagnostics` |
| 1 | `ofx-config` | XDG profile paths, private profile storage, settings layers, provider ids, context limits, custom provider connections | `core/config`, `shared/profile_paths`, durable paths in `shared/io` |
| 1 | `ofx-http` | HTTP client, TLS trust, proxy, SSE decoding | `shared/http_pool`, `gateway/sse` |
| 1 | `ofx-shell` | Shell command lexing, classification, effects, and risk notes | `core/shell_command`, `core/tooling/command_policy` |
| 1 | `ofx-markdown` | Streaming markdown, syntax highlighting, diffs | `agent/presentation`, `core/output` |
| 1 | `ofx-vt` | Terminal screen model used by the renderer and tests | `core/terminal/engine` |
| 1 | `ofx-jsonrpc` | JSON-RPC framing shared by MCP and ACP | `acp/jsonrpc`, MCP framing |
| 1 | `ofx-images` | Image type sniffing, loading image files the user attaches, their verified snapshots, and checking the images tools return | `core/images` |
| 2 | `ofx-workspace` | Path resolution, git file listing, the `@` file index and path completion, glob, literal grep, read tracking, the status line workspace identity, additional directories and their saved changes | `core/workspace` |
| 2 | `ofx-exec` | Processes, process groups, managed shell sessions, PTYs | `core/execution`, `core/terminal` |
| 2 | `ofx-upgrade` | Self-upgrade from GitHub Releases | `core/upgrade` |
| 3 | `ofx-gateway` | Vercel AI Gateway, OpenAI-compatible, and Codex Responses transports, model catalog | `src/gateway`, `core/gateway` |
| 3 | `ofx-session` | Session ids, storage, replay, result store, prompt history, usage | `core/session` |
| 3 | `ofx-permissions` | Rules, command admission, grants, auto-mode review | `core/permissions` |
| 3 | `ofx-auth` | Credential storage, login providers, and login flows | `core/auth` |
| 3 | `ofx-skills` | Skill discovery, frontmatter, catalog | `core/skills` |
| 3 | `ofx-github` | Git snapshots and drafting prompts for the `pr` and `issue` workflows | `core/github` |
| 4 | `ofx-cli` | Command spec table, argument parsing, help rendering | `core/cli`, `core/slash_commands` |
| 4 | `ofx-tools` | Built-in tools, one module per upstream tool | `src/tools`, `builtins/tools` |
| 4 | `ofx-mcp` | MCP client: stdio, streamable HTTP, legacy SSE, OAuth | `core/mcp` |
| 4 | `ofx-agent` | Turn loop, tool batches, project context delivery, compaction, steering, subagent domain | `core/agent`, `core/subagent`, `core/workspace/context_contract` delivery |
| 4 | `ofx-tui` | Event loop, input, composer, footer, transcript, frame rendering | `src/ui`, `core/input` |
| 5 | `ofx-app` | Composition, controller, command handlers, hosts | `core/app`, `src/builtins` |
| 5 | `ofx-acp` | Agent Client Protocol server | `src/acp` |
| 6 | `oh-fx` | Binary entry point and fast paths | `src/main.zig` |
| dev | `ofx-testkit` | Fake provider and MCP servers, PTY driver, TLS and proxy fixtures | test harnesses |

Every crate may depend on `ofx-contract`, `ofx-text`, and `ofx-trace`. Otherwise a crate depends only on crates in lower layers. `ofx-agent` and `ofx-tui` depend on nothing but layer 0 crates and `ofx-markdown`/`ofx-vt` for the TUI.

## Type ownership

- A type lives in the crate that ports the upstream file defining it. `core/shared/types` and the tool dispatch contract map to `ofx-contract`, so `PermissionMode`, `ReasoningEffort`, `ApprovalPolicy`, tool metadata, and tool result errors live there. `ProviderId` and context limits live in `ofx-config`, session ids in `ofx-session`, and shell command analysis in `ofx-shell`. `AutoCompactPercent`, which upstream defines in `core/compactor/settings.zig`, lives in `ofx-contract` because `ofx-config` reads the setting and `ofx-agent` applies it, and neither may depend on the other. The targets a tool call acts on (`ApplicableTarget`, `TargetKind`) live in `ofx-contract` with the tool and permission traits that report them; the project context provider, its result, and the delivery state live in `ofx-agent` beside `RuntimeContext`, since only the agent drives them. The subagent request contract upstream keeps in `core/subagent/domain.zig`, `model_contract.zig`, and `tool_provider.zig` (request limits and validation, creation overrides, the persistent plan, the request fingerprint, terminal and feedback results, and the provider trait) lives in `ofx-contract`, because `ofx-tools` decodes the `subagent` tool's requests and the host that runs children executes them, and neither may depend on the other. The subagent rows of `core/tooling/tool_presentation.zig` live beside the other plain action formats in `ofx-contract`. The lifecycle hooks of `core/hooks` (their definitions, the registering runtime, and its frozen view) live in `ofx-contract`, because upstream dispatches them from the shell's prompts, which `ofx-tui` ports, and from the turn loop, which `ofx-agent` ports, and neither crate may depend on the other; `ofx-app` registers the first-party handlers. The settings catalog of `core/config/settings_catalog.zig` (its categories, rows, choices, search, and snapshot) lives in `ofx-contract`, because the controller in `ofx-app` builds its snapshots and applies its changes while the shell in `ofx-tui` lays them out. The usage report types of `core/session/usage_report.zig` (scopes, completeness, coverage, generation facts, pending markers, incidents, and rolling reports) live in `ofx-contract`, because `ofx-session` builds them from its ledgers while the `/usage` dashboard in `ofx-tui`, which may not depend on `ofx-session`, will lay them out. The subagent host of `core/subagent` (the child registry, child execution, the managed owner, and the tool host) lives in `ofx-agent`, which runs each child through its own turn loop; the composition root supplies child agents through `ChildAgents` and receives their approval requests through it.
- A crate that needs another crate's type depends on its owner and never keeps a copy. `ofx-cli` validates provider ids, session ids, and login providers by calling their owners while parsing, as upstream does.
- Public items need a consumer outside their crate. A crate lands with its first consumer, or directly before it in the same stack of pull requests. A type moves to its owner in the pull request that adds its first outside consumer.

## Key decisions

| Concern | Decision | Reason |
|---|---|---|
| Async | tokio for the agent, transports, MCP, and processes; no runtime on fast paths such as `--version` | Cancellation of in-flight streams and process trees |
| UI thread | A plain OS thread polling the tty, a wake pipe, and a signal pipe | Rendering never competes with tool tasks, and it mirrors upstream's event loop |
| Agent and UI contract | `UiEvent` and `UiCommand` data, which gain serialization with their first wire consumer (ACP or a JSON event stream). Approvals and questions are correlated by `RequestId` through a map of pending oneshots | One event stream drives the TUI, `ask --json`, ACP, and test fixtures |
| HTTP and TLS | reqwest with rustls and the ring provider. On Linux, a verifier of our own reads one CA bundle, or `SSL_CERT_FILE` and `SSL_CERT_DIR`, on the first TLS handshake and adds a connection's `tls.ca_file`; macOS and Windows use rustls-platform-verifier with extra roots from `SSL_CERT_FILE` or `tls.ca_file` | Corporate CAs and proxies work, static musl builds need no cmake, and neither startup nor plain HTTP pays for parsing the system's roots |
| CLI | Top-level command and option records use integer offsets into a static byte arena; aliases and detail lines use integer spans. Original definitions are consumed at compile time, with no startup initialization. Help navigation and slash tables keep their existing representation. Arguments are scanned as upstream scans them: no flag clustering, and values may start with `-` | One source for `--help`, `/help`, and the slash menus, with upstream's exact acceptance rules |
| Terminal | rustix plus a port of upstream's escape parser | Upstream needs OSC 11, DSR, and DA1 replies that crossterm drops |
| Renderer | A `FrameSink` seam. A simple live-region renderer ships first; upstream's frame engine replaces it | Resize and tmux parity with upstream's resize tests, without blocking the first release |
| Markdown | Port upstream's streaming processor and profile tokenizer | Linear cost per delta, identical output, and no grammar data in the binary |
| Files and grep | `git ls-files`, `git grep -F`, a `memmem` fallback walk, and upstream's glob matcher | Tool descriptions promise literal matching and git semantics |
| MCP | Hand-written transports on `ofx-jsonrpc` and `ofx-http` behind an `McpTransport` trait | Upstream speaks legacy SSE and several protocol versions, and rmcp changed major versions three times in 2026 |
| Errors | `thiserror` in libraries; `anyhow` only in `ofx-app` and the binary. Upstream error names stay visible to the model | Tool results expose error names upstream already uses |
| Panics | `panic = "unwind"`. The agent contains every panic that unwinds out of tool code. One while preparing, describing, inspecting the file mutation of, or executing a call becomes that call's tool error, with all of `execute` running inside the call's tokio task, and one while dropping a call that never ran is discarded. Only the terminal owner restores the terminal, and only on fatal paths: a panic on the UI or main thread, or process exit | A bug in one tool becomes a tool error instead of ending a work session, and a contained panic never disturbs a live UI |
| Lints | Workspace clippy `pedantic`, `unreachable_pub`, `unwrap_used`, no `unsafe`, no comments | Code explains itself, and dead code stays visible to the compiler |
| Release linking | The x86_64 Linux release is a static-pie musl binary linked with `-z pack-relative-relocs`, set in `.cargo/config.toml`. Rust links the aarch64 musl target as a static non-PIE binary, which applies no relocations at startup | The startup code applies every relocation before `main`, and packed `DT_RELR` entries cost fewer instructions, less file size, and fewer touched pages than `RELA` entries |

## Naming and paths

- Binary `oh-fx`, with an `ofx` symlink installed next to it.
- Environment variables use the `OH_FX_` prefix in place of upstream's `FX_`.
- Settings live in `$XDG_CONFIG_HOME/oh-fx` (default `~/.config/oh-fx`), sessions, prompt history, and credentials in `$XDG_DATA_HOME/oh-fx`, logs and upgrade state in `$XDG_STATE_HOME/oh-fx`, and caches in `$XDG_CACHE_HOME/oh-fx`, on Linux and macOS alike.
- Project configuration is `.oh-fx.json`, and project skills live in `.oh-fx/skills`. The managed skill install root is `$XDG_CONFIG_HOME/oh-fx/skills` (default `~/.config/oh-fx/skills`), beside the global `AGENTS.md`, where upstream uses `~/.fx/skills`; the configuration directory is canonicalized first, as it is for `AGENTS.md`. The other agents' compatibility roots keep upstream's paths.
- Paths that belong to one feature, such as saved sessions, prompt history, the `@` file index cache, and profile MCP servers, are recorded with that feature's [deliberate differences](#deliberate-differences-from-upstream).

## Deliberate differences from upstream

Each file in [`differences/`](differences/) records the deliberate differences from upstream in one area. Record a new difference in the file for its area, next to the bullet it is closest to rather than at the end, so that concurrent changes rarely touch the same lines.

- [Agent turn loop](differences/agent.md): the turn context, tool results and panics, the response language, and prompts submitted during a turn.
- [`oh-fx ask`](differences/ask.md): its output, flags, model lookups, and session start.
- [Logins and credentials](differences/auth.md): ChatGPT and Grok sign-in, credential storage, and refresh.
- [Auto-mode review](differences/auto-mode.md): what auto mode admits on its own and how the reviewer decides.
- [Command line and help](differences/cli.md): commands, top-level help, and commands that are not available yet.
- [Codex](differences/codex.md): the Codex transport and its model catalog.
- [Compaction](differences/compaction.md): manual and automatic compaction and the summary request.
- [Settings files](differences/config.md): reading and writing `settings.json`.
- [Hooks](differences/hooks.md): lifecycle reporting to Herdr and command hooks.
- [Markdown](differences/markdown.md): assistant markdown and syntax highlighting.
- [MCP](differences/mcp.md): server configuration, transports, tools, and notices.
- [Modes](differences/modes.md): the mode registry and what each mode allows.
- [Permissions](differences/permissions.md): admission, session grants, approval requests, and the permission commands.
- [Project instructions](differences/project-instructions.md): `AGENTS.md` files and their notices.
- [Providers](differences/providers.md): custom connections, the chat-completions stream, error details, and TLS.
- [Retries and recovery](differences/recovery.md): model retries, the retry status, and recovery checkpoints.
- [Renderer](differences/renderer.md): resizes, the palette, escaping in rows, and renderer counters.
- [Session store](differences/session-store.md): the on-disk format, saved turns, and the listing index.
- [Sessions](differences/sessions.md): saving, resuming, the session picker, fresh sessions, and titles.
- [Approval and question panels](differences/shell-approvals.md): the interactive approval prompt, its file review, and the question panel.
- [Composer](differences/shell-composer.md): editing, prompt history, `@` mentions, and the clipboard.
- [Terminal input](differences/shell-input.md): keys, pastes, and terminal replies.
- [Terminal ownership](differences/shell-terminal.md): panics, signals, and restoring the terminal.
- [Transcript](differences/shell-transcript.md): turn errors, tool rows, and status text.
- [Skills](differences/skills.md): discovery, loading, and the `skill` tool.
- [Skills in the shell](differences/skills-shell.md): `/skills`, the `$` menu, and bound skills.
- [Slash commands](differences/slash-commands.md): the command list and the commands no other file covers.
- [Model and settings commands](differences/slash-settings.md): the model, Fast mode, settings, and status line commands.
- [Subagents](differences/subagent.md): the subagent host and children in `ask` and the interactive shell.
- [Text](differences/text.md): terminal-safe text, display width, secret masking, and relevance ranking.
- [File tools](differences/tool-files.md): `read_file`, `write_file`, and `edit_file`.
- [Search tools](differences/tool-search.md): `glob_files` and `grep_files`.
- [Shell tool](differences/tool-shell.md): the `shell` tool and its process supervisor.
- [Web tools](differences/tool-web.md): `web_fetch` and `web_search`.
- [Trace log](differences/trace.md): the trace log's variables and paths, and the `/trace` report.
- [Upgrades and releases](differences/upgrade.md): self-upgrade and the release stream.
- [Usage](differences/usage.md): the profile usage ledger and `oh-fx usage`.

## Parity tracking

[Upstream parity](upstream-parity.md) records the upstream commit oh-fx is synced to, where each upstream pull request since the previous sync point lands in oh-fx, and how to run the next parity pass. `parity/files/` maps every upstream Zig source file to its Rust modules, with a per-file status of `todo`, `partial`, `ported`, or `not-applicable` and concrete notes for partial ports and exclusions. `parity/UPSTREAM` pins the checkout used by `cargo xtask parity --upstream <path>` and the upstream parity CI job. The checker fetches nothing, verifies the checkout's commit, rejects missing or stale source entries and invalid Rust module paths, and prints the counts per status. The job feeds the required Repository checks gate. Structural coverage does not establish behavioral parity; each `ported` classification still rests on source and test review or a documented deliberate difference. The goldens in `parity/goldens/` hold fixed upstream bytes that oh-fx ships, and tests compare the shipped values with them byte for byte. `cargo xtask parity goldens --upstream <path>` regenerates them offline from Git objects at `parity/UPSTREAM`, ignoring the upstream working files, and its `--check` mode, which the parity CI job runs, fails when a golden no longer matches the pin. [The golden README](../parity/goldens/README.md) lists what they cover and what remains, their source provenance, and the exact branding substitutions, which retain the product-name difference recorded in [the agent differences](differences/agent.md) while preserving the upstream documentation URL.

## Delivery order

Crates land in the order they are wired into the binary.

1. Tooling, CI, releases, and self-upgrade.
2. A tracer bullet: contracts, provider connections with headers, the chat-completions transport, the agent loop, and `oh-fx ask`.
3. The command spec table replaces the binary's argument parser.
4. File tools, wired into `ask`.
5. The TUI: terminal input, the composer, markdown, and the simple renderer.
6. Permissions with the write tools and approvals, then the shell tool with `ofx-exec`.
7. MCP, skills, subagents, web tools, the Vercel AI Gateway protocol, menus, and the ported renderer.
8. ACP and subscription logins.

## Tool-result reader

Saved tool outputs already live in private `tool-results` sidecars. Storage keeps complete outputs above 8 MiB; replay retains upstream's 8 MiB limit and exact byte-count requirement. A private reader holds the opened regular file and reads raw bounded pages without reopening its path or creating a missing route. Replay consumes this reader. The `read_tool_result` tool, artifact retention, compaction record search and web-fetch artifact integration remain pending.

