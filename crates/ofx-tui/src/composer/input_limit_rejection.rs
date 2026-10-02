use super::Composer;
use crate::input::TextOwner;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LimitRejection {
    owner: Option<TextOwner>,
}

impl LimitRejection {
    pub(crate) fn begin(&mut self, owner: TextOwner) -> bool {
        if self.owner == Some(owner) {
            return false;
        }
        self.owner = Some(owner);
        true
    }

    pub(crate) fn clear(&mut self) {
        self.owner = None;
    }
}

impl Composer {
    pub(crate) fn note_limit_rejection(&mut self, owner: TextOwner) -> bool {
        self.limit_rejection.begin(owner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_limit_rejection_coalesces_only_within_one_owner_episode() {
        let mut state = LimitRejection::default();
        assert!(state.begin(TextOwner::Composer));
        assert_eq!(state.owner, Some(TextOwner::Composer));

        let before = state;
        assert!(!state.begin(TextOwner::Composer));
        assert_eq!(state, before);
    }

    #[test]
    fn input_limit_rejection_clear_starts_a_new_episode() {
        let mut state = LimitRejection::default();
        state.begin(TextOwner::Composer);
        assert_eq!(state.owner, Some(TextOwner::Composer));
        state.clear();
        assert_eq!(state.owner, None);
        assert!(state.begin(TextOwner::Composer));
    }
}
