use super::*;

fn skill(name: &str, path: &str, source: SkillSource) -> Skill {
    Skill {
        name: name.to_owned(),
        description: format!("{name} description"),
        path: PathBuf::from(path),
        source,
        read_authority: None,
    }
}

#[test]
fn skills_commands_parse_as_upstream_parses_them() {
    for (rest, expected) in [
        ("", Command::List),
        (" \t", Command::List),
        ("list", Command::List),
        (" list ", Command::List),
        ("show  review ", Command::Show("review")),
        ("create fresh", Command::Create("fresh")),
        ("remove old\t", Command::Remove("old")),
        ("path", Command::Path),
        ("paths", Command::Usage),
        ("list all", Command::Usage),
        ("show", Command::Usage),
        ("unknown", Command::Usage),
        (
            "add vercel-labs/agent-skills --skill review",
            Command::Install("vercel-labs/agent-skills"),
        ),
        (
            "install vercel-labs/agent-skills --skill=workflow",
            Command::Install("vercel-labs/agent-skills"),
        ),
        ("install ./pack", Command::Install("./pack")),
    ] {
        assert_eq!(parse(rest), expected, "{rest:?}");
    }
}

#[test]
fn managed_names_are_single_directory_names() {
    for valid in ["review", "a.b", "-x", "x".repeat(256).as_str()] {
        assert!(valid_managed_name(valid), "{valid}");
    }
    for invalid in [
        "",
        ".",
        "..",
        "nested/name",
        "nested\\name",
        "/absolute",
        "x".repeat(257).as_str(),
    ] {
        assert!(!valid_managed_name(invalid), "{invalid}");
    }
}

#[test]
fn the_template_matches_upstream_and_creates_a_missing_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("config/oh-fx/skills");
    let path = create_template(&root, "fresh-root").unwrap();
    assert_eq!(path, root.join("fresh-root/SKILL.md"));
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "---\nname: fresh-root\ndescription: Describe when this skill should activate\n---\n\n# fresh-root\n\nInstructions for this skill...\n"
    );
    let installed = skill(
        "fresh-root",
        root.join("fresh-root").to_str().unwrap(),
        SkillSource::GlobalOhFx,
    );
    remove_managed(&root, &installed).unwrap();
    assert!(!root.join("fresh-root").exists());
    assert!(root.exists());
}

#[test]
fn removing_a_managed_skill_never_leaves_the_managed_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("skills");
    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
    let linked = skill(
        "linked",
        root.join("linked").to_str().unwrap(),
        SkillSource::GlobalOhFx,
    );
    remove_managed(&root, &linked).unwrap();
    assert!(!root.join("linked").exists());
    assert!(outside.exists());
    let escaping = skill("escape", "/", SkillSource::GlobalOhFx);
    assert!(remove_managed(&root, &escaping).is_err());
    assert!(remove_managed(Path::new(""), &linked).is_err());
    assert!(create_template(Path::new(""), "fresh").is_err());
}

#[test]
fn the_path_notice_names_the_product_roots() {
    assert_eq!(
        path_notice(Path::new("/home/ada/.config/oh-fx/skills")),
        "oh-fx workspace roots are auto-discovered from .oh-fx/skills and skills/.\noh-fx managed install root: /home/ada/.config/oh-fx/skills\ncompatibility roots are auto-discovered from workspace and home (.opencode/.codex/.claude/.agents/.claw)."
    );
}

#[test]
fn menu_items_carry_their_tab_group_and_labels() {
    let item = menu_item(&skill(
        "review",
        "/w/.oh-fx/skills/review",
        SkillSource::WorkspaceOhFx,
    ));
    assert_eq!(item.source, SkillMenuSource::OhFx);
    assert_eq!(item.group, SkillMenuGroup::Workspace);
    assert_eq!(item.scope, "oh-fx · Workspace");
    assert_eq!(item.source_label, "workspace .oh-fx");
    let shared = menu_item(&skill(
        "shared",
        "/w/skills/shared",
        SkillSource::WorkspaceShared,
    ));
    assert_eq!(shared.source, SkillMenuSource::Workspace);
    assert_eq!(shared.scope, "oh-fx · Workspace");
    let managed = menu_item(&skill("managed", "/m/managed", SkillSource::GlobalOhFx));
    assert_eq!(managed.group, SkillMenuGroup::Managed);
    assert_eq!(managed.scope, "oh-fx · Global");
    let claude = menu_item(&skill(
        "c",
        "/h/.claude/skills/c",
        SkillSource::GlobalClaude,
    ));
    assert_eq!(claude.source, SkillMenuSource::Claude);
    assert_eq!(claude.group, SkillMenuGroup::Compatibility);
    assert_eq!(claude.scope, "Claude · Global");
    assert_eq!(
        source_label(SkillSource::GlobalClaude),
        "global ~/.claude/skills"
    );
}
