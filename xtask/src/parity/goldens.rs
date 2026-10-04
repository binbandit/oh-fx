use std::path::Path;

pub(super) fn run(options: &[&str]) -> Result<(), String> {
    let upstream =
        super::upstream_path(options, std::env::var_os("OH_FX_UPSTREAM").map(Into::into))?;
    let upstream = upstream.canonicalize().map_err(|error| error.to_string())?;
    crate::workspace_files::enter_repository_root()?;
    regenerate(Path::new("."), &upstream)
}

fn regenerate(root: &Path, upstream: &Path) -> Result<(), String> {
    let pin = super::read_pin(root)?;
    let system = super::git(
        upstream,
        &[
            "--no-lazy-fetch",
            "show",
            &format!("{pin}:src/builtins/system_prompt.md"),
        ],
    )?;
    let source = super::git(
        upstream,
        &[
            "--no-lazy-fetch",
            "show",
            &format!("{pin}:src/core/compactor/summarize.zig"),
        ],
    )?;
    let compaction = extract(&source)?;
    let destination = root.join("parity/goldens");
    std::fs::create_dir_all(&destination).map_err(|error| error.to_string())?;
    for (name, content) in [
        ("system_prompt.md", system),
        ("compaction_system_prompt.txt", compaction),
    ] {
        std::fs::write(destination.join(name), content).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn extract(source: &str) -> Result<String, String> {
    const DECLARATION: &str = "pub const system_prompt =";
    if source.lines().filter(|line| *line == DECLARATION).count() != 1 {
        return Err("expected one compaction system_prompt declaration".to_owned());
    }
    let (_, body) = source.split_once(DECLARATION).ok_or("missing prompt")?;
    let mut output = String::new();
    for line in body.lines().skip(1) {
        let line = line.trim();
        let (literal, finished) = if let Some(literal) = line.strip_suffix(';') {
            (literal, true)
        } else if let Some(literal) = line.strip_suffix(" ++") {
            (literal, false)
        } else {
            return Err("unsupported compaction prompt declaration".to_owned());
        };
        let literal = literal
            .strip_prefix('"')
            .and_then(|text| text.strip_suffix('"'))
            .ok_or("unsupported compaction prompt literal")?;
        if literal.contains(['"', '\\']) {
            return Err("unsupported compaction prompt escape".to_owned());
        }
        output.push_str(literal);
        if finished {
            return if output.is_empty() {
                Err("empty compaction prompt".to_owned())
            } else {
                Ok(output)
            };
        }
    }
    Err("unterminated compaction prompt declaration".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_prompt_extraction_preserves_joined_bytes() {
        assert_eq!(
            extract("pub const system_prompt =\n    \"one \" ++\n    \"two.\";\n").unwrap(),
            "one two."
        );
    }

    #[test]
    fn fixed_prompt_extraction_rejects_missing_duplicate_or_changed_syntax() {
        for source in [
            "",
            "pub const system_prompt =\n    other;",
            "pub const system_prompt =\n    \"escape\\n\";",
            "pub const system_prompt =\n    \"unterminated\" ++",
            "pub const system_prompt =\n    \"a\";\npub const system_prompt =\n    \"b\";",
        ] {
            assert!(extract(source).is_err(), "{source}");
        }
    }

    #[test]
    fn unsupported_arguments_do_not_regenerate() {
        assert!(run(&["--unknown"]).is_err());
    }

    #[test]
    fn regeneration_uses_pinned_objects_and_preserves_goldens_on_source_failure() {
        let root = tempfile::tempdir().unwrap();
        let upstream = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let upstream = upstream.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("parity")).unwrap();
        std::fs::create_dir_all(upstream.join("src/builtins")).unwrap();
        std::fs::create_dir_all(upstream.join("src/core/compactor")).unwrap();
        std::fs::write(upstream.join("src/builtins/system_prompt.md"), "original\n").unwrap();
        std::fs::write(
            upstream.join("src/core/compactor/summarize.zig"),
            "pub const system_prompt =\n    \"notes\";\n",
        )
        .unwrap();
        super::super::git(&upstream, &["init", "-q"]).unwrap();
        super::super::git(&upstream, &["add", "-A"]).unwrap();
        super::super::git(
            &upstream,
            &[
                "-c",
                "user.name=Parity Fixture",
                "-c",
                "user.email=parity@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "test: prompt fixture",
            ],
        )
        .unwrap();
        let pin = super::super::git(&upstream, &["rev-parse", "HEAD"]).unwrap();
        std::fs::write(root.join("parity/UPSTREAM"), &pin).unwrap();
        std::fs::write(
            upstream.join("src/builtins/system_prompt.md"),
            "dirty source",
        )
        .unwrap();
        regenerate(&root, &upstream).unwrap();
        assert_eq!(
            std::fs::read(root.join("parity/goldens/system_prompt.md")).unwrap(),
            b"original\n"
        );
        assert_eq!(
            std::fs::read(root.join("parity/goldens/compaction_system_prompt.txt")).unwrap(),
            b"notes"
        );
        std::fs::write(
            root.join("parity/UPSTREAM"),
            "0000000000000000000000000000000000000000",
        )
        .unwrap();
        assert!(regenerate(&root, &upstream).is_err());
        assert_eq!(
            std::fs::read(root.join("parity/goldens/system_prompt.md")).unwrap(),
            b"original\n"
        );
        std::fs::write(
            upstream.join("src/core/compactor/summarize.zig"),
            "changed declaration",
        )
        .unwrap();
        super::super::git(&upstream, &["add", "-A"]).unwrap();
        super::super::git(
            &upstream,
            &[
                "-c",
                "user.name=Parity Fixture",
                "-c",
                "user.email=parity@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "test: changed fixture",
            ],
        )
        .unwrap();
        let changed_pin = super::super::git(&upstream, &["rev-parse", "HEAD"]).unwrap();
        std::fs::write(root.join("parity/UPSTREAM"), changed_pin).unwrap();
        assert!(regenerate(&root, &upstream).is_err());
        assert_eq!(
            std::fs::read(root.join("parity/goldens/system_prompt.md")).unwrap(),
            b"original\n"
        );
        std::fs::write(root.join("parity/UPSTREAM"), "invalid").unwrap();
        assert!(regenerate(&root, &upstream).is_err());
    }

    fn commit_fixture(upstream: &Path) {
        super::super::git(
            upstream,
            &[
                "-c",
                "user.name=Parity Fixture",
                "-c",
                "user.email=parity@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "test: promisor fixture",
            ],
        )
        .unwrap();
    }

    fn object_files(directory: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
        let mut files = std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(object_files(&path));
            } else {
                let bytes = std::fs::read(&path).unwrap();
                files.insert(path, bytes);
            }
        }
        files
    }

    #[test]
    fn missing_promisor_blobs_never_fetch_or_change_objects_or_goldens() {
        for missing in [
            "src/builtins/system_prompt.md",
            "src/core/compactor/summarize.zig",
        ] {
            let fixture = tempfile::tempdir().unwrap();
            let fixture = fixture.path().canonicalize().unwrap();
            let upstream = fixture.join("upstream");
            let donor = fixture.join("donor");
            let root = fixture.join("root");
            std::fs::create_dir_all(upstream.join("src/builtins")).unwrap();
            std::fs::create_dir_all(upstream.join("src/core/compactor")).unwrap();
            std::fs::create_dir_all(root.join("parity/goldens")).unwrap();
            std::fs::write(upstream.join("src/builtins/system_prompt.md"), "prompt").unwrap();
            std::fs::write(
                upstream.join("src/core/compactor/summarize.zig"),
                "pub const system_prompt =\n    \"notes\";\n",
            )
            .unwrap();
            super::super::git(&upstream, &["init", "-q"]).unwrap();
            super::super::git(&upstream, &["add", "-A"]).unwrap();
            commit_fixture(&upstream);
            super::super::git(
                &fixture,
                &[
                    "clone",
                    "--bare",
                    "--no-hardlinks",
                    upstream.to_str().unwrap(),
                    donor.to_str().unwrap(),
                ],
            )
            .unwrap();
            let pin = super::super::git(&upstream, &["rev-parse", "HEAD"]).unwrap();
            std::fs::write(root.join("parity/UPSTREAM"), &pin).unwrap();
            for name in ["system_prompt.md", "compaction_system_prompt.txt"] {
                std::fs::write(root.join("parity/goldens").join(name), "previous fixture").unwrap();
            }
            let marker = fixture.join("contacted");
            let upload_pack = fixture.join("upload-pack");
            std::fs::write(
                &upload_pack,
                format!(
                    "printf contacted > '{}'\nexec git-upload-pack \"$@\"\n",
                    marker.display()
                ),
            )
            .unwrap();
            super::super::git(
                &upstream,
                &["config", "remote.origin.url", donor.to_str().unwrap()],
            )
            .unwrap();
            super::super::git(&upstream, &["config", "remote.origin.promisor", "true"]).unwrap();
            super::super::git(
                &upstream,
                &["config", "remote.origin.partialclonefilter", "blob:none"],
            )
            .unwrap();
            super::super::git(
                &upstream,
                &[
                    "config",
                    "remote.origin.uploadpack",
                    &format!("sh '{}'", upload_pack.display()),
                ],
            )
            .unwrap();
            let blob =
                super::super::git(&upstream, &["rev-parse", &format!("HEAD:{missing}")]).unwrap();
            let blob = blob.trim();
            std::fs::remove_file(
                upstream
                    .join(".git/objects")
                    .join(&blob[..2])
                    .join(&blob[2..]),
            )
            .unwrap();
            let before = object_files(&upstream.join(".git/objects"));
            let outcome = regenerate(&root, &upstream);
            assert!(!marker.exists(), "contacted promisor remote for {missing}");
            assert!(outcome.is_err(), "accepted missing blob {missing}");
            assert_eq!(before, object_files(&upstream.join(".git/objects")));
            for name in ["system_prompt.md", "compaction_system_prompt.txt"] {
                assert_eq!(
                    std::fs::read(root.join("parity/goldens").join(name)).unwrap(),
                    b"previous fixture"
                );
            }
        }
    }
}
