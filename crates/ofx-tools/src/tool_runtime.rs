use std::panic;

use ofx_contract::{
    BoxFuture, CallDescription, FileMutation, PathAccess, PreparedCall, ToolContext, ToolOutput,
};

type Run = Box<dyn FnOnce(ToolContext) -> ToolOutput + Send>;

pub(crate) struct BlockingCall {
    description: CallDescription,
    mutation: Option<FileMutation>,
    run: Run,
}

impl BlockingCall {
    pub(crate) fn boxed(
        description: CallDescription,
        run: impl FnOnce(PathAccess) -> ToolOutput + Send + 'static,
    ) -> Box<dyn PreparedCall> {
        Box::new(Self {
            description,
            mutation: None,
            run: Box::new(move |context: ToolContext| run(context.path_access)),
        })
    }

    pub(crate) fn mutation(
        description: CallDescription,
        mutation: FileMutation,
        run: impl FnOnce(ToolContext) -> ToolOutput + Send + 'static,
    ) -> Box<dyn PreparedCall> {
        Box::new(Self {
            description,
            mutation: Some(mutation),
            run: Box::new(run),
        })
    }
}

impl PreparedCall for BlockingCall {
    fn describe(&self) -> CallDescription {
        self.description.clone()
    }

    fn file_mutation(&self) -> Option<&FileMutation> {
        self.mutation.as_ref()
    }

    fn execute(self: Box<Self>, context: ToolContext) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || (self.run)(context)).await {
                Ok(output) => output,
                Err(error) => panic::resume_unwind(error.into_panic()),
            }
        })
    }
}
