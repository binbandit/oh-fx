use ofx_contract::ReasoningEffort;

pub(crate) const MODEL_PREFIX: &str = "/model ";
pub(crate) const FAST_OPTIONS: [&str; 2] = ["normal", "fast"];
const PICKER_SPACES: [char; 2] = [' ', '\t'];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ModelStage {
    #[default]
    Model,
    Effort,
    Fast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModelQuery<'a> {
    pub(crate) stage: ModelStage,
    pub(crate) query: &'a str,
    pub(crate) token_start: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Cursor {
    pub(crate) index: usize,
    pub(crate) window_start: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ModelFlow {
    pub(crate) stage: ModelStage,
    pub(crate) pending: String,
    pub(crate) model: Cursor,
    pub(crate) effort: Cursor,
    pub(crate) fast: Cursor,
    pub(crate) anchor_current: bool,
    pub(crate) dismissed: bool,
    pub(crate) owned_revision: Option<u64>,
}

impl ModelFlow {
    pub(crate) fn query<'a>(&self, text: &'a str) -> Option<ModelQuery<'a>> {
        if self.dismissed {
            return None;
        }
        self.raw_query(text)
    }

    pub(crate) fn raw_query<'a>(&self, text: &'a str) -> Option<ModelQuery<'a>> {
        let lead = text.len() - text.trim_start_matches(PICKER_SPACES).len();
        let trimmed = &text[lead..];
        let after = strip_prefix_ignore_case(trimmed, MODEL_PREFIX)?;
        let token_start = match self.stage {
            ModelStage::Model => MODEL_PREFIX.len(),
            stage => {
                if self.pending.is_empty() {
                    return None;
                }
                let rest = after.strip_prefix(self.pending.as_str())?;
                let effort_start = trimmed.len() - rest.trim_start_matches(PICKER_SPACES).len();
                if stage == ModelStage::Effort {
                    effort_start
                } else {
                    let effort = &trimmed[effort_start..];
                    if effort.is_empty() {
                        return None;
                    }
                    let effort_end = effort.find(PICKER_SPACES).unwrap_or(effort.len());
                    let mode = &effort[effort_end..];
                    trimmed.len() - mode.trim_start_matches(PICKER_SPACES).len()
                }
            }
        };
        Some(ModelQuery {
            stage: self.stage,
            query: &trimmed[token_start..],
            token_start: lead + token_start,
        })
    }

    pub(crate) fn begin(
        &mut self,
        model: &str,
        effort_index: usize,
        fast_mode: bool,
        stage: ModelStage,
    ) {
        model.clone_into(&mut self.pending);
        self.stage = stage;
        self.effort = Cursor {
            index: effort_index,
            window_start: 0,
        };
        self.fast = Cursor {
            index: usize::from(fast_mode),
            window_start: 0,
        };
    }

    pub(crate) fn clear(&mut self) {
        let dismissed = self.dismissed;
        *self = Self {
            dismissed,
            ..Self::default()
        };
    }

    pub(crate) fn reset_active_index(&mut self) {
        match self.stage {
            ModelStage::Model => {
                self.model = Cursor::default();
                self.anchor_current = false;
            }
            ModelStage::Effort => self.effort = Cursor::default(),
            ModelStage::Fast => self.fast = Cursor::default(),
        }
    }

    pub(crate) fn active_cursor(&mut self) -> &mut Cursor {
        match self.stage {
            ModelStage::Model => &mut self.model,
            ModelStage::Effort => &mut self.effort,
            ModelStage::Fast => &mut self.fast,
        }
    }
}

pub(crate) fn is_bare_model_command(text: &str, cursor: usize) -> bool {
    cursor == text.len()
        && text
            .trim_start_matches(PICKER_SPACES)
            .eq_ignore_ascii_case(MODEL_PREFIX.trim_end())
}

pub(crate) fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

pub(crate) fn matches_query(label: &str, query: &str) -> bool {
    let query = query.trim_matches(PICKER_SPACES);
    query.is_empty() || ofx_text::contains_ignore_case(label, query)
}

pub(crate) fn effort_options(efforts: &[String]) -> Vec<ReasoningEffort> {
    if efforts.is_empty() {
        return Vec::new();
    }
    std::iter::once(ReasoningEffort::Auto)
        .chain(efforts.iter().cloned().map(ReasoningEffort::Named))
        .collect()
}

pub(crate) fn effort_index(efforts: &[String], effort: &ReasoningEffort) -> usize {
    match effort {
        ReasoningEffort::Auto => 0,
        ReasoningEffort::Named(name) => efforts
            .iter()
            .position(|offered| offered == name)
            .map_or(0, |position| position + 1),
    }
}

pub(crate) fn effort_at(efforts: &[String], index: usize) -> ReasoningEffort {
    if index == 0 || efforts.is_empty() {
        return ReasoningEffort::Auto;
    }
    ReasoningEffort::Named(efforts[(index - 1) % efforts.len()].clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExplicitModel<'a> {
    None,
    Invalid,
    Pick {
        model: &'a str,
        effort: ReasoningEffort,
        fast_mode: Option<bool>,
    },
}

pub(crate) fn explicit_model(text: &str) -> ExplicitModel<'_> {
    let trimmed = text.trim_matches([' ', '\t', '\r', '\n']);
    let Some(arguments) = strip_prefix_ignore_case(trimmed, MODEL_PREFIX.trim_end())
        .filter(|rest| rest.starts_with([' ', '\t']))
    else {
        return ExplicitModel::None;
    };
    let tokens: Vec<&str> = arguments
        .split([' ', '\t', '\r', '\n'])
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.len() > 4 {
        return ExplicitModel::Invalid;
    }
    let (Some(model), Some(effort)) = (tokens.first(), tokens.get(1)) else {
        return ExplicitModel::None;
    };
    let Some(effort) = ReasoningEffort::parse(effort) else {
        return ExplicitModel::None;
    };
    let fast_mode = match tokens.get(2..) {
        Some([]) => None,
        Some([mode]) if mode.eq_ignore_ascii_case(FAST_OPTIONS[1]) => Some(true),
        Some([mode]) if mode.eq_ignore_ascii_case(FAST_OPTIONS[0]) => Some(false),
        _ => return ExplicitModel::Invalid,
    };
    ExplicitModel::Pick {
        model,
        effort,
        fast_mode,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(stage: ModelStage, pending: &str) -> ModelFlow {
        let mut flow = ModelFlow::default();
        flow.begin(pending, 0, false, stage);
        flow
    }

    #[test]
    fn the_model_prefix_opens_the_model_stage_after_leading_blanks_in_any_case() {
        let flow = ModelFlow::default();
        let query = flow.raw_query(" \t/MODEL gpt").unwrap();
        assert_eq!(
            (query.stage, query.query, query.token_start),
            (ModelStage::Model, "gpt", 9)
        );
        assert!(flow.raw_query("/model").is_none());
        assert!(flow.raw_query("x /model gpt").is_none());
        assert_eq!(flow.raw_query("/model @src").unwrap().query, "@src");
    }

    #[test]
    fn later_stages_anchor_under_their_own_token_after_the_pending_model() {
        let effort = flow(ModelStage::Effort, "openai/gpt-5");
        let query = effort.raw_query("/model openai/gpt-5  hi").unwrap();
        assert_eq!((query.query, query.token_start), ("hi", 21));
        assert!(effort.raw_query("/model openai/gpt-4 hi").is_none());
        let fast = flow(ModelStage::Fast, "openai/gpt-5");
        let query = fast.raw_query("/model openai/gpt-5 high fa").unwrap();
        assert_eq!((query.query, query.token_start), ("fa", 25));
        assert!(fast.raw_query("/model openai/gpt-5 ").is_none());
        assert!(flow(ModelStage::Fast, "").raw_query("/model  x").is_none());
    }

    #[test]
    fn a_dismissed_column_hides_its_query_until_cleared_and_clearing_keeps_the_dismissal() {
        let mut flow = flow(ModelStage::Fast, "m");
        flow.model.index = 4;
        flow.dismissed = true;
        assert!(flow.query("/model m auto ").is_none());
        flow.clear();
        assert_eq!(flow.stage, ModelStage::Model);
        assert!(flow.pending.is_empty());
        assert_eq!(flow.model, Cursor::default());
        assert!(flow.dismissed);
    }

    #[test]
    fn efforts_offer_the_default_first_and_index_back_to_their_values() {
        let efforts = vec!["low".to_owned(), "high".to_owned()];
        let labels: Vec<String> = effort_options(&efforts)
            .iter()
            .map(|effort| effort.display_label().to_owned())
            .collect();
        assert_eq!(labels, ["default", "low", "high"]);
        assert!(effort_options(&[]).is_empty());
        let high = ReasoningEffort::Named("high".to_owned());
        assert_eq!(effort_index(&efforts, &high), 2);
        assert_eq!(effort_index(&efforts, &ReasoningEffort::Auto), 0);
        assert_eq!(effort_at(&efforts, 2), high);
        assert_eq!(effort_at(&efforts, 0), ReasoningEffort::Auto);
        assert_eq!(effort_at(&[], 3), ReasoningEffort::Auto);
    }

    #[test]
    fn explicit_choices_name_a_model_an_effort_and_optionally_a_mode() {
        let pick = |model, effort, fast_mode| ExplicitModel::Pick {
            model,
            effort,
            fast_mode,
        };
        let high = || ReasoningEffort::Named("high".to_owned());
        assert_eq!(
            explicit_model("/model gpt-5 high"),
            pick("gpt-5", high(), None)
        );
        assert_eq!(
            explicit_model(" /MODEL\tgpt-5  high  FAST \n"),
            pick("gpt-5", high(), Some(true))
        );
        assert_eq!(
            explicit_model("/model gpt-5 default normal"),
            pick("gpt-5", ReasoningEffort::Auto, Some(false))
        );
        assert_eq!(explicit_model("/model gpt-5"), ExplicitModel::None);
        assert_eq!(explicit_model("/model gpt-5 very!"), ExplicitModel::None);
        assert_eq!(explicit_model("/models gpt-5 high"), ExplicitModel::None);
        assert_eq!(
            explicit_model("/model gpt-5 high quick"),
            ExplicitModel::Invalid
        );
        assert_eq!(
            explicit_model("/model a high fast d"),
            ExplicitModel::Invalid
        );
        assert_eq!(explicit_model("/model a b c d e"), ExplicitModel::Invalid);
    }

    #[test]
    fn only_a_bare_model_command_at_the_end_of_the_text_opens_the_menu() {
        assert!(is_bare_model_command("/model", 6));
        assert!(is_bare_model_command(" /Model", 7));
        assert!(!is_bare_model_command("/model", 3));
        assert!(!is_bare_model_command("/model ", 7));
        assert!(matches_query("openai/GPT-5", " gpt "));
        assert!(!matches_query("openai/gpt-5", "claude"));
    }
}
