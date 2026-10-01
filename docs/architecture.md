# Architecture

oh-fx is a behavior-faithful Rust port of [vercel-labs/fx](https://github.com/vercel-labs/fx). Upstream behavior is the specification: tool schemas, prompts, help text, and terminal output match upstream unless this document records a deliberate difference.

## Principles

- **Port leaf behavior, not upstream coupling.** Escape parsing, markdown, syntax highlighting, glob matching, git-backed file listing, the frame renderer, and MCP transports are ported from upstream, and their upstream tests become ours. Upstream's large shared structs (`DispatchContext`, `AgentRuntimeDeps`) are replaced by narrow traits and constructor injection.
- **Contracts first.** The agent and the TUI depend only on `ofx-contract`, so they can be built and tested with fakes while transports and tools land in parallel.
- **Module names follow upstream files.** A Rust module that ports `src/core/tooling/tool_dispatch.zig` is named `tool_dispatch.rs`, which keeps the parity ledger mechanical.
- **Every boundary is a compile-time fact.** Crates are layered and acyclic. The UI cannot reach sessions or config, and transports cannot reach product state.

## Crates

| Layer | Crate | Owns | Upstream |
|---|---|---|---|
| 0 | `ofx-contract` | Ids, messages, model requests and stream events, tool traits and metadata, tool result errors, permission modes and decisions, reasoning effort, UI events and commands | `core/shared/types`, `agent/stream_provider`, `core/tooling` contracts, UI contracts |
| 0 | `ofx-text` | Display width, graphemes, terminal-safe text, token estimates, lexical relevance | `core/shared/*` text utilities |
| 1 | `ofx-config` | XDG profile paths, settings layers, provider ids, context limits, custom provider connections | `core/config`, `shared/profile_paths` |
| 1 | `ofx-http` | HTTP client, TLS trust, proxy, SSE decoding | `shared/http_pool`, `gateway/sse` |
| 1 | `ofx-shell` | Shell command lexing, classification, and effects | `core/shell_command` |
| 1 | `ofx-markdown` | Streaming markdown, syntax highlighting, diffs | `agent/presentation`, `core/output` |
| 1 | `ofx-vt` | Terminal screen model used by the renderer and tests | `core/terminal/engine` |
| 1 | `ofx-jsonrpc` | JSON-RPC framing shared by MCP and ACP | `acp/jsonrpc`, MCP framing |
| 2 | `ofx-workspace` | Path resolution, git file listing, glob, literal grep, read tracking | `core/workspace` |
| 2 | `ofx-exec` | Processes, process groups, managed shell sessions, PTYs | `core/execution`, `core/terminal` |
| 2 | `ofx-upgrade` | Self-upgrade from GitHub Releases | `core/upgrade` |
| 3 | `ofx-gateway` | Vercel AI Gateway and OpenAI-compatible transports, model catalog | `src/gateway`, `core/gateway` |
| 3 | `ofx-session` | Session ids, storage, replay, result store, prompt history, usage | `core/session` |
| 3 | `ofx-permissions` | Rules, command admission, grants | `core/permissions` |
| 3 | `ofx-auth` | Credential storage, login providers, and login flows | `core/auth` |
| 3 | `ofx-skills` | Skill discovery, frontmatter, catalog | `core/skills` |
| 4 | `ofx-cli` | Command spec table, argument parsing, help rendering | `core/cli`, `core/slash_commands` |
| 4 | `ofx-tools` | Built-in tools, one module per upstream tool | `src/tools`, `builtins/tools` |
| 4 | `ofx-mcp` | MCP client: stdio, streamable HTTP, legacy SSE, OAuth | `core/mcp` |
| 4 | `ofx-agent` | Turn loop, tool batches, compaction, steering, subagent domain | `core/agent`, `core/subagent` |
| 4 | `ofx-tui` | Event loop, input, composer, footer, transcript, frame rendering | `src/ui`, `core/input` |
| 5 | `ofx-app` | Composition, controller, command handlers, hosts | `core/app`, `src/builtins` |
| 5 | `ofx-acp` | Agent Client Protocol server | `src/acp` |
| 6 | `oh-fx` | Binary entry point and fast paths | `src/main.zig` |
| dev | `ofx-testkit` | Fake provider and MCP servers, PTY driver, TLS and proxy fixtures | test harnesses |

Every crate may depend on `ofx-contract` and `ofx-text`. Otherwise a crate depends only on crates in lower layers. `ofx-agent` and `ofx-tui` depend on nothing but layer 0 crates and `ofx-markdown`/`ofx-vt` for the TUI.

## Type ownership

- A type lives in the crate that ports the upstream file defining it. `core/shared/types` and the tool dispatch contract map to `ofx-contract`, so `PermissionMode`, `ReasoningEffort`, `ApprovalPolicy`, tool metadata, and tool result errors live there. `ProviderId` and context limits live in `ofx-config`, session ids in `ofx-session`, and shell command analysis in `ofx-shell`.
- A crate that needs another crate's type depends on its owner and never keeps a copy. `ofx-cli` validates provider ids, session ids, and login providers by calling their owners while parsing, as upstream does.
- Public items need a consumer outside their crate. A crate lands with its first consumer, or directly before it in the same stack of pull requests. A type moves to its owner in the pull request that adds its first outside consumer.

## Key decisions

| Concern | Decision | Reason |
|---|---|---|
| Async | tokio for the agent, transports, MCP, and processes; no runtime on fast paths such as `--version` | Cancellation of in-flight streams and process trees |
| UI thread | A plain OS thread polling the tty, a wake pipe, and a signal pipe | Rendering never competes with tool tasks, and it mirrors upstream's event loop |
| Agent and UI contract | `UiEvent` and `UiCommand` data, which gain serialization with their first wire consumer (ACP or a JSON event stream). Approvals and questions are correlated by `RequestId` through a map of pending oneshots | One event stream drives the TUI, `ask --json`, ACP, and test fixtures |
| HTTP and TLS | reqwest with rustls, the ring provider, and rustls-platform-verifier with extra roots from `SSL_CERT_FILE` or a connection's `tls.ca_file` | Corporate CAs and proxies work, and static musl builds need no cmake |
| CLI | A static command spec table ported from upstream, scanned the way upstream scans arguments: no flag clustering, and values may start with `-` | One source for `--help`, `/help`, and the slash menus, with upstream's exact acceptance rules |
| Terminal | rustix plus a port of upstream's escape parser | Upstream needs OSC 11, DSR, and DA1 replies that crossterm drops |
| Renderer | A `FrameSink` seam. A simple live-region renderer ships first; upstream's frame engine replaces it | Resize and tmux parity with upstream's resize tests, without blocking the first release |
| Markdown | Port upstream's streaming processor and profile tokenizer | Linear cost per delta, identical output, and no grammar data in the binary |
| Files and grep | `git ls-files`, `git grep -F`, a `memmem` fallback walk, and upstream's glob matcher | Tool descriptions promise literal matching and git semantics |
| MCP | Hand-written transports on `ofx-jsonrpc` and `ofx-http` behind an `McpTransport` trait | Upstream speaks legacy SSE and several protocol versions, and rmcp changed major versions three times in 2026 |
| Errors | `thiserror` in libraries; `anyhow` only in `ofx-app` and the binary. Upstream error names stay visible to the model | Tool results expose error names upstream already uses |
| Panics | `panic = "unwind"`. Each tool call runs as a tokio task, and the agent maps a panicked task's `JoinError` to a tool error. Only the terminal owner restores the terminal, and only on fatal paths: a panic on the UI or main thread, or process exit | A bug in one tool becomes a tool error instead of ending a work session, and a contained panic never disturbs a live UI |
| Lints | Workspace clippy `pedantic`, `unreachable_pub`, `unwrap_used`, no `unsafe`, no comments | Code explains itself, and dead code stays visible to the compiler |

## Naming and paths

- Binary `oh-fx`, with an `ofx` symlink installed next to it.
- Environment variables use the `OH_FX_` prefix in place of upstream's `FX_`.
- Settings live in `$XDG_CONFIG_HOME/oh-fx` (default `~/.config/oh-fx`), sessions and credentials in `$XDG_DATA_HOME/oh-fx`, logs and upgrade state in `$XDG_STATE_HOME/oh-fx`, and caches in `$XDG_CACHE_HOME/oh-fx`, on Linux and macOS alike.
- Project configuration is `.oh-fx.json`, and project skills live in `.oh-fx/skills`.

## Deliberate differences from upstream

- Custom provider connections accept `headers` with `${VAR}` interpolation, `tls.ca_file`, `proxy`, a `models` list, and plain `http://` base URLs on any host, so self-hosted gateways such as Portkey work. The chat-completions stream parser tolerates the empty-choices chunks and missing `[DONE]` markers that gateways emit.
- There is one release stream, so `oh-fx upgrade` has no `--channel` option.
- Releases come from GitHub Releases. Every push to `main` that changes more than documentation publishes `v<version>-dev.<n>`, where `<n>` is the commit count on `main`.
- Upgrade checks run from every command through a detached background process, at most once every five minutes when commands run, instead of upstream's thirty, so each merge reaches users quickly.

## Parity tracking

`parity/` will record the pinned upstream commit and map every upstream source file to its Rust module, with a status of `todo`, `partial`, `ported`, or `not-applicable`. CI will fail when an upstream file has no entry. Goldens dumped from upstream (tool schemas, help text, prompts) will be compared byte for byte.

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
