#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) raw_start: usize,
    pub(crate) raw_end: usize,
}

impl Span {
    pub(crate) fn new(raw_start: usize, raw_end: usize) -> Self {
        Self { raw_start, raw_end }
    }

    pub(crate) fn is_valid(self, input_len: usize) -> bool {
        self.raw_start < self.raw_end && self.raw_end <= input_len
    }

    pub(crate) fn contains(self, raw_offset: usize) -> bool {
        self.raw_start < raw_offset && raw_offset < self.raw_end
    }

    pub(crate) fn overlaps(self, other: Self) -> bool {
        self.raw_start < other.raw_end && other.raw_start < self.raw_end
    }

    pub(crate) fn contains_span(self, inner: Self) -> bool {
        self.raw_start <= inner.raw_start && inner.raw_end <= self.raw_end
    }

    pub(crate) fn after_insert(self, raw_offset: usize, byte_len: usize) -> Option<Self> {
        if byte_len == 0 || raw_offset >= self.raw_end {
            return Some(self);
        }
        if raw_offset > self.raw_start {
            return None;
        }
        Some(Self {
            raw_start: self.raw_start.checked_add(byte_len)?,
            raw_end: self.raw_end.checked_add(byte_len)?,
        })
    }

    pub(crate) fn after_delete(self, delete_start: usize, delete_end: usize) -> Option<Self> {
        if delete_start >= delete_end || delete_start >= self.raw_end {
            return Some(self);
        }
        if delete_end <= self.raw_start {
            let removed = delete_end - delete_start;
            return Some(Self {
                raw_start: self.raw_start - removed,
                raw_end: self.raw_end - removed,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_spans_shift_only_for_edits_outside_their_contents() {
        let span = Span::new(4, 10);
        assert_eq!(span.after_insert(4, 2), Some(Span::new(6, 12)));
        assert_eq!(span.after_insert(10, 2), Some(span));
        assert_eq!(span.after_insert(7, 2), None);

        assert_eq!(span.after_delete(1, 3), Some(Span::new(2, 8)));
        assert_eq!(span.after_delete(10, 12), Some(span));
        assert_eq!(span.after_delete(3, 5), None);
        assert_eq!(span.after_delete(5, 7), None);
    }

    #[test]
    fn entity_span_relationship_helpers_use_half_open_ranges() {
        let entity = Span::new(4, 10);
        assert!(entity.is_valid(10));
        assert!(!entity.is_valid(9));
        assert!(!entity.contains(4));
        assert!(entity.contains(5));
        assert!(!entity.contains(10));
        assert!(entity.overlaps(Span::new(9, 12)));
        assert!(!entity.overlaps(Span::new(10, 12)));
        assert!(entity.contains_span(Span::new(5, 9)));
    }
}
