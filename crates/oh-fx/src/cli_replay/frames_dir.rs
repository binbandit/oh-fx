use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

use ofx_workspace::PathError;

use super::push_json;

const INPUT_PROMPTS: [&str; 2] = ["❯", ">"];
const DIVIDERS: [&str; 3] = ["──", "━━", "══"];
pub(super) const FRAMES_DIR: &str = "frames";

pub(super) fn prepare_frames_dir(root: &Path) -> Result<(), PathError> {
    make_dir_recursive(root)?;
    make_dir_recursive(&root.join(FRAMES_DIR))?;
    Ok(())
}

fn make_dir_recursive(path: &Path) -> Result<(), PathError> {
    if path.is_relative() {
        return create_dir_path(path).map_err(|error| PathError::from(&error));
    }
    match fs::create_dir(path) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
            if let Some(parent) = path.parent() {
                make_dir_recursive(parent)?;
            }
            fs::create_dir(path).map_err(|error| PathError::from(&error))
        }
        _ => Ok(()),
    }
}

fn create_dir_path(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    match create_missing_dir(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .ok_or(error)?;
            create_dir_path(parent)?;
            create_missing_dir(path)
        }
        result => result,
    }
}

fn create_missing_dir(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if fs::metadata(path)?.is_dir() {
                Ok(())
            } else {
                Err(io::ErrorKind::NotADirectory.into())
            }
        }
        result => result,
    }
}

pub(super) fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = File::create(path).map_err(|error| PathError::from(&error).to_string())?;
    file.write_all(bytes)
        .map_err(|error| crate::write_error_name(&error).to_owned())
}

pub(super) fn push_footer_candidates(out: &mut String, snapshot: &[u8]) {
    let lines: Vec<&[u8]> = snapshot
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect();
    let footers = lines.windows(3).enumerate().filter(|(_, rows)| {
        is_input_row(rows[1]) && is_divider_row(rows[0]) && is_divider_row(rows[2])
    });
    out.push('[');
    for (position, (top, _)) in footers.enumerate() {
        if position > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"top_divider\":{},\"input\":{},\"bottom_divider\":{}}}",
            top + 1,
            top + 2,
            top + 3
        );
    }
    out.push(']');
}

pub(super) fn push_visible_markers(out: &mut String, snapshot: &[u8], markers: &[&[u8]]) {
    let visible = markers
        .iter()
        .filter(|marker| !marker.is_empty() && contains(snapshot, marker));
    out.push('[');
    for (position, marker) in visible.enumerate() {
        if position > 0 {
            out.push(',');
        }
        push_json(out, marker);
    }
    out.push(']');
}

fn is_input_row(line: &[u8]) -> bool {
    let text = row_text(line);
    INPUT_PROMPTS.iter().any(|prompt| {
        text.starts_with(prompt.as_bytes())
            || (text.starts_with(b"[") && contains(text, format!("] {prompt}").as_bytes()))
    })
}

fn is_divider_row(line: &[u8]) -> bool {
    let text = row_text(line);
    DIVIDERS
        .iter()
        .any(|divider| contains(text, divider.as_bytes()))
}

fn row_text(line: &[u8]) -> &[u8] {
    let inner = match line {
        [b'|', inner @ .., b'|'] => inner,
        _ => line,
    };
    let kept = inner.len() - inner.iter().rev().take_while(|&&byte| byte == b' ').count();
    &inner[..kept]
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
