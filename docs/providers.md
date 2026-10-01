# Custom provider connections

oh-fx talks to any OpenAI-compatible Chat Completions endpoint through a *custom connection*: a self-hosted gateway such as Portkey or LiteLLM, a local server such as Ollama or llama.cpp, or a hosted router such as OpenRouter. Connections live in your profile settings, never in a project's `.oh-fx.json`. To use a ChatGPT Plus or Pro subscription instead, see [ChatGPT subscription](#chatgpt-subscription).

## Where settings live

| File | Purpose |
| --- | --- |
| `$XDG_CONFIG_HOME/oh-fx/settings.json` (default `~/.config/oh-fx/settings.json`) | Profile settings: `provider`, `model`, `models`, `codex_model`, `providers`, `permission_mode`, `yolo_acknowledged`, `max_agent_steps` |
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

A `provider` of `codex` selects a ChatGPT subscription instead of a connection; see [ChatGPT subscription](#chatgpt-subscription).

## TLS and corporate certificate authorities

On Linux, oh-fx trusts the roots in the system's CA bundle, as upstream fx does. It reads them only when a TLS handshake first needs them, so a plain `http://` connection never opens a certificate file.

- With neither `SSL_CERT_FILE` nor `SSL_CERT_DIR` set, oh-fx reads the first bundle it finds among `/etc/ssl/certs/ca-certificates.crt` (Debian, Ubuntu, Gentoo), `/etc/pki/tls/certs/ca-bundle.crt` (Fedora, RHEL), `/etc/ssl/ca-bundle.pem` (openSUSE), `/etc/pki/tls/cacert.pem` (OpenELEC), `/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem` (CentOS, RHEL 7), `/etc/ssl/cert.pem` (Alpine), `/opt/etc/ssl/certs/ca-certificates.crt` (Entware), and `/etc/ssl/certs/cacert.pem` (OpenHarmony). Without any of them, it reads every certificate in `/etc/ssl/certs`, `/etc/pki/tls/certs`, and `/etc/security/certificates` instead.
- When a server's certificate fails verification against the bundle for any reason other than a name mismatch, oh-fx reads those certificate directories once and verifies the certificate again against the bundle and the directories together. A CA copied into `/etc/ssl/certs` without regenerating the bundle (with `update-ca-certificates` or `update-ca-trust`) is therefore still trusted, including a renewed CA that replaces a bundled one with the same name, and only a failed verification pays for the scan.
- `SSL_CERT_FILE` and `SSL_CERT_DIR` (a colon-separated list of directories) replace the system bundle, as they do for OpenSSL: when either is set, oh-fx trusts only the certificates they name, and the directory fallback does not apply.
- `tls.ca_file` adds the certificates in a PEM file to those roots for one connection. Use it for gateways behind an internal CA. The file is read and checked when the connection is set up, so a missing or malformed file fails before any request.
- The roots are read once per run. If they cannot be read or hold no usable certificate, and the connection's `tls.ca_file` does not vouch for the server, the request fails with `oh-fx: CertificateBundleLoadFailure` and a second line that names the file, for example `oh-fx ask: no CA certificates found at /etc/ssl/custom.pem`.

On macOS and Windows, oh-fx verifies servers with the operating system's trust store through rustls-platform-verifier. `tls.ca_file` and `SSL_CERT_FILE` add their certificates to that store, since the native verifiers ignore `SSL_CERT_FILE`.

Other certificate problems are reported as `oh-fx: ConnectionFailed` followed by the underlying cause, for example `oh-fx ask: error sending request for url (...): client error (Connect): invalid peer certificate: UnknownIssuer`.

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

A `finish_reason` of `length` or `content_filter`, tool calls for tools that were not offered, malformed tool arguments, and errors inside the stream (an `error` object, `{"object": "error", ...}`, or a `finish_reason` of `error`) still fail the request. A response whose `Content-Type` is not `text/event-stream` fails with the provider's JSON error when there is one, and otherwise with `oh-fx: UnexpectedContentType`, the media type it sent, and the body's size.

## Retries

Like upstream, oh-fx retries a request that fails before any text arrives when the gateway answers 429, 500, 502, 503, or 504, the connection drops, the request times out, or the host cannot be reached. It honors `Retry-After` up to 30 seconds, otherwise backs off from 250 ms to 30 seconds, and gives up after 10 attempts. Each retry prints a notice, and `ask --json` reports the last one under `recovery`. The notice goes to stderr, or to stdout with the answer when stdout is a terminal and neither `--json` nor `--quiet` is given:

```
[notice] ⚠ Rate limited · HTTP 429 · slow down · retrying request in 2s
[notice] ✓ recovered · succeeded on attempt 2
```

Other HTTP errors, TLS failures, and failures after text has streamed are not retried.

## Errors

A failed request prints the error's name and a detail line on stderr. The detail describes rejected stream data by its position and size and never repeats it. Error messages from the provider are shown with configured secrets and anything that looks like a credential masked and control characters escaped:

```
oh-fx: InvalidChunk
oh-fx ask: stream event 2 (28 bytes) was rejected
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
| `oh-fx: CertificateBundleLoadFailure` | The trusted root certificates could not be read; the next line names the file. See [TLS and corporate certificate authorities](#tls-and-corporate-certificate-authorities). |
| `oh-fx: UnexpectedContentType` | The gateway answered with something other than an event stream, often an HTML page from a proxy. |
| `oh-fx: InvalidProfileConfiguration` | `settings.json` is malformed or larger than 64 KiB, has duplicate keys, or holds a setting with the wrong type or value. As upstream, the message does not say which. |

## ChatGPT subscription

oh-fx can send requests through a ChatGPT Plus or Pro subscription, using the same Codex sign-in and Responses endpoint as upstream fx. Requests count against the subscription's Codex usage limits.

### Sign in

```sh
oh-fx login codex
```

oh-fx prints the sign-in URL, opens it in your browser (`open` on macOS, `xdg-open` elsewhere), and waits up to five minutes:

```
Open this URL to sign in with Codex:
https://auth.openai.com/oauth/authorize?response_type=code&client_id=...

Waiting for browser authorization...
```

After you approve, the browser returns to `http://127.0.0.1:1455/auth/callback` (or port 1457 when 1455 is busy), shows "Authorization complete", and the terminal prints `Signed in with Codex.` The callback listener binds `127.0.0.1` only, accepts exactly one authorization code, rejects any callback whose `state` does not match, and answers every other path with 404.

Set `OH_FX_NO_OPEN_BROWSER=1` to print the URL without opening a browser.

When `OH_FX_AUTH_MODE` is `host-managed`, `oh-fx login` and `oh-fx logout` change nothing and print `Authentication is managed by the host.`

### Signing in on a remote machine

The browser must reach the callback port on the machine where oh-fx runs. Over SSH, forward the port and open the printed URL in your local browser:

```sh
ssh -L 1455:127.0.0.1:1455 you@remote-host
OH_FX_NO_OPEN_BROWSER=1 oh-fx login codex
```

The `redirect_uri` in the printed URL names the port oh-fx is listening on; forward `1457` instead when it says so.

Without port forwarding, approve the sign-in in your local browser and let its redirect fail. Copy the full `http://127.0.0.1:1455/auth/callback?code=...&state=...` address from the address bar, and within the five minutes run this in a second shell on the remote machine:

```sh
curl 'http://127.0.0.1:1455/auth/callback?code=...&state=...'
```

### Select ChatGPT and a model

Save the provider and a Codex model in `~/.config/oh-fx/settings.json`:

```json
{
  "provider": "codex",
  "models": {"codex": "gpt-6.1-sol"}
}
```

```sh
oh-fx ask "Summarize this repository"
```

Or choose them for one run:

```sh
OH_FX_PROVIDER=codex oh-fx ask --model gpt-6.1-sol "Summarize this repository"
```

Model ids are the slugs from the Codex model list that ChatGPT offers your plan, such as `gpt-6.1-sol`, and are sent as written. The list changes as OpenAI adds and retires models, and a slug it no longer offers fails with HTTP 400. The model is chosen from, highest first:

1. `oh-fx ask --model <id>`
2. `OH_FX_MODEL`
3. `models.codex`, then the legacy `codex_model`, from the workspace entry
4. `models.codex`, then `codex_model`, from the top level

The `model` key belongs to the gateway and is never used for Codex. Without a Codex model, `oh-fx ask` stops with `no Codex model is selected; save one as "codex" under "models" in ~/.config/oh-fx/settings.json, or set a model for this run with --model or OH_FX_MODEL`.

### What is sent where

- Requests go to `https://chatgpt.com/backend-api/codex/responses` with the access token, the ChatGPT account id, `originator: oh-fx`, and `OpenAI-Beta: responses=experimental`. The body uses `store: false` and asks for encrypted reasoning, which oh-fx replays on the next step of the same run.
- The sign-in code and refresh token go only to `https://auth.openai.com/oauth/token`.
- Redirects are never followed, and tokens are masked in every error and never printed.
- oh-fx refreshes the access token a minute before it expires and saves the new tokens before the request. When ChatGPT answers 401, oh-fx refreshes once and resends the request.
- Ctrl-C or SIGTERM during a refresh starts no new one. oh-fx waits up to 2 seconds for a refresh already sent, so the new tokens are saved, then exits. If `auth.openai.com` has not answered by then, the next run may ask you to sign in again.

### Sign out

```sh
oh-fx logout codex
```

This prints `Signed out of Codex.`, or `No Codex login session found.` when there was nothing to remove.

### Where the login is stored

The login lives in `$XDG_DATA_HOME/oh-fx/chatgpt-auth.json` (default `~/.local/share/oh-fx/chatgpt-auth.json`), next to a `chatgpt-auth.lock` file that keeps concurrent oh-fx processes from refreshing at the same time. The directory is created with mode `0700` and the file with mode `0600`, and every update is written to a temporary file, synced, and renamed into place. oh-fx refuses a credential file that is a symbolic link, has more than one hard link, or is readable or writable by group or others.

### Errors

| Message | Cause |
| --- | --- |
| `oh-fx needs a Codex subscription login for this model. Run oh-fx login codex.` | No login is saved, or the refresh token expired or was revoked, in which case oh-fx removes the saved login. With `--json`, `error` is `MissingCredentials`. |
| `Codex subscription authentication failed · HTTP 401`, then `Run oh-fx login codex to sign in again.` | ChatGPT rejected the token even after a refresh. With `--json`, `auth_failure.source` is `Codex subscription`. |
| `[notice] ⚠ Rate limited · HTTP 429 · usage_limit_reached: ...` | The subscription's usage limit was reached. oh-fx waits for `Retry-After`, up to 30 seconds, and retries as described in [Retries](#retries). |
| `Saved credential storage is unavailable. Check the saved credential, then retry.` | The credential file or its directory is unsafe or unreadable. Run `chmod 600` on the file, or sign out and sign in again. |
| `Credential refresh is temporarily unavailable. Retry shortly.` | The token refresh could not reach `auth.openai.com` or failed temporarily. |
| `Credential could not be saved. Check authentication storage before signing in again.` | The refreshed tokens could not be saved durably. |
| `The credential account or team changed. Review authentication before retrying.` | A refresh returned a token for a different ChatGPT account. |
| `oh-fx login: authorization denied` | The sign-in was denied in the browser. |
| `oh-fx login: failed to sign in` | The authorization code could not be exchanged for tokens, or `auth.openai.com` could not be reached. |
| `oh-fx login: authorization expired; run oh-fx login again` | Nothing came back to the callback within five minutes. |
