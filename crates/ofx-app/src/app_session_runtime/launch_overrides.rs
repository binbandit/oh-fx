use ofx_contract::ReasoningEffort;
use ofx_session::SessionPreferences;

pub(crate) struct LaunchOverrides {
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<ReasoningEffort>,
    pub(crate) fast_mode: Option<bool>,
    pub(crate) ultrafast_mode: Option<bool>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RestoredPreferences {
    pub(crate) model: String,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) fast_mode: bool,
    pub(crate) ultrafast_mode: bool,
}

impl LaunchOverrides {
    pub(crate) fn restore(&self, saved: &SessionPreferences) -> RestoredPreferences {
        RestoredPreferences {
            model: self.model.as_ref().unwrap_or(&saved.model).clone(),
            reasoning_effort: self
                .effort
                .as_ref()
                .unwrap_or(&saved.effort)
                .clone()
                .into_named(),
            fast_mode: self.fast_mode.unwrap_or(saved.fast_mode),
            ultrafast_mode: self.ultrafast_mode.unwrap_or(saved.ultrafast_mode),
        }
    }
}

#[cfg(test)]
mod tests {
    use ofx_config::ProviderId;
    use ofx_session::SavedProvider;

    use super::*;

    fn saved() -> SessionPreferences {
        SessionPreferences {
            provider: SavedProvider::new(ProviderId::Gateway, None).unwrap(),
            model: "saved-model".to_owned(),
            effort: ReasoningEffort::parse("high").unwrap(),
            fast_mode: true,
            ultrafast_mode: false,
        }
    }

    #[test]
    fn a_session_brings_back_what_its_launch_flags_left_unset() {
        let unset = LaunchOverrides {
            model: None,
            effort: None,
            fast_mode: None,
            ultrafast_mode: None,
        };
        assert_eq!(
            unset.restore(&saved()),
            RestoredPreferences {
                model: "saved-model".to_owned(),
                reasoning_effort: Some("high".to_owned()),
                fast_mode: true,
                ultrafast_mode: false,
            }
        );
        let flags = LaunchOverrides {
            model: Some("flag-model".to_owned()),
            effort: Some(ReasoningEffort::Auto),
            fast_mode: Some(false),
            ultrafast_mode: None,
        };
        assert_eq!(
            flags.restore(&saved()),
            RestoredPreferences {
                model: "flag-model".to_owned(),
                reasoning_effort: None,
                fast_mode: false,
                ultrafast_mode: false,
            }
        );
    }
}
