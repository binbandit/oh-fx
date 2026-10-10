# Usage

- The profile usage ledger is `$XDG_DATA_HOME/oh-fx/usage.jsonl`, locked through `usage.lock` beside it, where upstream keeps `~/.fx/usage.jsonl` and `~/.fx/usage.lock`. Its records are upstream's byte for byte, so a ledger fx wrote reads the same in oh-fx. `oh-fx usage` reads only oh-fx's ledger and never fx's, as upstream reads only its own profile.
- oh-fx does not record usage yet: it writes no `usage-v2.json` sidecars and appends nothing to the ledger, so `oh-fx usage` reports that tracking has not started unless a ledger is already there. Without session accounting there are no usage recovery markers either, so a report has no unpublished session usage to fold in, where upstream's adds the usage of the sessions marked in `~/.fx/usage-recovery`.
