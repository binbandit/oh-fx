const CAPACITY: usize = 96;

#[derive(Clone, Copy)]
pub(crate) struct ForwardedBytes {
    buffer: [u8; CAPACITY],
    len: usize,
}

impl ForwardedBytes {
    pub(crate) fn single(byte: u8) -> Self {
        let mut forwarded = Self::from_slice(&[]);
        forwarded.push(byte);
        forwarded
    }

    pub(crate) fn from_slice(bytes: &[u8]) -> Self {
        let mut forwarded = Self {
            buffer: [0; CAPACITY],
            len: 0,
        };
        forwarded.extend_from_slice(bytes);
        forwarded
    }

    pub(crate) fn push(&mut self, byte: u8) {
        self.buffer[self.len] = byte;
        self.len += 1;
    }

    pub(crate) fn extend_from_slice(&mut self, bytes: &[u8]) {
        let end = self.len + bytes.len();
        self.buffer[self.len..end].copy_from_slice(bytes);
        self.len = end;
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.buffer[..self.len]
    }
}

impl std::fmt::Debug for ForwardedBytes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_list().entries(self.as_slice()).finish()
    }
}

impl PartialEq for ForwardedBytes {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for ForwardedBytes {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarded_bytes_hold_the_longest_probe_forward_inline() {
        let mut forwarded = ForwardedBytes::from_slice(&[b'a'; 16]);
        forwarded.extend_from_slice(&[b'b'; 64]);
        forwarded.push(b'c');
        assert_eq!(forwarded.as_slice().len(), 81);
        assert_eq!(forwarded.as_slice()[80], b'c');
        assert_eq!(ForwardedBytes::single(b'x').as_slice(), b"x");
        assert_eq!(
            ForwardedBytes::from_slice(b"ab"),
            ForwardedBytes::from_slice(b"ab")
        );
    }
}
