use std::io;
use std::os::fd::BorrowedFd;

use rustix::termios::{
    self, ControlModes, InputModes, LocalModes, OptionalActions, SpecialCodeIndex, Termios,
};

pub struct RawInput<'a> {
    input: BorrowedFd<'a>,
    original: Termios,
}

impl<'a> RawInput<'a> {
    pub fn enable(input: BorrowedFd<'a>) -> io::Result<Self> {
        let original = termios::tcgetattr(input)?;
        let mut raw = original.clone();
        raw.input_modes.remove(
            InputModes::BRKINT
                | InputModes::ICRNL
                | InputModes::INPCK
                | InputModes::ISTRIP
                | InputModes::IXON
                | InputModes::IXOFF,
        );
        raw.control_modes.remove(ControlModes::CSIZE);
        raw.control_modes.insert(ControlModes::CS8);
        raw.local_modes
            .remove(LocalModes::ECHO | LocalModes::ICANON | LocalModes::IEXTEN | LocalModes::ISIG);
        raw.special_codes[SpecialCodeIndex::VMIN] = 1;
        raw.special_codes[SpecialCodeIndex::VTIME] = 0;
        termios::tcsetattr(input, OptionalActions::Flush, &raw)?;
        Ok(Self { input, original })
    }
}

impl Drop for RawInput<'_> {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(self.input, OptionalActions::Flush, &self.original);
    }
}
