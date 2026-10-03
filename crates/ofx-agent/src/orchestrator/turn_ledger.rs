use ofx_contract::HistoryCut;

use crate::execution_memory::Cut;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TurnRecord {
    Saved,
    Unsaved,
    LogOnly,
}

#[derive(Debug, Default)]
pub(super) struct TurnLedger {
    pub(super) records: Vec<TurnRecord>,
}

impl TurnLedger {
    pub(super) fn reset(&mut self, saved_turns: usize) {
        self.records.clear();
        self.records.resize(saved_turns, TurnRecord::Saved);
    }

    pub(super) fn push(&mut self, record: TurnRecord) {
        self.records.push(record);
    }

    pub(super) fn logged_cut(&self, cut: Cut) -> HistoryCut {
        let covered = self.covered(cut.turns);
        let turns = self.records[..covered]
            .iter()
            .filter(|record| **record != TurnRecord::Unsaved)
            .count();
        let (tool_steps, steering) = match self.records.get(covered) {
            Some(TurnRecord::Unsaved) => (0, 0),
            _ => (cut.tool_steps, cut.steering),
        };
        HistoryCut {
            turns,
            tool_steps,
            steering,
        }
    }

    pub(super) fn compact(&mut self, cut: Cut) {
        let covered = self.covered(cut.turns);
        self.records.drain(..covered);
    }

    fn covered(&self, turns: usize) -> usize {
        self.records
            .iter()
            .enumerate()
            .filter(|(_, record)| **record != TurnRecord::LogOnly)
            .nth(turns)
            .map_or(self.records.len(), |(index, _)| index)
    }
}

#[cfg(test)]
mod tests;
