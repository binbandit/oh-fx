use ofx_contract::{Notice, NoticeTone};

const URL: &str = "https://github.com/binbandit/oh-fx/issues/new";

pub(crate) fn feedback_notice(open: impl FnOnce(&str) -> bool) -> Notice {
    if open(URL) {
        Notice::new(NoticeTone::Neutral, "", format!("Opened {URL}."))
    } else {
        Notice::new(
            NoticeTone::Error,
            "",
            format!("Could not open {URL}. Open it manually."),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_opens_the_issue_form_once_and_reports_the_launch_result() {
        for (opened, tone, body) in [
            (
                true,
                NoticeTone::Neutral,
                "Opened https://github.com/binbandit/oh-fx/issues/new.",
            ),
            (
                false,
                NoticeTone::Error,
                "Could not open https://github.com/binbandit/oh-fx/issues/new. Open it manually.",
            ),
        ] {
            let mut calls = Vec::new();
            let notice = feedback_notice(|url| {
                calls.push(url.to_owned());
                opened
            });
            assert_eq!(calls, [URL]);
            assert_eq!(notice, Notice::new(tone, "", body));
        }
    }
}
