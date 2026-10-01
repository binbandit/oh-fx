# Working in oh-fx

- Work on a branch and open a ready-for-review pull request against `main`.
- Rebase onto the latest `main` before opening the PR. Keep the title and description concise and explain the change and validation.
- Run `node --test .github/tests/tidy-pullfrog-reviews.test.cjs` and `actionlint` for automation changes.
- The repository starts without application code. When adding it, add its build and test commands to CI and make failures block the required `Repository checks` job.
- Pullfrog reviews new PRs and new commits, including agent-authored PRs. Fix valid findings and rerun relevant tests; don't approve or merge around a failing review check.
- Mergify automatically queues reviewed PRs, runs CI against the current base, and squash-merges passing changes. No manual merge or human approval is needed.
- Keep drafts as drafts while work is incomplete. Pullfrog waits until they are marked ready.
- Leave `mergify/merge-queue/` branches and temporary PRs alone. They run CI without a duplicate Pullfrog review.
- Preserve the Pullfrog approval checks in Mergify's `queue_conditions`. Adding them to GitHub required checks or `merge_conditions` blocks the temporary queue PRs.
- Preserve the rule that only Mergify may update `main`. Do not weaken review or CI requirements to get a PR merged.
