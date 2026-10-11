use serde::{Deserialize, Serialize};

pub(crate) const MAX_COMMAND_BYTES: usize = 64 * 1024;
pub(crate) const MAX_AUTHORITY_TEXT_BYTES: usize = 4096;
pub(crate) const MAX_SHELL_PATH_BYTES: usize = 4096;
const MAX_DIMENSION: u16 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Dimensions {
    pub(crate) rows: u16,
    pub(crate) columns: u16,
}

impl Dimensions {
    pub(crate) fn is_valid(self) -> bool {
        self.rows != 0
            && self.columns != 0
            && self.rows <= MAX_DIMENSION
            && self.columns <= MAX_DIMENSION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimensions_accept_only_nonzero_sizes_up_to_the_maximum() {
        for (rows, columns, valid) in [
            (24, 80, true),
            (1, 1, true),
            (4096, 4096, true),
            (0, 80, false),
            (24, 0, false),
            (4097, 80, false),
            (24, 4097, false),
        ] {
            assert_eq!(
                Dimensions { rows, columns }.is_valid(),
                valid,
                "{rows}x{columns}"
            );
        }
    }
}
