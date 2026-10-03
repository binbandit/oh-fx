pub trait Clipboard: Send + Sync {
    fn copy(&self, text: &str) -> bool;
}

pub trait SteeringQueue: Send {
    fn retract_newest(&self) -> Option<(u64, String)>;
}
