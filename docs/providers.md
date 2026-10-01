# Custom provider connections

oh-fx talks to any OpenAI-compatible Chat Completions endpoint through a *custom connection*: a self-hosted gateway such as Portkey or LiteLLM, a local server such as Ollama or llama.cpp, or a hosted router such as OpenRouter. Connections live in your profile settings, never in a project's `.oh-fx.json`.

## Where settings live

| File | Purpose |
| --- | --- |
| `$XDG_CONFIG_HOME/oh-fx/settings.json` (default `~/.config/oh-fx/settings.json`) | Profile settings: `provider`, `model`, `models`, `providers`, `permission_mode`, `yolo_acknowledged`, `max_agent_steps` |
| `<workspace>/.oh-fx.json` | Project settings. Only project-safe keys such as `max_agent_steps` apply; `provider`, `model`, `providers` and other profile keys are ignored with a `config project: ignored_project_user_only_setting` notice |

Settings files are JSON objects of at most 64 KiB. A leading UTF-8 byte order mark is ignored. Duplicate keys are rejected anywhere in the document.

## A Portkey connection

```json
{
  "provider": "portkey",
  "model": "@openai/gpt-4o",
  "providers": {
    "portkey": {
      "protocol": "openai-chat-completions",
      "base_url": "https://portkey.internal.example.com/v1",
      "auth": {"type": "none"},
      "headers": {"x-portkey-api-key": "${PORTKEY_API_KEY}"},
      "models": ["@openai/gpt-4o"]
    }
  }
}
```

```sh
export PORTKEY_API_KEY=...
oh-fx ask "Summarize this repository"
```

oh-fx sends `POST https://portkey.internal.example.com/v1/chat/completions` with `x-portkey-api-key` set from the environment. `auth` is `none` because Portkey authenticates with its own header, so no `Authorization` header is sent. Model ids are opaque: `@openai/gpt-4o` is passed through exactly as written, so Portkey's model catalog slugs work as-is.

### Routing with a saved config

Point requests at a Portkey config with `x-portkey-config`:

```json
"headers": {
  "x-portkey-api-key": "${PORTKEY_API_KEY}",
  "x-portkey-config": "${PORTKEY_CONFIG:-pc-coding-agent}"
}
```

### Provider header with your own provider key

To route through a provider directly and forward that provider's key, set `x-portkey-provider` to the provider's name, send the key as a bearer token, and use the provider's own model ids:

```json
{
  "provider": "portkey",
  "model": "gpt-4o",
  "providers": {
    "portkey": {
      "protocol": "openai-chat-completions",
      "base_url": "https://portkey.internal.example.com/v1",
      "auth": {"type": "bearer", "env": "OPENAI_API_KEY"},
      "headers": {
        "x-portkey-api-key": "${PORTKEY_API_KEY}",
        "x-portkey-provider": "openai"
      },
      "models": ["gpt-4o"]
    }
  }
}
```

A plain provider name such as `openai` takes the key from the `Authorization` header. Model ids prefixed with `@`, as in the first example, select a provider saved in Portkey's Model Catalog instead, so the two styles are not mixed in one connection.

With `auth.type` set to `bearer`, oh-fx owns the `Authorization` header and refuses an `Authorization` entry under `headers`. If you prefer to build that header yourself, keep `auth` as `none` and write `"Authorization": "Bearer ${OPENAI_API_KEY}"` under `headers`.

## Connection fields

| Field | Required | Meaning |
| --- | --- | --- |
| `protocol` | yes | Must be `"openai-chat-completions"`. |
| `base_url` | yes | API prefix; oh-fx appends `/chat/completions`. `https://` or `http://` on any host, including internal names with `_`. One trailing `/` is removed. No credentials, query, or fragment. Redirects are never followed. |
| `auth` | yes | `{"type": "none"}` or `{"type": "bearer", "env": "VAR"}`. A bearer token is always read from the named environment variable, never written in the file, and must be at most 16 KiB of visible ASCII characters. |
| `headers` | no | Extra request headers. Values may use `${VAR}` and `${VAR:-default}`. |
| `tls` | no | `{"ca_file": "/path/to/ca.pem"}`: extra trusted CA certificates in PEM form. Absolute paths or `~/...`. |
| `proxy` | no | `http://` or `https://` proxy URL for this connection only. |
| `models` | no | Model ids this connection offers. The first one is used when nothing else selects a model. |
| `tool_choice_mode` | no | `"omit"` (default) never sends `tool_choice`; `"send"` sends `"auto"` or `"required"` when tools are offered. |
| `max_tokens_parameter` | no | The request field that carries the output limit: `"max_tokens"` (default) or `"max_completion_tokens"`, which OpenAI reasoning models and Azure OpenAI require. |
| `reviewer_model` | no | Model id for the automatic permission reviewer. It is validated now and used once the reviewer lands. |
| `model_metadata` | no | Per-model `context_window`, `max_output_tokens`, `supports_tool_use`, `supports_vision`. `max_output_tokens` becomes the request's output limit and must be smaller than `context_window`; otherwise the connection fails to load with `InvalidModelMetadata`. The `supports_*` flags are validated but not used yet. |

Unknown fields are rejected so that a typo cannot silently change where requests go. Validation errors keep upstream's names, for example `oh-fx: InvalidBaseUrl` or `oh-fx: UnknownField`.

### Header values and environment variables

- `${VAR}` is replaced with the variable's value. An unset, empty, or whitespace-only variable stops the request before anything is sent, with an error that names the header and the variable, never a value:
  `oh-fx ask: header x-portkey-api-key needs the environment variable PORTKEY_API_KEY, which is not set; export it or give a default with ${PORTKEY_API_KEY:-value}`
- `${VAR:-default}` uses `default` when `VAR` is unset, empty, or whitespace-only. Defaults are literal text: a nested `${...}` inside a default is rejected (`InvalidHeaderValue`).
- Any other `$` is literal. Values must be printable ASCII and at most 16 KiB after substitution.
- Values that look like secrets are masked wherever a provider error is shown. For a header whose name contains a word such as `key`, `token`, `secret`, `auth`, `password`, `credential`, or `cookie` (split on `-`, `_`, and `.`, so `x-portkey-api-key` counts and `x-portkey-config` does not), every substituted value and the whole header value are masked. For other headers, substituted values of 8 bytes or more are masked. Bearer tokens and the user name and password of a `proxy` URL are always masked.
- oh-fx owns `accept`, `connection`, `content-length`, `content-type`, `host`, `transfer-encoding`, and `user-agent`, so those names are refused (`ReservedHeader`). `authorization` is refused only when `auth.type` is `bearer`.

## Selecting the provider and model

Provider, highest first:

1. `OH_FX_PROVIDER`
2. `workspaces["<workspace root>"].provider` in `settings.json`
3. top-level `provider`

Model, highest first:

1. `oh-fx ask --model <id>`
2. `OH_FX_MODEL`
3. `models.<provider>`, then `model`, from the workspace entry
4. `models.<provider>`, then `model`, from the top level
5. the first entry of the connection's `models` list

Without any of these, `oh-fx ask` stops with `no model is selected for this connection; save one under "models" in ~/.config/oh-fx/settings.json, or set a model for this run with --model or OH_FX_MODEL`.

## TLS and corporate certificate authorities

oh-fx verifies servers with the operating system's trust store through rustls-platform-verifier.

- `tls.ca_file` adds the certificates in a PEM file to that store for one connection. Use it for gateways behind an internal CA.
- `SSL_CERT_FILE` is honored for every connection. On Linux the platform verifier reads it itself and then trusts only that bundle, as OpenSSL does. On macOS and Windows, where the native verifier ignores it, oh-fx adds its certificates as extra roots.
- A certificate problem is reported as `oh-fx: ConnectionFailed` followed by the underlying cause, for example `oh-fx ask: error sending request for url (...): client error (Connect): invalid peer certificate: UnknownIssuer`.

## Proxies

By default oh-fx follows `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, and `NO_PROXY`. A connection's `proxy` field replaces those settings for that connection only:

```json
"proxy": "http://proxy.corp.example.com:3128"
```

## Plain HTTP

`http://` base URLs are accepted on any host so internal gateways without TLS work. Headers, including API keys, then travel unencrypted, so prefer `https://` whenever the gateway offers it.

## Gateway differences oh-fx tolerates

The stream parser follows upstream fx's rules, with deliberate exceptions for dialects that self-hosted gateways and local servers commonly emit:

- chunks with an empty `choices` array (and empty `id` or `model`), such as Azure's `prompt_filter_results`, at any point in the stream;
- a stream that ends after its `finish_reason` chunk without `data: [DONE]`, and a repeated `finish_reason` chunk without usage;
- usage totals that differ from `prompt_tokens + completion_tokens`;
- provider-native finish reasons such as `end_turn` or `STOP`, and `stop` after tool calls, which are read as `stop` or `tool_calls` depending on whether tool calls arrived;
- tool call deltas without an `index`, with an empty `id` on continuation deltas, or that repeat the function name in every delta;
- finish chunks without a `delta`.

A `finish_reason` of `length` or `content_filter`, tool calls for tools that were not offered, malformed tool arguments, and errors inside the stream (an `error` object, `{"object": "error", ...}`, or a `finish_reason` of `error`) still fail the request. A response whose `Content-Type` is not `text/event-stream` fails with the provider's JSON error when there is one, and otherwise with `oh-fx: UnexpectedContentType` and an excerpt of the body.

## Retries

Like upstream, oh-fx retries a request that fails before any text arrives when the gateway answers 429, 500, 502, 503, or 504, the connection drops, the request times out, or the host cannot be reached. It honors `Retry-After` up to 30 seconds, otherwise backs off from 250 ms to 30 seconds, and gives up after 10 attempts. Each retry prints a notice on stderr, and `ask --json` reports the last one under `recovery`:

```
[notice] ⚠ Rate limited · HTTP 429 · slow down · retrying request in 2s
[notice] ✓ recovered · succeeded on attempt 2
```

Other HTTP errors, TLS failures, and failures after text has streamed are not retried.

## Errors

A failed request prints the error's name and a detail line on stderr, with configured secrets and anything that looks like a credential masked and control characters escaped:

```
oh-fx: InvalidChunk
oh-fx ask: stream event: {not json ...
```

With `--json`, the name goes in the result's `error` field and the detail line still goes to stderr.

An HTTP error from the gateway is reported differently. It prints one `oh-fx ask:` line with the status, the gateway's code and its message, and a 401 adds the gateway's own explanation on a second line. With `--json`, the result has no `error` field. `exit_code` is `1`, the message line is in `output`, and a 401 also sets `auth_failure`.

## Troubleshooting

| Message | Cause |
| --- | --- |
| `the gateway provider is not available in oh-fx yet; ...` | No `provider` is selected. Add a connection and select it. |
| `oh-fx: UnknownConfiguredProvider` | `provider` or `OH_FX_PROVIDER` names a connection that is not under `providers`. |
| `The configured provider credential is unavailable. ...` | `auth.type` is `bearer` and its environment variable is unset or blank. |
| `the configured provider credential is not a valid bearer token; ...` | The bearer token is longer than 16 KiB or contains spaces, control characters, or non-ASCII characters. With `--json`, `error` is `InvalidConfiguredProviderCredential`. |
| `configured provider authentication failed · HTTP 401` | The gateway rejected the key. Check the key variable and the header name. |
| `API request failed · HTTP 4xx/5xx · ...` | The gateway answered with an error; the message shows its code and text, with secrets masked. |
| `HTTP 302: redirect to https://sso.example.com was not followed; ...` | `base_url` points at a sign-in page or a proxy that redirects. Use the gateway's API URL. |
| `oh-fx: ConnectionFailed` | DNS, connection, proxy, or TLS failure; the next line says which. |
| `oh-fx: UnexpectedContentType` | The gateway answered with something other than an event stream, often an HTML page from a proxy. |
| `oh-fx: InvalidProfileConfiguration` | `settings.json` is malformed or larger than 64 KiB, has duplicate keys, or holds a setting with the wrong type or value. As upstream, the message does not say which. |
