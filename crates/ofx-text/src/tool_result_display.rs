const PATH_OPEN: &str = "<path>";
const PATH_CLOSE: &str = "</path>";
const CONTENT_OPEN: &str = "<content>";
const CONTENT_CLOSE: &str = "</content>";
const LINE_BREAKS: [char; 2] = ['\r', '\n'];

pub fn content_for_display(body: &str) -> &str {
    let mut rest = body;
    if rest.starts_with(PATH_OPEN) {
        let Some(close) = rest.find(PATH_CLOSE) else {
            return body;
        };
        rest = rest[close + PATH_CLOSE.len()..].trim_start_matches(LINE_BREAKS);
    }
    let Some(content) = rest.strip_prefix(CONTENT_OPEN) else {
        return body;
    };
    let Some(inner) = content
        .trim_start_matches(LINE_BREAKS)
        .strip_suffix(CONTENT_CLOSE)
    else {
        return body;
    };
    inner.trim_end_matches(LINE_BREAKS)
}

#[cfg(test)]
mod tests;
