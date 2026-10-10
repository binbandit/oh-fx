use ofx_contract::{
    Completion, ConversationLog, DeliveryOutcome, LogFailure, ProviderError, ProviderErrorKind,
    RequestTicket,
};

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
        let begun = log.begin_request();
        let admitted = begun.is_ok();
        self.begun = Some(begun);
        admitted
    }

    pub(super) fn settle(
        self,
        streamed: &Result<Completion, ProviderError>,
    ) -> Result<(), LogFailure> {
        match (self.log, self.begun) {
            (Some(log), Some(Ok(ticket))) => log.finish_request(ticket, delivery_outcome(streamed)),
            (_, Some(Err(failure))) => Err(failure),
            _ => Ok(()),
        }
    }
}

fn delivery_outcome(streamed: &Result<Completion, ProviderError>) -> DeliveryOutcome {
    match streamed {
        Ok(_) => DeliveryOutcome::PossiblyBilledWithoutIdentity,
        Err(error) if error.status.is_some() || never_sent(error.kind) => DeliveryOutcome::Unbilled,
        Err(_) => DeliveryOutcome::AmbiguousDelivery,
    }
}

fn never_sent(kind: ProviderErrorKind) -> bool {
    matches!(
        kind,
        ProviderErrorKind::ConnectionFailed | ProviderErrorKind::ConnectivityLost
    )
}
