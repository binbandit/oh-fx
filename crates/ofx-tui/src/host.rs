pub trait Clipboard: Send + Sync {
    fn copy(&self, text: &str) -> bool;
}
