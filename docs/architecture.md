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

- Custom provider connections accept `headers` with `${VAR}` interpolation, `tls.ca_file`, `proxy`, a `models` list, `max_tokens_parameter` (`max_completion_tokens` for reasoning models and Azure OpenAI), plain `http://` base URLs on any host, and host names with `_`, so self-hosted gateways such as Portkey work. Settings files may start with a UTF-8 byte order mark.
- The chat-completions stream parser tolerates gateway dialects that upstream rejects: empty-choices chunks, a stream that ends after its finish chunk without `[DONE]`, repeated finish chunks, mismatched usage totals, provider-native finish reasons, `stop` after tool calls, tool call deltas without an `index` or with empty ids or repeated names, and finish chunks without a `delta`. A repeated name that could also extend to another advertised tool fails as `InvalidToolName`, where upstream appends it. `{"object": "error"}` chunks and a `finish_reason` of `error` fail as provider errors, and a response that is not `text/event-stream` fails with its JSON error or `UnexpectedContentType` instead of a parse error.
- Provider connections never follow redirects. A 3xx answer names the redirect target's host and explains that `base_url` must point at the gateway API, so a sign-in page never receives the request or its headers.
- An HTTP 404 keeps its real status in the error line, and protocol, transport, and stream failures print a second `oh-fx ask:` line with a masked, terminal-safe detail or excerpt; a 401 adds the gateway's own explanation. Upstream prints only the error name.
- Configured secrets are masked in every provider detail, including their JSON-escaped and percent-encoded forms: the bearer token, proxy credentials, every substituted value of a header whose name looks sensitive, and other substituted values of at least 8 bytes. A bearer token longer than 16 KiB or with non-visible characters fails as `InvalidConfiguredProviderCredential` before any request. Whitespace-only header variables count as missing, and nested defaults are rejected.
- Model retries stop after 10 attempts instead of continuing indefinitely, never start after text has streamed, and do not retry TLS or proxy tunnel failures.
- `oh-fx ask ""` fails with `InvalidConversationEvent` before any request is sent.
- The turn context reads git facts from the repository that encloses the workspace, where upstream reads only `<workspace>/.git`, and reports a tracked file as dirty only when it is missing or its size differs from the index, never from its modification time alone.
- A streamed write to a closed or failing stdout cancels the request and ends `ask` with the failed write's name, such as `BrokenPipe` or `NoSpaceLeft`.
- Until sessions land, a tool result over 64 KiB is cut at upstream's `--no-save` truncation marker instead of being stored behind a `read_tool_result` handle, and `read_file` reports image files as binary content until tool image attachments land.
- Until the TUI lands, terminal-mode `ask` prints tool progress lines and blocked-action labels on stderr, as the raw, quiet, and JSON modes do, but escapes their control characters with terminal-safe encoding.
- In the ask and auto permission modes, whether set in settings or for one request with `--auto`, a `read_file` call whose path, or a `glob_files` or `grep_files` call whose search root, resolves outside the workspace needs approval. Upstream reads and searches such paths without approval unless a configured `permission` rule asks for it. Until approval prompts, permission rules, and the auto-mode reviewer are ported, nothing can grant that approval, so `ask` handles the call as upstream does when no approval prompt is available: it prints the blocked-action guidance with upstream's ask-mode or auto-mode hint (which still name `--auto` and the interactive shell), records the call as an error, and fails with `NonInteractivePermissionRequired`. Full access, from settings, `--full-access`, or `--yolo`, reads and searches external paths. A call admitted only for the workspace resolves its path again when it runs and fails with `PathOutsideWorkspace` if the path has left the workspace since admission.
- `read_file` opens the resolved path without following a symlink in any component, not only the last one as upstream does. It walks from `/` with `O_NOFOLLOW | O_DIRECTORY` descriptors, opened with `O_PATH` on Linux so directories that grant only search permission still work; on macOS each directory on the path must also be readable. A parent directory swapped for a symlink after the path was resolved fails as `NotRegularFile` instead of exposing the symlink target's content.
- `read_file` clips a line longer than its 2,000-byte cap after the last whole character that fits. Upstream cuts at the byte cap, which can split a multibyte character and replace the whole result with the binary-output notice.
- `ask --json` lists the tool calls of a parallel group in call order. Upstream lists the calls that ran in the order their threads finish, then the calls that failed before running, such as on invalid arguments or a missing path.
- Until sessions and the settings writer land, the full-access warning prints on every run unless `yolo_acknowledged` is set by hand, and `ask --json` reports recovery as `"durable": false`.
- Secret masking runs upstream's matchers unchanged and a hardened pass, and masks the union of both: credentials in any `scheme://user:password@host` (cut at the last `@`), sensitive keys (api keys, tokens, secrets, passwords, credentials, cookies, auth headers, Portkey virtual keys) in `key=value`, `key: value`, JSON string members, and `--key value` forms, case-insensitive `Bearer`/`Basic`/`Token` values with the full base64 alphabet, and JWTs whose base64url header decodes to a JSON object. Output is byte-identical to upstream when the hardened pass finds nothing.
- Terminal-safe encoding escapes every Unicode default-ignorable code point except the emoji presentation selectors U+FE0E and U+FE0F, so soft hyphens, the Arabic letter mark, Hangul fillers, and tag characters cannot hide text. Upstream escapes only C1 controls, zero-width characters, line separators, bidi embeddings, and the byte order mark.
- ANSI-aware width measurement and wrap cuts treat DCS, SOS, PM, and APC strings (`ESC P`, `ESC X`, `ESC ^`, `ESC _`) as zero width through their string terminator, so a cut never splits one and leaves the terminal inside it. Upstream treats them as two-byte escapes.
- A file tool `path` that contains a NUL byte fails with `Cannot resolve path "a.txt\u0000x": InvalidPath`, which shows each NUL as `\u0000`, and a glob pattern whose static base contains one fails with `Unable to resolve glob search root: <path> (InvalidPath)`. Upstream cuts both at the NUL, so `a.txt\u0000x` reads `a.txt` there.
- Git commands behind `glob_files` and `grep_files` run with `--no-pager -c core.fsmonitor=false -c core.hooksPath=/dev/null`, `GIT_NO_LAZY_FETCH=1`, and an empty `GIT_ALLOW_PROTOCOL`, so a repository's config cannot make a search run its fsmonitor hook, a git hook, a pager, or a lazy fetch from a promisor remote through `remote.<name>.uploadpack` or `core.sshCommand`. Upstream runs them with the repository's config in effect.
- `git grep` runs with `-c color.grep=never -c grep.column=false -c grep.fullName=false`, so settings such as `color.ui=always`, `grep.column`, and `grep.fullName` cannot change the output `grep_files` parses. Upstream drops tracked matches under those settings, and its scoped searches find nothing under `grep.fullName`.
- `grep_files` canonicalizes every candidate file before reading it and skips one that resolves outside both the workspace and the search root, so a tracked file reached through a symlinked directory cannot expose content from elsewhere. Upstream checks containment only when the file itself is a symlink.
- `grep_files` opens each resolved candidate with `read_file`'s opener, which follows no symlink in any component and opens with `O_NONBLOCK`, and reads it only when the opened file is regular, so a directory swapped for a symlink after the candidate was checked cannot expose outside content, and a symlink to a FIFO is skipped instead of blocking the search forever as it does upstream.
- `glob_files` and `grep_files` use git only when `git rev-parse --show-toplevel` names the search root or one of its ancestors, so a repository whose `core.worktree` or `.git` file puts its work tree elsewhere, or whose `core.bare` leaves it without one, cannot make a search list or read files from outside the search root under the search root's names. Those roots use the directory walk and in-process scan, as outside a repository. Upstream searches whatever work tree git reports.
- When `glob_files` and `grep_files` walk the directory tree instead of using git, an entry whose directory listing reports no type, as on filesystems without `d_type`, is checked with `fstatat` and listed or descended like any other file or directory. Upstream skips such entries. Each subdirectory is opened relative to its parent without following symlinks, so a directory swapped for a symlink after it was listed is skipped where upstream follows it.
- A `glob_files` pattern with a directory part matches only at the depth it spells out: `src/*.rs` and `src/main.rs` match files directly in `src`, and `src/**/*.rs` matches at any depth below it. Upstream matches what follows the directory against file names at any depth, so `src/*.rs` also lists `src/nested/other.rs`.
- `grep_files` passes `include` to `git grep` as a pathspec only when it starts with `*` and contains no `/`, `\`, `[`, `{`, or `}`, so git can only widen the match, and otherwise lets git search every tracked file and filters with its own matcher. Upstream passes every other slash-free include as well, so git reads `[ab]` as a character class and matches a pattern that does not start with `*` only at the search root: a tracked `foo[ab].txt` is missed for `*[ab].txt`, and a tracked `src/main.rs` for `main.rs`.
- Git commands behind the file tools clear `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR`, `GIT_OBJECT_DIRECTORY`, and `GIT_ALTERNATE_OBJECT_DIRECTORIES`, so git finds the repository from the search root alone. Upstream passes them through from its environment.
- The assistant intro filter matches "oh-fx" instead of "fx".
- There is one release stream, so `oh-fx upgrade` has no `--channel` option.
- The top-level help links to the oh-fx repository instead of fx.sh/docs.
- Top-level help narrower than its two-column layout needs (under 54 columns for commands and flags) puts each usage on its own line with the summary indented below, and stacks the resource links when their longest word does not fit beside the labels, so only unbreakable words overflow. Upstream keeps the two-column layout at every width.
- An unknown command is echoed through terminal-safe encoding capped at 160 bytes, so escape sequences, other control characters, and invalid UTF-8 print as escapes such as `\x1b` and `\xff`. Upstream writes the raw bytes to the terminal.
- A closed stdout cannot be detected: the Rust runtime reopens closed standard streams on `/dev/null` before `main` runs, so output is discarded where upstream reports `WriteFailed` or `NotOpenForWriting`. Full disks and closed pipes fail as upstream does.
- Until their features land, commands and flags that oh-fx parses but cannot run fail with `oh-fx: <command> is not available yet` and exit status 1, where upstream runs them. Arguments are validated first, so malformed ones still fail with upstream's usage errors. With `--json`, such a command also prints `{"kind":"<command>","error":"<command> is not available yet","code":"NotAvailableYet"}` on stdout, and `ask --json` prints its JSON result with the error `NotAvailableYet`. This covers the interactive session (`oh-fx` with no arguments, `-c`, `-r`, `--resume`, and `resume`), every command except `ask`, `upgrade`, `help`, `--version`, `login codex`, and `logout codex` (so `login` and `logout` for Vercel and Grok are covered), the `--context-limit` and `--add-dir` launch modifiers, since `ask` has no skills, MCP servers, project instructions, or extra workspace roots for them to apply to, and the `--sessions-v2` launch modifier on `ask`. Commands that keep no sessions upstream, such as `upgrade`, `login codex`, `logout codex`, `help`, and `--version`, accept and ignore `--sessions-v2` as upstream does; the commands that use sessions upstream, the interactive session, and `acp` fail as not available yet with or without it.
- `oh-fx ask` honors `--model`, `--auto`, `--full-access`, `--yolo`, `--system`, `--json`, `--quiet`, and `--no-color`. It fails as not available yet on `--image`, `--prompt-permissions`, `--timeout` with a valid number of seconds, `--resume`, `--resume-id`, and `--continue-recovery`, because image attachments, approval prompts, shell command timeouts, and sessions do not exist yet. It also fails as not available yet when `--sessions-v2`, given before or after `ask`, or `OH_FX_SESSIONS_V2` set to `1` or to `true` in any letter case selects the v2 session store, unless `--no-save` is given. Without them, `ask` runs and saves nothing, since upstream's default store is not ported either; the flag and the variable explicitly ask for the v2 store, whose sessions upstream resumes only with the flag, so oh-fx reports that it cannot honor the request instead of quietly saving nothing. Upstream's `--no-save` writes nothing to either store, so with it the request runs. It validates `--effort`, `--fast`, `--no-fast`, `--provider-order`, `--provider-strict`, and `--no-provider-strict` as upstream does and then ignores them, as upstream does for custom provider connections, which never advertise reasoning, Fast mode, or gateway routing. `--no-additional-dirs` and `--no-save` are accepted, since ask saves no sessions yet, and `--verbose` is accepted and ignored, as upstream does.
- `ask --system` must be valid UTF-8 and otherwise fails with `InvalidAskArgs` before any request. Upstream sends the invalid bytes as a JSON byte array, which OpenAI-compatible gateways reject.
- Releases come from GitHub Releases. Every push to `main` that changes more than documentation publishes `v<version>-dev.<n>`, where `<n>` is the commit count on `main`.
- Upgrade checks run from every command through a detached background process, at most once every five minutes when commands run, instead of upstream's thirty, so each merge reaches users quickly.
- Assistant markdown escapes terminal controls once, as text enters a span: C0 controls other than tab, DEL, C1 controls, line and paragraph separators, and bidi embedding, override and isolate controls appear as `\xNN` or `\u{NNNN}`, and table widths and hanging indents measure that escaped text. Link targets and numeric entities stay literal text unless every code point is one the terminal-safe encoder keeps. Upstream passes these code points to the terminal.
- Pipe tables take their column count from the header row and drop extra cells, as GitHub-flavoured Markdown does, and a table that would render more than 16 cells per source byte stays plain lines. Upstream widens to the longest row and pads every row, so 32 KiB of pipes rendered hundreds of megabytes.
- `MarkdownProcessor::push` takes `&str`, so callers replace invalid UTF-8 before markdown sees it. Upstream processes raw bytes.
- A heading with no text at the end of a stream produces no line. Upstream writes only its style codes.
- A level-six heading whose whole text is `• `, `☐ ` or `[1] ` has no hanging indent. Upstream's wrap parser reads its dim text as a list, task or footnote marker; the line is too short to wrap either way.
- `flush` never changes events an earlier `push` returned, and the footnote separator counts blank lines already emitted, as upstream's assistant stream does. Upstream's processor trims trailing blank lines from a buffer it shares with earlier pushes.
- The syntax highlighter never splits a multi-byte character. Upstream can cut one after a backslash inside a string, which shows replacement characters.
- The ChatGPT sign-in redirects the browser to `http://127.0.0.1:<port>/auth/callback`, the address its callback listener binds and the one OpenAI's Codex CLI uses, where upstream uses `localhost`, which some browsers resolve to `::1` first. The code exchange sends the same redirect URI. A token response without `expires_in` takes the session's expiry from the access token's `exp` claim, as refreshes already do, where upstream fails the sign-in; an `expires_in` that is present but not a positive integer still fails it.
- oh-fx names itself to OpenAI's sign-in with `originator=oh-fx`, matching its `oh-fx/<version>` user agent, where upstream sends `fx`.

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
