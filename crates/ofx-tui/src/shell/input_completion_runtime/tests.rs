use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use ofx_contract::{
    QuestionBatchEntry, QuestionOption, QuestionRequest, RequestId, ResumeRefusal, SessionPage,
    SessionRow, SessionScope, SkillMenuFocus, SkillMenuGroup, SkillMenuItem, SkillMenuSource,
    TurnId, UiCommand, UiEvent,
};

use super::*;
use crate::shell::PromptHistory;
use crate::shell::test_shell::TestShell;

const DOWN: &[u8] = b"\x1b[B";
const UP: &[u8] = b"\x1b[A";
const ESCAPE: &[u8] = b"\x1b";

#[derive(Default)]
struct FakeIndex {
    files: Vec<(String, MentionKind)>,
    loading: bool,
    generation: usize,
    refreshes: usize,
    gone: Vec<String>,
}

struct FakeSource(Rc<RefCell<FakeIndex>>);

fn explicit(query: &str) -> bool {
    query.contains('/') || matches!(query, "~" | "." | "..")
}

fn matched(path: &str, kind: MentionKind, query: &str) -> Option<FileMatch> {
    let start = path.find(query)?;
    Some(FileMatch {
        path: path.to_owned(),
        kind,
        spans: if query.is_empty() {
            Vec::new()
        } else {
            std::iter::once(start..start + query.len()).collect()
        },
    })
}

impl FileMentionSource for FakeSource {
    fn revision(&self) -> IndexRevision {
        let index = self.0.borrow();
        IndexRevision {
            scope_epoch: 0,
            generation: index.generation,
            count: if index.loading { 0 } else { index.files.len() },
            state: if index.loading {
                IndexState::Loading
            } else {
                IndexState::Ready
            },
        }
    }

    fn search(&self, revision: IndexRevision, query: &str, limit: usize) -> Option<Vec<FileMatch>> {
        let index = self.0.borrow();
        if revision.generation != index.generation {
            return None;
        }
        Some(
            index
                .files
                .iter()
                .take(revision.count)
                .filter_map(|(path, kind)| matched(path, *kind, query))
                .take(limit)
                .collect(),
        )
    }

    fn refresh(&mut self) {
        self.0.borrow_mut().refreshes += 1;
    }

    fn poll(&mut self) -> bool {
        false
    }

    fn is_loading(&self) -> bool {
        self.0.borrow().loading
    }

    fn depends_on_index(&self, query: &str) -> bool {
        !explicit(query)
    }

    fn is_current(&self, _: &str, path: &str, _: MentionKind) -> bool {
        !self.0.borrow().gone.iter().any(|gone| gone == path)
    }

    fn directory_lister(&self) -> DirectoryLister {
        Arc::new(|query: &str, _: usize, _: &AtomicBool| {
            (query == "src/").then(|| {
                vec![FileMatch {
                    path: "src/lib.rs".to_owned(),
                    kind: MentionKind::File,
                    spans: Vec::new(),
                }]
            })
        })
    }
}

fn shell_with(files: &[(&str, MentionKind)]) -> (TestShell, Rc<RefCell<FakeIndex>>) {
    let index = Rc::new(RefCell::new(FakeIndex {
        files: files
            .iter()
            .map(|(path, kind)| ((*path).to_owned(), *kind))
            .collect(),
        generation: 1,
        ..FakeIndex::default()
    }));
    let source = FakeSource(Rc::clone(&index));
    let test = TestShell::start_with(|options| {
        options.prompt_history = PromptHistory::disabled();
        options.file_mentions = Some(Box::new(source));
    });
    (test, index)
}

fn files() -> (TestShell, Rc<RefCell<FakeIndex>>) {
    shell_with(&[
        ("src/main.rs", MentionKind::File),
        ("src/mailbox.rs", MentionKind::File),
        ("docs/my notes.md", MentionKind::File),
        ("src", MentionKind::Directory),
    ])
}

fn press(test: &mut TestShell, bytes: &[u8]) -> String {
    test.type_bytes(bytes);
    test.draining(|shell| assert!(shell.step().unwrap().is_none()));
    test.screen()
}

#[test]
fn typing_an_at_opens_the_picker_under_the_mention_and_tab_inserts_the_selection() {
    let (mut test, index) = files();
    let screen = press(&mut test, b"look at @ma");
    assert!(screen.contains("┃ look at @ma"), "{screen}");
    let rows: Vec<&str> = screen.lines().collect();
    let first = rows
        .iter()
        .position(|row| row.contains("src/main.rs"))
        .unwrap_or_else(|| panic!("{screen}"));
    assert_eq!(rows[first], "          src/main.rs");
    assert_eq!(rows[first + 1], "          src/mailbox.rs");
    assert!(rows[first - 1].starts_with("────"), "{screen}");
    assert_eq!(index.borrow().refreshes, 1);
    press(&mut test, b"\t");
    assert_eq!(test.shell.composer.text(), "look at @src/main.rs ");
    let screen = test.screen();
    assert!(!screen.contains("src/mailbox.rs"), "{screen}");
    press(&mut test, b" too");
    assert_eq!(test.shell.composer.text(), "look at @src/main.rs too");
}

#[test]
fn arrows_and_control_keys_move_the_selection_and_enter_accepts_it_before_submitting() {
    let (mut test, _) = files();
    press(&mut test, b"@ma");
    press(&mut test, DOWN);
    press(&mut test, b"\r");
    assert_eq!(test.shell.composer.text(), "@src/mailbox.rs ");
    assert!(test.sent().is_empty());
    press(&mut test, b"@ma");
    press(&mut test, b"\n");
    press(&mut test, b"\x0b");
    press(&mut test, UP);
    let screen = press(&mut test, b"\r");
    assert_eq!(
        test.shell.composer.text(),
        "@src/mailbox.rs @src/mailbox.rs "
    );
    assert!(!screen.contains("────"), "{screen}");
    press(&mut test, b"\r");
    assert_eq!(
        test.sent(),
        [UiCommand::Submit {
            prompt: "@src/mailbox.rs @src/mailbox.rs".to_owned(),
            skills: Vec::new(),
        }]
    );
}

#[test]
fn paths_that_need_quotes_are_inserted_quoted() {
    let (mut test, _) = files();
    press(&mut test, b"@notes");
    press(&mut test, b"\t");
    assert_eq!(test.shell.composer.text(), "@\"docs/my notes.md\" ");
}

#[test]
fn escape_dismisses_the_picker_until_the_mention_changes() {
    let (mut test, _) = files();
    press(&mut test, b"@ma");
    let screen = press(&mut test, ESCAPE);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut screen = screen;
    while screen.contains("src/main.rs") {
        assert!(Instant::now() < deadline, "{screen}");
        test.advance(50);
        screen = press(&mut test, b"");
    }
    assert_eq!(test.shell.composer.text(), "@ma");
    let screen = press(&mut test, b"i");
    assert!(!screen.contains("src/main.rs"), "{screen}");
    press(&mut test, b"\x7f\x7f\x7f\x7f");
    let screen = press(&mut test, b"@mai");
    assert!(screen.contains("src/main.rs"), "{screen}");
}

#[test]
fn an_open_skills_menu_owns_the_footer_until_it_closes() {
    let (mut test, _) = files();
    let screen = press(&mut test, b"@ma");
    assert!(screen.contains("src/main.rs"), "{screen}");
    test.deliver(UiEvent::SkillsMenu {
        items: vec![SkillMenuItem {
            name: "review".to_owned(),
            description: String::new(),
            path: PathBuf::from("/skills/review"),
            source: SkillMenuSource::OhFx,
            group: SkillMenuGroup::Workspace,
            scope: "oh-fx · Workspace".to_owned(),
            source_label: String::new(),
        }],
        focus: SkillMenuFocus::Start,
    });
    let screen = press(&mut test, b"i");
    assert!(screen.contains("No skills found."), "{screen}");
    assert!(!screen.contains("src/main.rs"), "{screen}");
    assert!(!test.shell.file_picker_owns_surface());
    press(&mut test, DOWN);
    press(&mut test, b"\t\r");
    assert_eq!(test.shell.composer.text(), "@mai");
    assert!(test.sent().is_empty());
    press(&mut test, ESCAPE);
    test.advance(40);
    test.draining(|shell| shell.flush_pending_input().unwrap());
    assert!(test.shell.skills_menu.is_none());
    assert!(test.shell.composer.is_empty());
    let screen = press(&mut test, b"@mai");
    assert!(screen.contains("src/main.rs"), "{screen}");
}

#[test]
fn an_open_session_picker_owns_the_footer_and_filters_by_a_typed_at() {
    let (mut test, _) = files();
    let scope = SessionScope::CurrentWorkspace;
    test.deliver(UiEvent::SessionPickerOpened { scope });
    test.deliver(UiEvent::SessionsListed {
        page: SessionPage {
            scope,
            after: None,
            rows: vec![SessionRow {
                id: "mailbox".to_owned(),
                title: Some("fix @mailbox".to_owned()),
                workspace_root: "/workspace".to_owned(),
                updated_at_ms: 0,
                turns: 1,
                from_fx: false,
            }],
            has_more: false,
        },
    });
    let screen = press(&mut test, b"@ma");
    assert!(screen.contains("fix @mailbox"), "{screen}");
    assert!(!screen.contains("src/main.rs"), "{screen}");
    assert!(!test.shell.file_picker_owns_surface());
    press(&mut test, b"\t");
    assert_eq!(test.shell.composer.text(), "@ma");
    press(&mut test, b"\r");
    assert_eq!(
        test.sent().last(),
        Some(&UiCommand::ResumeSession {
            id: "mailbox".to_owned()
        })
    );
    test.deliver(UiEvent::SessionResumeFailed {
        id: "mailbox".to_owned(),
        refusal: ResumeRefusal::Unavailable,
    });
    press(&mut test, ESCAPE);
    test.advance(40);
    test.draining(|shell| shell.flush_pending_input().unwrap());
    assert!(!test.shell.picker_active());
    assert!(test.shell.composer.is_empty());
    let screen = press(&mut test, b"@mai");
    assert!(screen.contains("src/main.rs"), "{screen}");
}

#[test]
fn directories_continue_into_a_directory_listing() {
    let (mut test, _) = shell_with(&[
        ("src", MentionKind::Directory),
        ("lib/other.rs", MentionKind::File),
    ]);
    let screen = press(&mut test, b"@src");
    assert!(screen.lines().any(|row| row.trim() == "src/"), "{screen}");
    press(&mut test, b"\t");
    assert_eq!(test.shell.composer.text(), "@src/");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut screen = test.screen();
    while !screen.contains("src/lib.rs") {
        assert!(Instant::now() < deadline, "{screen}");
        screen = press(&mut test, b"");
    }
    press(&mut test, b"\t");
    assert_eq!(test.shell.composer.text(), "@src/lib.rs ");
}

#[test]
fn a_loading_index_shows_its_status_and_rows_arrive_once_it_is_ready() {
    let (mut test, index) = files();
    index.borrow_mut().loading = true;
    let screen = press(&mut test, b"@ma");
    assert!(screen.contains("indexing files..."), "{screen}");
    assert_eq!(index.borrow().refreshes, 0);
    assert!(test.shell.file_picker_busy());
    index.borrow_mut().loading = false;
    let screen = test.screen();
    assert!(screen.contains("src/main.rs"), "{screen}");
    let screen = press(&mut test, b"zzz");
    assert!(screen.contains("no matching files"), "{screen}");
}

#[test]
fn a_selection_that_vanished_is_refused_and_flagged() {
    let (mut test, index) = files();
    press(&mut test, b"@main");
    index.borrow_mut().gone.push("src/main.rs".to_owned());
    let screen = press(&mut test, b"\t");
    assert_eq!(test.shell.composer.text(), "@main");
    assert!(
        screen.contains("Selection unavailable. navigate to choose; tab to retry."),
        "{screen}"
    );
}

#[test]
fn without_a_source_at_mentions_are_plain_text() {
    let mut test = TestShell::start();
    let screen = press(&mut test, b"@src");
    assert!(!screen.contains("────"), "{screen}");
    press(&mut test, b"\t\r");
    assert_eq!(
        test.sent(),
        [UiCommand::Submit {
            prompt: "@src".to_owned(),
            skills: Vec::new(),
        }]
    );
}

#[test]
fn a_question_hides_the_picker_until_it_is_answered() {
    let (mut test, _) = files();
    test.submit("work");
    test.deliver(UiEvent::TurnStarted {
        turn_id: TurnId::new(1),
    });
    let screen = press(&mut test, b"look at @ma");
    assert!(screen.contains("src/main.rs"), "{screen}");
    test.deliver(UiEvent::QuestionRequested {
        turn_id: TurnId::new(1),
        request: QuestionRequest {
            id: RequestId::new(4),
            entries: vec![QuestionBatchEntry {
                question: "Proceed?".to_owned(),
                options: ["Yes", "No"]
                    .into_iter()
                    .map(|label| QuestionOption {
                        label: label.to_owned(),
                        description: None,
                    })
                    .collect(),
            }],
        },
    });
    let screen = test.screen();
    assert!(screen.contains("Proceed?"), "{screen}");
    assert!(!screen.contains("src/main.rs"), "{screen}");
    let screen = press(&mut test, DOWN);
    assert!(!screen.contains("src/main.rs"), "{screen}");
    press(&mut test, b"\r");
    assert!(test.sent().contains(&UiCommand::QuestionAnswered {
        request_id: RequestId::new(4),
        answers: Some(vec!["No".to_owned()]),
    }));
    assert_eq!(test.shell.composer.text(), "look at @ma");
    let screen = test.screen();
    assert!(screen.contains("src/main.rs"), "{screen}");
}

#[test]
fn a_file_query_typed_in_the_full_transcript_accepts_no_hidden_rows() {
    for key in [b"\t".as_slice(), b"\r"] {
        let (mut test, _) = files();
        press(&mut test, b"\x0f");
        let screen = press(&mut test, b"@ma");
        assert!(screen.contains("full detail"), "{screen}");
        assert!(!screen.contains("src/main.rs"), "{screen}");
        press(&mut test, key);
        assert!(!test.shell.composer.text().contains("src/main.rs"));
        assert!(!format!("{:?}", test.sent()).contains("src/main.rs"));
        assert!(test.shell.full_transcript.is_some());
    }
}

#[test]
fn file_completion_resumes_once_the_full_transcript_closes() {
    let (mut test, _) = files();
    press(&mut test, b"\x0f");
    press(&mut test, b"look at @ma");
    press(&mut test, b"\t");
    assert_eq!(test.shell.composer.text(), "look at @ma");
    let screen = press(&mut test, b"\x0f");
    assert!(!screen.contains("full detail"), "{screen}");
    assert!(screen.contains("src/main.rs"), "{screen}");
    press(&mut test, b"\t");
    assert_eq!(test.shell.composer.text(), "look at @src/main.rs ");
}
