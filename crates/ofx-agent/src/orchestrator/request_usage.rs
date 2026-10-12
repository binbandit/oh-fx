use ofx_contract::{
    Completion, ConversationLog, DeliveryOutcome, GenerationIdentity, LogFailure, ProviderError,
    ProviderErrorKind, RequestTicket,
};
use ofx_trace::trace_log;

pub(super) struct RequestUsage<'a> {
    log: Option<&'a dyn ConversationLog>,
    begun: Option<Result<RequestTicket, LogFailure>>,
}

impl<'a> RequestUsage<'a> {
    pub(super) fn new(log: Option<&'a dyn ConversationLog>) -> Self {
        Self { log, begun: None }
    }

    pub(super) fn admit(&mut self) -> bool {
        let Some(log) = self.log else {
            return true;
        };
        if let Some(begun) = &self.begun {
            return begun.is_ok();
        }
        let begun = log.begin_request();
        let admitted = begun.is_ok();
        self.begun = Some(begun);
        admitted
    }

    pub(super) fn settle(
        self,
        streamed: &Result<Completion, ProviderError>,
    ) -> Result<(), LogFailure> {
        let (Some(log), Some(begun)) = (self.log, self.begun) else {
            return Ok(());
        };
        let ticket = begun?;
        let completion = match streamed {
            Ok(completion) => completion,
            Err(error) => return log.finish_request(ticket, delivery_outcome(error)),
        };
        let unidentified = DeliveryOutcome::PossiblyBilledWithoutIdentity;
        match (&completion.generation, &completion.billing) {
            (Some(GenerationIdentity::Invalid), _) => {
                log.finish_request(ticket, unidentified)?;
                trace_log!(
                    "session",
                    "usage generation ignored sequence={} metadata_invalid=true",
                    ticket.sequence
                );
                Ok(())
            }
            (_, Some(billing)) => log.finish_exact_request(ticket, billing),
            (Some(GenerationIdentity::Observed(generation)), None) => {
                log.finish_deferred_request(ticket, generation)
            }
            (None, None) => {
                log.finish_request(ticket, unidentified)?;
                trace_log!(
                    "session",
                    "usage billing incomplete sequence={} reason=generation_identity_missing",
                    ticket.sequence
                );
                Ok(())
            }
        }
    }
}

fn delivery_outcome(error: &ProviderError) -> DeliveryOutcome {
    if error.status.is_some() || never_sent(error.kind) {
        DeliveryOutcome::Unbilled
    } else {
        DeliveryOutcome::AmbiguousDelivery
    }
}

fn never_sent(kind: ProviderErrorKind) -> bool {
    matches!(
        kind,
        ProviderErrorKind::ConnectionFailed | ProviderErrorKind::ConnectivityLost
    )
}
