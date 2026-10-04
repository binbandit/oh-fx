# Upgrades and releases

- There is one release stream, so `oh-fx upgrade` has no `--channel` option.
- Releases come from GitHub Releases. Every push to `main` that changes more than documentation publishes `v<version>-dev.<n>`, where `<n>` is the commit count on `main`.
- Upgrade checks run from every command through a detached background process, at most once every five minutes when commands run, instead of upstream's thirty, so each merge reaches users quickly.
