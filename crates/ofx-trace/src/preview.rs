const TRIMMED: [char; 4] = [' ', '\t', '\r', '\n'];
const FIRST_LINE_BYTES: usize = 512;
const ESCAPE: u8 = 0x1b;
const BELL: u8 = 0x07;

pub fn preview(text: &str, max_len: usize) -> &str {
    let trimmed = text.trim_matches(TRIMMED);
    let first_line = trimmed
        .split('\n')
        .next()
        .unwrap_or_default()
        .trim_end_matches('\r');
    &first_line[..first_line.floor_char_boundary(max_len)]
}

pub fn terminal_preview(text: &str, max_bytes: usize) -> String {
    let line = preview(text, FIRST_LINE_BYTES);
    let bytes = line.as_bytes();
    let mut shown = String::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == ESCAPE {
            at = line.ceil_char_boundary(escape_end(bytes, at));
            continue;
        }
        let end = line.ceil_char_boundary(at + 1);
        let replaced = match bytes[at] {
            b'\r' | b'\n' | b'\t' => " ",
            byte if byte < 0x20 || byte == 0x7f => "?",
            _ => &line[at..end],
        };
        if shown.len() + replaced.len() > max_bytes {
            break;
        }
        shown.push_str(replaced);
        at = end;
    }
    shown
}

fn escape_end(bytes: &[u8], start: usize) -> usize {
    match bytes.get(start + 1) {
        None => start + 1,
        Some(b'[') => bytes[start + 2..]
            .iter()
            .position(|byte| (0x40..=0x7e).contains(byte))
            .map_or(bytes.len(), |offset| start + 2 + offset + 1),
        Some(b']') => {
            let mut at = start + 2;
            while at < bytes.len() {
                if bytes[at] == BELL {
                    return at + 1;
                }
                if bytes[at] == ESCAPE && bytes.get(at + 1) == Some(&b'\\') {
                    return at + 2;
                }
                at += 1;
            }
            bytes.len()
        }
        Some(_) => start + 2,
    }
}

#[cfg(test)]
mod tests;
