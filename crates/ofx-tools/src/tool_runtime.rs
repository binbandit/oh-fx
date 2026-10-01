use std::panic;

use ofx_contract::{BoxFuture, CallDescription, PreparedCall, ToolContext, ToolOutput};

type Run = Box<dyn FnOnce() -> ToolOutput + Send>;

pub(crate) struct BlockingCall {
    description: CallDescription,
    run: Run,
}

impl BlockingCall {
    pub(crate) fn boxed(
        description: CallDescription,
        run: impl FnOnce() -> ToolOutput + Send + 'static,
    ) -> Box<dyn PreparedCall> {
        Box::new(Self {
            description,
            run: Box::new(run),
        })
    }
}

impl PreparedCall for BlockingCall {
    fn describe(&self) -> CallDescription {
        self.description.clone()
    }

    fn execute(self: Box<Self>, _context: ToolContext) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            match tokio::task::spawn_blocking(self.run).await {
                Ok(output) => output,
                Err(error) => panic::resume_unwind(error.into_panic()),
            }
        })
    }
}
