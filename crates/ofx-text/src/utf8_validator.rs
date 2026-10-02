#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidUtf8;

#[derive(Debug, Clone, Default)]
pub struct Utf8Validator {
    pending: [u8; 4],
    pending_len: usize,
}

impl Utf8Validator {
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), InvalidUtf8> {
        let mut rest = bytes;
        if self.pending_len > 0 {
            let taken = self.missing_bytes().min(rest.len());
            self.pending[self.pending_len..self.pending_len + taken]
                .copy_from_slice(&rest[..taken]);
            self.pending_len += taken;
            rest = &rest[taken..];
            if incomplete_tail(&self.pending[..self.pending_len])? > 0 {
                return Ok(());
            }
            self.pending_len = 0;
        }
        let tail = incomplete_tail(rest)?;
        self.pending[..tail].copy_from_slice(&rest[rest.len() - tail..]);
        self.pending_len = tail;
        Ok(())
    }

    pub fn missing_bytes(&self) -> usize {
        if self.pending_len == 0 {
            return 0;
        }
        let sequence_len = match self.pending[0] {
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            _ => 4,
        };
        sequence_len - self.pending_len
    }

    pub fn finish(&self) -> Result<(), InvalidUtf8> {
        if self.pending_len == 0 {
            Ok(())
        } else {
            Err(InvalidUtf8)
        }
    }
}

fn incomplete_tail(bytes: &[u8]) -> Result<usize, InvalidUtf8> {
    match std::str::from_utf8(bytes) {
        Ok(_) => Ok(0),
        Err(error) if error.error_len().is_none() => Ok(bytes.len() - error.valid_up_to()),
        Err(_) => Err(InvalidUtf8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(chunks: &[&[u8]]) -> Result<(), InvalidUtf8> {
        let mut validator = Utf8Validator::default();
        for chunk in chunks {
            validator.push(chunk)?;
        }
        validator.finish()
    }

    #[test]
    fn utf8_validation_carries_sequences_split_across_chunks() {
        let text = "a\u{e9}\u{20ac}\u{1f600}z".as_bytes();
        for split in 0..=text.len() {
            assert_eq!(
                validate(&[&text[..split], &text[split..]]),
                Ok(()),
                "{split}"
            );
        }
        let bytes: Vec<&[u8]> = text.chunks(1).collect();
        assert_eq!(validate(&bytes), Ok(()));
        assert_eq!(validate(&[b"", b"plain", b""]), Ok(()));
    }

    #[test]
    fn utf8_validation_rejects_invalid_and_unfinished_sequences() {
        assert_eq!(validate(&[b"a\xe2", b"x"]), Err(InvalidUtf8));
        assert_eq!(validate(&[b"\xff"]), Err(InvalidUtf8));
        assert_eq!(validate(&[b"ok\xe2\x82"]), Err(InvalidUtf8));
        assert_eq!(validate(&[b"\xf0", b"\x9f", b"\x98"]), Err(InvalidUtf8));
    }

    #[test]
    fn utf8_validation_reports_the_bytes_an_unfinished_sequence_still_needs() {
        let mut validator = Utf8Validator::default();
        validator.push(b"one\xe2").unwrap();
        assert_eq!(validator.missing_bytes(), 2);
        validator.push(b"\x82").unwrap();
        assert_eq!(validator.missing_bytes(), 1);
        validator.push(b"\xac").unwrap();
        assert_eq!(validator.missing_bytes(), 0);
        assert_eq!(validator.finish(), Ok(()));
    }
}
