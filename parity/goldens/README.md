# Upstream goldens

Each golden holds fixed bytes that oh-fx ships, extracted from upstream at the commit in `parity/UPSTREAM`. A test in the crate that ships the value compares it with its golden byte for byte. This is partial OH-7 coverage: tool metadata, CLI help, dynamic compaction notes requests, and other prompts are not covered yet.

`cargo xtask parity goldens --check --upstream PATH` compares every golden with its pinned source and writes nothing; the parity CI job fetches the pinned snapshot and runs it. `cargo xtask parity goldens --upstream PATH` (or `OH_FX_UPSTREAM`) regenerates them. Both read Git objects at `parity/UPSTREAM`, which must resolve as a commit; the upstream HEAD and working files are not inputs. Neither fetches, checks out, or changes the upstream repository: inherited `GIT_*` variables are removed, `GIT_NO_LAZY_FETCH=1` disables lazy fetches, and an empty `GIT_ALLOW_PROTOCOL` refuses every transport, so a Git older than 2.44, which lacks `--no-lazy-fetch`, cannot fetch either. A blob missing from a partial clone fails locally. Every source is read and every extractor validated before any golden is written.

| Golden | Upstream source | Extraction |
|---|---|---|
| `system_prompt.md` | `src/builtins/system_prompt.md` | Entire blob, retaining its final newline |
| `compaction_system_prompt.txt` | `src/core/compactor/summarize.zig` | Concatenated plain string literals in the unique `pub const system_prompt` declaration, without an added newline |
| `review_policy.xml` | `src/core/permissions/auto_classifier.zig` | Lines of the unique `const review_policy_template` Zig multiline string, each without its four-space `\\` prefix, joined with newlines up to the standalone `;`, retaining blank lines and the final newline |

Each extractor accepts only the source grammar it was reviewed against, and rejects changed syntax, escapes, empty values, and missing or duplicate declarations. Such a change needs a source review rather than a silent change to the extraction rules.

## Baseline evidence

At both `34f1ed14de47760b44d628ec2d5eb28055b9adf0` and `6bdd49736abd4a85fb6dd60a824e4329784f34c0`, the complete system-prompt blob is `b313d59d8944d573ed3c5005968ad8e6ebd97f1a`. The compaction source blobs differ (`9eb0039f31ac86d1937c1ef7840bab12dc06d44a` and `83f5af40bb587f014ba9dd0ef9197cf47d5d9013`), but the extracted fixed system instruction is identical.

The permission-review source blob is `f0d5cec96da2461ea5c81e19f60abf3da2e3f1ef` at both commits. The extracted policy is 3,195 bytes with SHA-256 `15a6347eb5ad37c65c7559d2d0a513b779951fc43a02d1734c718b0b5119581e`, the digest upstream's own artifact test asserts. Its test also checks the single `{{REVIEW_DATA}}` marker and the final newline.

## Exact substitutions

The app test applies each contextual replacement exactly once, and fails if a context is absent or repeated:

- `You are fx,` → `You are oh-fx,`
- `For questions about fx,` → `For questions about oh-fx,`
- `which fx renders` → `which oh-fx renders`
- `since fx already bolds` → `since oh-fx already bolds`

These substitutions capture the shipped product name difference. The URL `https://fx.sh/llms.txt` is preserved, as recorded in [the agent differences](../../docs/differences/agent.md). The compaction instruction and the review policy have no substitutions; the policy's `fx` stays as upstream wrote it.
