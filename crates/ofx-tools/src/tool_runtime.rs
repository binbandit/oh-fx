use std::panic;

use ofx_contract::{BoxFuture, CallDescription, PathAccess, PreparedCall, ToolContext, ToolOutput};

type Run = Box<dyn FnOnce(PathAccess) -> ToolOutput + Send>;

pub(crate) struct BlockingCall {
    description: CallDescription,
    run: Run,
}

impl BlockingCall {
    pub(crate) fn boxed(
        description: CallDescription,
        run: impl FnOnce(PathAccess) -> ToolOutput + Send + 'static,
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

    fn execute(self: Box<Self>, context: ToolContext) -> BoxFuture<'static, ToolOutput> {
        let path_access = context.path_access;
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || (self.run)(path_access)).await {
                Ok(output) => output,
                Err(error) => panic::resume_unwind(error.into_panic()),
            }
        })
    }
}
