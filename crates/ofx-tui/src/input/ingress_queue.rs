use zeroize::Zeroize;

#[derive(Debug, Default)]
pub(super) struct IngressQueue {
    bytes: Vec<u8>,
    next: usize,
}

impl IngressQueue {
    pub(super) fn is_empty(&self) -> bool {
        self.next == self.bytes.len()
    }

    pub(super) fn push(&mut self, incoming: &[u8]) {
        if self.is_empty() {
            self.bytes.clear();
            self.next = 0;
        }
        if self.bytes.capacity() - self.bytes.len() < incoming.len() {
            self.grow(incoming.len());
        }
        self.bytes.extend_from_slice(incoming);
    }

    pub(super) fn pop(&mut self) -> Option<u8> {
        let byte = std::mem::take(self.bytes.get_mut(self.next)?);
        self.next += 1;
        Some(byte)
    }

    #[cfg(test)]
    pub(super) fn storage(&self) -> &[u8] {
        &self.bytes
    }

    fn grow(&mut self, additional: usize) {
        let pending = &self.bytes[self.next..];
        let needed = pending.len().saturating_add(additional);
        let mut grown = Vec::with_capacity(needed.max(self.bytes.capacity().saturating_mul(2)));
        grown.extend_from_slice(pending);
        self.bytes.zeroize();
        self.bytes = grown;
        self.next = 0;
    }
}

impl Drop for IngressQueue {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(queue: &mut IngressQueue) -> Vec<u8> {
        std::iter::from_fn(|| queue.pop()).collect()
    }

    #[test]
    fn consumed_bytes_are_zeroed_in_place_before_the_storage_is_reused() {
        let mut queue = IngressQueue::default();
        queue.push(b"code");
        assert_eq!(queue.pop(), Some(b'c'));
        assert_eq!(queue.pop(), Some(b'o'));
        assert_eq!(queue.storage(), b"\0\0de");
        assert_eq!(drain(&mut queue), b"de");
        assert!(queue.is_empty());
        assert_eq!(queue.storage(), [0; 4]);

        let capacity = queue.bytes.capacity();
        queue.push(b"ab");
        assert_eq!(queue.storage(), b"ab");
        assert_eq!(queue.bytes.capacity(), capacity);
    }

    #[test]
    fn growing_keeps_pending_bytes_in_order_and_wipes_the_old_storage() {
        let mut queue = IngressQueue::default();
        queue.push(b"abc");
        assert_eq!(queue.pop(), Some(b'a'));
        queue.push(&[b'x'; 64]);
        assert_eq!(&queue.storage()[..2], b"bc");
        assert_eq!(queue.next, 0);
        let mut expected = b"bc".to_vec();
        expected.extend_from_slice(&[b'x'; 64]);
        assert_eq!(drain(&mut queue), expected);
        assert!(queue.storage().iter().all(|byte| *byte == 0));
    }

    #[test]
    fn pushing_onto_pending_bytes_appends_without_reordering() {
        let mut queue = IngressQueue::default();
        queue.push(b"abcd");
        assert_eq!(queue.pop(), Some(b'a'));
        queue.push(b"e");
        assert_eq!(drain(&mut queue), b"bcde");
    }
}
