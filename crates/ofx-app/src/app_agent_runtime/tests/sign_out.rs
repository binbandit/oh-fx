use super::*;

const BUSY: &str = "Sign out is unavailable until active and queued work finishes.";

fn codex_login(harness: &Harness) -> std::path::PathBuf {
    harness.home.path().join("data/chatgpt-auth.json")
}

#[tokio::test]
async fn the_selected_codex_login_stays_while_a_prompt_waits_for_sign_in() {
    let codex = FakeServer::start([codex_text("found it")]);
    let catalog = codex_catalog(false, 2);
    let mut harness = signed_out(&codex, &catalog).await;
    held(&mut harness, "hello").await;
    save_login(&harness);
    for command in ["/logout codex", "/logout"] {
        assert_eq!(
            notices_of(&mut harness, command).await,
            [auth(NoticeTone::Warning, BUSY)],
            "{command}"
        );
    }
    for (command, body) in [
        ("/logout grok", "No Grok login session found."),
        ("/logout vercel", "No oh-fx login session found."),
    ] {
        assert_eq!(
            notices_of(&mut harness, command).await,
            [auth(NoticeTone::Neutral, body)],
            "{command}"
        );
    }
    assert!(codex_login(&harness).exists());
    harness.send(UiCommand::RetryHeldPrompt);
    within(harness.until(finished(TurnOutcome::Completed))).await;
    assert!(codex.requests()[0].json().to_string().contains("hello"));
}

#[tokio::test]
async fn the_selected_codex_login_stays_while_a_prompt_waits_behind_a_skill_installation() {
    let codex = FakeServer::start([codex_text("done")]);
    let catalog = codex_catalog(false, 1);
    let mut harness = Harness::codex(&codex, &catalog).await;
    write_skill(&harness.home, "install-pack", "new-skill");
    let source = fs::canonicalize(harness.home.path().join("workspace/install-pack")).unwrap();
    let (release, worker) = held_install_lock(&harness.home);
    harness.command(&format!("/skills install {}", source.display()));
    harness.submit("use the new skill");
    assert_eq!(
        notices_of(&mut harness, "/logout codex").await,
        [auth(NoticeTone::Warning, BUSY)]
    );
    assert!(codex_login(&harness).exists());
    release.send(()).unwrap();
    within(harness.until(finished(TurnOutcome::Completed))).await;
    worker.join().unwrap();
    let request = &codex.requests()[0];
    assert!(request.json().to_string().contains("use the new skill"));
    assert_eq!(
        request.header("authorization"),
        Some("Bearer eyJhbGciOiJub25lIn0.c2F2ZWQtYWNjZXNz.c2lnbmF0dXJl")
    );
}
