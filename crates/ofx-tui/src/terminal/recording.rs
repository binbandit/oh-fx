use std::io;

pub trait TerminalRecorder {
    fn stdout(&self, bytes: &[u8]);
    fn stdin(&self, bytes: &[u8]);
    fn resize(&self, cols: u16, rows: u16);
    fn stop(&self);
}

pub type StartRecording =
    Box<dyn FnOnce(u16, u16) -> io::Result<Option<Box<dyn TerminalRecorder>>>>;
