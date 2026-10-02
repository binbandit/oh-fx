use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

pub(crate) fn read_positional_all(
    file: &File,
    buffer: &mut [u8],
    offset: u64,
) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let position = offset.saturating_add(u64::try_from(filled).unwrap_or(u64::MAX));
        match file.read_at(&mut buffer[filled..], position) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}
