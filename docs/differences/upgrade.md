# Upgrades and releases

- There is one release stream, so `oh-fx upgrade` has no `--channel` option.
- Releases come from GitHub Releases. Every push to `main` that changes more than documentation publishes `v<version>-dev.<n>`, where `<n>` is the commit count on `main`.
- The interactive shell checks for upgrades in-process as upstream does: 10 seconds after launch and then every five minutes instead of upstream's thirty, so each merge reaches users quickly. The footer shows upstream's `upgrading to <version>...`, `update ready: ctrl+g to reload`, and `upgrade failed`; a binary another oh-fx process installed meanwhile counts as ready without a download. Leaving the shell stops a check or download at once and waits out an install already under way. One-shot commands, which upstream never upgrades, schedule a detached `oh-fx upgrade --background` instead, at most once every five minutes, and every installer shares one file lock, so two never overlap.
