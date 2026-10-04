pub trait Clipboard: Send + Sync {
    fn copy(&self, text: &str) -> bool;
}

#[derive(Clone, Copy)]
pub enum ForegroundState {
    Idle,
    Working,
    Blocked,
}

pub trait ForegroundLifecycle: Send + Sync {
    fn report(&self, state: ForegroundState, status: Option<&[u8]>);
    fn shutdown(&self);
}

pub trait SteeringQueue: Send {
    fn retract_newest(&self) -> Option<(u64, String)>;
}
