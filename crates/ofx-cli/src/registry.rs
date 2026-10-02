use crate::command_specs::SlashSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashRegistry<'a> {
    commands: &'a [SlashSpec],
}

impl<'a> SlashRegistry<'a> {
    pub(crate) const fn new(commands: &'a [SlashSpec]) -> Self {
        Self { commands }
    }

    pub fn commands(&self) -> &'a [SlashSpec] {
        self.commands
    }
}
