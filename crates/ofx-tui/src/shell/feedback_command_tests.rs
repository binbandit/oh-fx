use ofx_contract::{Notice, NoticeTone, UiCommand, UiEvent};

use super::SlashCommandSpec;
use super::test_shell::TestShell;

#[test]
fn feedback_submits_to_the_controller_and_paints_its_notices() {
    for (tone, body) in [
        (
            NoticeTone::Neutral,
            "Opened https://github.com/binbandit/oh-fx/issues/new.",
        ),
        (
            NoticeTone::Error,
            "Could not open https://github.com/binbandit/oh-fx/issues/new. Open it manually.",
        ),
    ] {
        let mut test = TestShell::start_with(|options| {
            options.commands.push(SlashCommandSpec {
                command: "/feedback".to_owned(),
                aliases: Vec::new(),
                description: "open the fx feedback form".to_owned(),
                help_entry: "/feedback".to_owned(),
                takes_arguments: false,
                category: 10,
                compacts: false,
            });
        });
        test.submit("/feedback");
        assert_eq!(
            test.sent(),
            [UiCommand::RunCommand {
                text: "/feedback".to_owned()
            }]
        );
        test.deliver(UiEvent::Notice {
            notice: Notice::new(tone, "", body),
        });
        let screen = test.screen();
        assert!(screen.contains("https://github.com/binbandit/oh-fx/issues/new"));
        assert!(screen.contains(if tone == NoticeTone::Error {
            "Could not open"
        } else {
            "Opened"
        }));
        assert!(!screen.contains("Unknown command"));
    }
}
