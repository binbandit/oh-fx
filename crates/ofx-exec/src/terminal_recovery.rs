use crate::terminal_contracts::Lifecycle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    RetainLive,
    RetainFinal,
    FinalizeExited,
    MarkLost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Decision {
    pub(crate) disposition: Disposition,
    pub(crate) lifecycle: Lifecycle,
}

pub(crate) fn reconcile(lifecycle: Lifecycle, termination_present: bool, owned: bool) -> Decision {
    let (disposition, lifecycle) = if !lifecycle.is_live() {
        (Disposition::RetainFinal, lifecycle)
    } else if termination_present {
        (Disposition::FinalizeExited, Lifecycle::Exited)
    } else if owned {
        (Disposition::RetainLive, lifecycle)
    } else {
        (Disposition::MarkLost, Lifecycle::Lost)
    };
    Decision {
        disposition,
        lifecycle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_retains_only_terminals_this_process_owns() {
        assert_eq!(
            reconcile(Lifecycle::Running, false, true),
            Decision {
                disposition: Disposition::RetainLive,
                lifecycle: Lifecycle::Running
            }
        );
        for lifecycle in [Lifecycle::Starting, Lifecycle::Running] {
            assert_eq!(
                reconcile(lifecycle, false, false),
                Decision {
                    disposition: Disposition::MarkLost,
                    lifecycle: Lifecycle::Lost
                }
            );
        }
    }

    #[test]
    fn recovery_finalizes_durable_termination_and_preserves_completed_records() {
        for owned in [false, true] {
            assert_eq!(
                reconcile(Lifecycle::Running, true, owned),
                Decision {
                    disposition: Disposition::FinalizeExited,
                    lifecycle: Lifecycle::Exited
                }
            );
        }
        for lifecycle in [Lifecycle::Exited, Lifecycle::Lost, Lifecycle::Closed] {
            assert_eq!(
                reconcile(lifecycle, false, false),
                Decision {
                    disposition: Disposition::RetainFinal,
                    lifecycle
                }
            );
        }
    }
}
