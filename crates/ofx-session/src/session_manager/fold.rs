use super::fields::{BadBody, Fields, Tag};
use super::json::{self, Value};
use super::schema::{Body, Outcome, Reason, SetKey};

const SETTINGS: usize = SetKey::ALL.len();

#[derive(Debug, Clone, PartialEq, Eq)]
struct Child {
    id: String,
    work_id: String,
    open: bool,
    outcome: Option<Outcome>,
    spawn_data: Option<Vec<u8>>,
    finish_data: Option<Vec<u8>>,
    seq: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct State {
    open_turn: Option<u64>,
    last_turn: u64,
    committed: u64,
    interrupted: u64,
    last_interrupted: Option<(u64, Reason)>,
    settings: [Option<Vec<u8>>; SETTINGS],
    children: Vec<Child>,
    last_compaction_seq: Option<u64>,
    compaction_offset: Option<u64>,
    clean_exit: bool,
    last_seq: u64,
}

impl State {
    pub(crate) fn apply(&mut self, fork_seq: u64, seq: u64, offset: u64, body: &Body<'_>) {
        match body {
            Body::TurnStarted { turn } => {
                self.open_turn = Some(*turn);
                self.last_turn = self.last_turn.max(*turn);
            }
            Body::Compacted { .. } => {
                self.last_compaction_seq = Some(seq);
                self.compaction_offset = Some(offset);
            }
            Body::TurnCommitted => {
                self.open_turn = None;
                self.committed += 1;
            }
            Body::TurnInterrupted { turn, reason } => {
                self.open_turn = None;
                self.interrupted += 1;
                self.last_interrupted = Some((*turn, *reason));
            }
            Body::Set { key, value, .. } => self.settings[*key as usize] = Some(value.to_vec()),
            Body::ChildSpawned {
                child,
                work_id,
                data,
            } if seq > fork_seq => self.spawn_child(seq, child, work_id, *data),
            Body::ChildFinished {
                child,
                work_id,
                outcome,
                data,
            } if seq > fork_seq => self.finish_child(seq, child, work_id, *outcome, *data),
            Body::SessionCreated { .. }
            | Body::Item { .. }
            | Body::Snapshot { .. }
            | Body::Closed
            | Body::ChildSpawned { .. }
            | Body::ChildFinished { .. } => {}
        }
        self.last_seq = seq;
        self.clean_exit = matches!(body, Body::Closed);
    }

    fn spawn_child(&mut self, seq: u64, child: &str, work_id: &str, data: Option<&[u8]>) {
        let spawn_data = data.map(<[u8]>::to_vec);
        if let Some(known) = self.children.iter_mut().find(|known| known.id == child) {
            work_id.clone_into(&mut known.work_id);
            known.spawn_data = spawn_data;
            known.open = true;
            known.seq = seq;
            return;
        }
        self.children.push(Child {
            id: child.to_owned(),
            work_id: work_id.to_owned(),
            open: true,
            outcome: None,
            spawn_data,
            finish_data: None,
            seq,
        });
    }

    fn finish_child(
        &mut self,
        seq: u64,
        child: &str,
        work_id: &str,
        outcome: Outcome,
        data: Option<&[u8]>,
    ) {
        let Some(known) = self.children.iter_mut().find(|known| known.id == child) else {
            return;
        };
        if !known.open || known.work_id != work_id {
            return;
        }
        known.finish_data = data.map(<[u8]>::to_vec);
        known.open = false;
        known.outcome = Some(outcome);
        known.seq = seq;
    }

    pub(crate) fn clear_children(&mut self) {
        self.children.clear();
    }

    pub(crate) fn decode(raw: &[u8]) -> Result<Self, BadBody> {
        let fields = Fields::parse_object(raw).ok_or(BadBody)?;
        let last_interrupted = match fields.opt_struct("last_interrupted", &["turn", "reason"])? {
            Some(members) => Some((
                Fields::u64_value(members[0])?,
                Fields::tag_value(members[1])?,
            )),
            None => None,
        };
        let mut state = Self {
            open_turn: fields.opt_u64("open_turn")?,
            last_turn: fields.req_u64("last_turn")?,
            committed: fields.req_u64("committed")?,
            interrupted: fields.req_u64("interrupted")?,
            last_interrupted,
            last_compaction_seq: fields.opt_u64("last_compaction_seq")?,
            compaction_offset: fields.opt_u64("compaction_offset")?,
            ..Self::default()
        };
        for key in SetKey::ALL {
            state.settings[*key as usize] = fields.raw(key.name()).map(<[u8]>::to_vec);
        }
        let Some(Value::Array(children)) = json::parse(fields.raw("children").ok_or(BadBody)?)
        else {
            return Err(BadBody);
        };
        for raw_child in children {
            let child = Fields::parse_object(raw_child).ok_or(BadBody)?;
            state.children.push(Child {
                id: child.req_str("id")?.into_owned(),
                work_id: child.req_str("work_id")?.into_owned(),
                spawn_data: child.raw("spawn_data").map(<[u8]>::to_vec),
                finish_data: child.raw("finish_data").map(<[u8]>::to_vec),
                open: child.req_bool("open")?,
                outcome: child.opt_tag("outcome")?,
                seq: child.req_u64("seq")?,
            });
        }
        Ok(state)
    }

    pub(crate) fn at_seq(mut self, seq: u64) -> Self {
        self.last_seq = seq;
        self
    }

    pub(crate) fn skip_unknown(&mut self, seq: u64) {
        self.last_seq = seq;
        self.clean_exit = false;
    }

    pub(crate) fn setting(&self, key: SetKey) -> Option<&[u8]> {
        self.settings[key as usize].as_deref()
    }

    pub(crate) fn with_position_of(mut self, other: &Self) -> Self {
        self.last_seq = other.last_seq;
        self.clean_exit = other.clean_exit;
        self
    }
}

#[cfg(test)]
mod tests;
