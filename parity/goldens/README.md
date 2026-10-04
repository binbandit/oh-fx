# Upstream goldens

This partial OH-7 coverage includes the fixed system prompt, compaction system instruction and permission-review policy. Tool metadata, CLI help, dynamic compaction notes requests, and other prompts remain outside this slice.

Check without writes with `cargo xtask parity goldens --check --upstream PATH`. The parity CI job explicitly fetches the pinned snapshot and runs this check. Regenerate offline with `cargo xtask parity goldens --upstream PATH` (or `OH_FX_UPSTREAM`). The repository must contain the commit named by `parity/UPSTREAM`; its HEAD and working files are not inputs. The command reads Git objects with lazy fetching explicitly disabled and does not fetch, check out, or change the upstream repository. A missing blob in a partial clone fails locally; source blobs must already be available. The pin must resolve as a commit. All registered sources are read and every extractor is validated before any golden is replaced. `GIT_NO_LAZY_FETCH=1` and an empty `GIT_ALLOW_PROTOCOL` are set after inherited Git variables are removed, avoiding a Git2.44-only command option.

| Golden | Upstream source | Extraction |
|---|---|---|
| `system_prompt.md` | `src/builtins/system_prompt.md` | Entire blob, retaining its final newline |
| `compaction_system_prompt.txt` | `src/core/compactor/summarize.zig` | Concatenated plain string literals in the unique `pub const system_prompt` declaration, without an added newline |
| `review-policy/review_policy.xml` | `src/core/permissions/auto_classifier.zig` | Unique Zig multiline `review_policy_template`, retaining its final newline; see [policy provenance](review-policy/README.md) |

The extractor rejects changed syntax, escapes, empty prompts, missing declarations, and duplicate declarations. Such changes require a source review rather than silently changing extraction rules.

## Baseline evidence

At both `34f1ed14de47760b44d628ec2d5eb28055b9adf0` and `6bdd49736abd4a85fb6dd60a824e4329784f34c0`, the complete system-prompt blob is `b313d59d8944d573ed3c5005968ad8e6ebd97f1a`. The compaction source blobs differ (`9eb0039f31ac86d1937c1ef7840bab12dc06d44a` and `83f5af40bb587f014ba9dd0ef9197cf47d5d9013`), but the extracted fixed system instruction is identical. This slice leaves the existing parity pin unchanged.

## Exact substitutions

The app test applies each contextual replacement exactly once, and fails if a context is absent or repeated:

- `You are fx,` → `You are oh-fx,`
- `For questions about fx,` → `For questions about oh-fx,`
- `which fx renders` → `which oh-fx renders`
- `since fx already bolds` → `since oh-fx already bolds`

These substitutions capture the shipped product name difference. The URL `https://fx.sh/llms.txt` is preserved. The compaction instruction has no substitutions. Tests compare the actual app and summarizer constants byte for byte, including whitespace and newline boundaries. Mismatches in reserved behavior belong on OH-7; this slice changes no runtime behavior.
