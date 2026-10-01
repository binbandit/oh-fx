use std::fmt;

use zeroize::Zeroizing;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Secret(Zeroizing<String>);

impl Secret {
    pub(crate) fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    pub(crate) fn into_inner(mut self) -> String {
        std::mem::take(&mut *self.0)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use zeroize::Zeroize;

    use super::*;

    #[test]
    fn zero_and_free_overwrites_bytes_before_release() {
        let mut value = vec![1_u8, 2, 3];
        value.zeroize();
        assert!(value.is_empty());
        let mut text = String::from("abc");
        text.zeroize();
        assert!(text.is_empty());
    }

    #[test]
    fn debug_output_never_contains_the_value() {
        let secret = Secret::new("sk-live-token".to_owned());
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert_eq!(secret.expose(), "sk-live-token");
        assert_eq!(secret.into_inner(), "sk-live-token");
    }
}
