use std::collections::HashMap;

use crate::styled::Span;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TableColumnAlign {
    #[default]
    Left,
    Right,
    Center,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableRow {
    pub cells: Vec<Vec<Span>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TablePayload {
    pub rows: Vec<TableRow>,
    pub alignments: Vec<TableColumnAlign>,
    pub column_count: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodeBlockPayload {
    pub language: String,
    pub code: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Footnote {
    pub(crate) number: Option<usize>,
    pub(crate) body: String,
    pub(crate) has_definition: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct FootnoteSink {
    pub(crate) notes: Vec<Footnote>,
    pub(crate) next_number: usize,
    index_by_label: HashMap<String, usize>,
    index_by_number: Vec<usize>,
}

impl FootnoteSink {
    pub(crate) fn find_or_append(&mut self, label: &str) -> usize {
        if let Some(&index) = self.index_by_label.get(label) {
            return index;
        }
        self.notes.push(Footnote::default());
        let index = self.notes.len() - 1;
        self.index_by_label.insert(label.to_owned(), index);
        index
    }

    pub(crate) fn register(&mut self, label: &str) -> usize {
        let index = self.find_or_append(label);
        if let Some(number) = self.notes[index].number {
            return number;
        }
        self.next_number += 1;
        self.notes[index].number = Some(self.next_number);
        self.index_by_number.push(index);
        self.next_number
    }

    pub(crate) fn has_numbered_definition(&self) -> bool {
        self.notes
            .iter()
            .any(|note| note.number.is_some() && note.has_definition)
    }

    pub(crate) fn defined_body(&self, number: usize) -> Option<String> {
        let index = *self.index_by_number.get(number.checked_sub(1)?)?;
        let note = &self.notes[index];
        note.has_definition.then(|| note.body.clone())
    }
}
