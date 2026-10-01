# oh-fx

A Rust port of [fx](https://github.com/vercel-labs/fx), the terminal coding agent.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/binbandit/oh-fx/main/install.sh | sh
```

This installs `oh-fx` and an `ofx` alias into `~/.local/bin`. Set `OH_FX_INSTALL_DIR` to install elsewhere, or install a specific release by passing its tag with `sh -s v0.1.0-dev.12`.

Every push to `main` that passes CI publishes a release. Installed binaries check for a newer release in the background at most every five minutes and replace themselves, so the next launch runs it. Run `oh-fx upgrade` to upgrade immediately, or set `OH_FX_AUTO_UPGRADE=0` to turn background upgrades off and stay on the installed release.

### From a checkout

With [just](https://github.com/casey/just) installed:

| Command | Effect |
| --- | --- |
| `just install` | Builds a release binary and installs `oh-fx` and the `ofx` alias the same way as the script, honoring `OH_FX_INSTALL_DIR`. |
| `just build` | Builds the release binary only. |
| `just run <args>` | Runs oh-fx from the checkout, for example `just run ask "summarize this repository"`. |

Binaries built from a checkout report a `-local` version and never upgrade themselves.

## Automated pull requests

Open a ready-for-review PR against `main`. Pullfrog reviews it and re-reviews
new commits. Once its run and approval checks pass, Mergify queues the PR,
checks it against the latest `main`, and squash-merges it automatically.
Merged branches are deleted automatically.

Agent-authored PRs follow the same flow, including Pullfrog's own PRs. The
required verdict is Pullfrog's approval check, so GitHub's restriction on
approving your own PR does not require a human to step in. Outstanding change
requests still block merging.

Draft PRs wait until they are marked ready. Mergify's temporary draft PRs only
run CI, avoiding duplicate reviews of changes Pullfrog has already checked.
Older, superseded Pullfrog review summaries are collapsed while current and
unresolved feedback stays visible.

## Checks

`Repository checks` is the required check on every PR, including Mergify's
integration PRs. It passes only when these jobs pass:

- `Automation checks`: validates GitHub Actions workflows and runs the review
  cleanup regression tests.
- `Rust lint`: formatting, the comment ban, clippy, the attribution check, and
  unused dependencies.
- `Rust tests`: the workspace tests on Linux and macOS.

Run the Rust checks locally with `cargo xtask ci`. Run `cargo xtask hooks` once
to enable the git hooks described in [AGENTS.md](AGENTS.md).

For local automation checks, use Node.js 24 and actionlint 1.7.12:

```sh
node --test .github/tests/tidy-pullfrog-reviews.test.cjs
actionlint
```

See [AGENTS.md](AGENTS.md) for the agent workflow and [.mergify.yml](.mergify.yml)
for the queue configuration.
