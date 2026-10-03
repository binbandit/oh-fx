use std::fmt::{self, Write as _};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::ExitCode;

use ofx_cli::ReplayArgs;
use ofx_vt::{Grid, GridError};
use ofx_workspace::{PathError, TapeFrame, TapeHeader, TapeKind, TapeParser};
use signal_hook::consts::SIGPIPE;

use frames_dir::{
    FRAMES_DIR, prepare_frames_dir, push_footer_candidates, push_visible_markers, write_file,
};

const MESSAGE_PREFIX: &str = "oh-fx replay: ";
const MAX_TAPE_BYTES: u64 = 64 * 1024 * 1024;
const PATH_MESSAGE_BYTES: usize = 512;
const MESSAGE_BYTES: usize = 256;
const INCOMPLETE_TAIL: &[u8] = b"oh-fx replay: ignored incomplete final tape frame\n";
const MANIFEST: &str = "manifest.json";

trait Output {
    fn stdout(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn stderr(&mut self, bytes: &[u8]) -> io::Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Replayed,
    Failed,
}

#[derive(Debug)]
enum Fault {
    Write(io::Error),
    Grid(GridError),
}

impl fmt::Display for Fault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Write(error) => formatter.write_str(crate::write_error_name(error)),
            Self::Grid(error) => error.fmt(formatter),
        }
    }
}

struct Failure {
    message: Vec<u8>,
    capacity: usize,
    fallback: &'static str,
    code: String,
}

impl Failure {
    fn new(capacity: usize, fallback: &'static str, subject: &[&[u8]], code: String) -> Self {
        let mut message = MESSAGE_PREFIX.as_bytes().to_vec();
        for part in subject {
            message.extend_from_slice(part);
        }
        message.extend_from_slice(b": ");
        message.extend_from_slice(code.as_bytes());
        message.push(b'\n');
        Self {
            message,
            capacity,
            fallback,
            code,
        }
    }

    fn naming(lead: &str, path: &Path, fallback: &'static str, code: String) -> Self {
        Self::new(
            PATH_MESSAGE_BYTES,
            fallback,
            &[lead.as_bytes(), b" ", path.as_os_str().as_bytes()],
            code,
        )
    }

    fn text(&self) -> Vec<u8> {
        if self.message.len() > self.capacity {
            format!("{MESSAGE_PREFIX}{}\n", self.fallback).into_bytes()
        } else {
            self.message.clone()
        }
    }
}

enum Stop {
    Failed(Failure),
    Fault(Fault),
}

impl From<Failure> for Stop {
    fn from(failure: Failure) -> Self {
        Self::Failed(failure)
    }
}

impl From<GridError> for Stop {
    fn from(error: GridError) -> Self {
        Self::Fault(Fault::Grid(error))
    }
}

fn written(result: io::Result<()>) -> Result<(), Stop> {
    result.map_err(|error| Stop::Fault(Fault::Write(error)))
}

struct Terminal;

impl Output for Terminal {
    fn stdout(&mut self, bytes: &[u8]) -> io::Result<()> {
        crate::write_unbuffered(rustix::stdio::stdout(), bytes)
    }

    fn stderr(&mut self, bytes: &[u8]) -> io::Result<()> {
        crate::write_unbuffered(rustix::stdio::stderr(), bytes)
    }
}

struct Replay<'t> {
    grid: Grid,
    frames: usize,
    resizes: usize,
    stdout_bytes: usize,
    elapsed_ms: i64,
    markers: Vec<&'t [u8]>,
    listed: String,
}

impl<'t> Replay<'t> {
    fn new(header: &TapeHeader<'t>) -> Result<Self, GridError> {
        Ok(Self {
            grid: Grid::new(header.cols, header.rows)?,
            frames: 0,
            resizes: 0,
            stdout_bytes: 0,
            elapsed_ms: 0,
            markers: Vec::new(),
            listed: String::new(),
        })
    }

    fn apply(
        &mut self,
        frame: &TapeFrame<'t>,
        args: &ReplayArgs,
        output: &mut impl Output,
    ) -> Result<(), Stop> {
        self.frames += 1;
        self.elapsed_ms += i64::from(frame.delta_ms);
        self.feed(frame)?;
        if args.json {
            self.list(frame);
        }
        if args.frames && frame.kind != TapeKind::Marker {
            let heading = format!(
                "\n--- frame {} ({}, +{}ms) ---\n",
                self.frames,
                frame.kind.name(),
                frame.delta_ms
            );
            written(output.stdout(heading.as_bytes()))?;
            written(output.stdout(&self.grid.snapshot()))?;
        }
        if let Some(dir) = &args.frames_dir {
            let dir = Path::new(dir);
            self.write_artifacts(dir, frame).map_err(|code| {
                Failure::naming(
                    "cannot write frame artifacts to",
                    dir,
                    "cannot write frame artifacts",
                    code,
                )
            })?;
        }
        Ok(())
    }

    fn feed(&mut self, frame: &TapeFrame<'t>) -> Result<(), GridError> {
        match frame.kind {
            TapeKind::Stdout => {
                self.stdout_bytes += frame.payload.len();
                self.grid.feed(frame.payload)?;
            }
            TapeKind::Resize => {
                if let [cols_low, cols_high, rows_low, rows_high, ..] = *frame.payload {
                    self.grid.resize(
                        u16::from_le_bytes([cols_low, cols_high]),
                        u16::from_le_bytes([rows_low, rows_high]),
                    )?;
                    self.resizes += 1;
                }
            }
            TapeKind::Marker => self.markers.push(frame.payload),
            TapeKind::Stdin | TapeKind::Sigint | TapeKind::Unknown(_) => {}
        }
        Ok(())
    }

    fn list(&mut self, frame: &TapeFrame<'t>) {
        if !self.listed.is_empty() {
            self.listed.push(',');
        }
        let _ = write!(
            self.listed,
            "{{\"delta_ms\":{},\"kind\":\"{}\",\"len\":{}}}",
            frame.delta_ms,
            frame.kind.name(),
            frame.payload.len()
        );
    }

    fn counts_json(&self) -> String {
        format!(
            "\"frame_count\":{},\"resize_count\":{},\"stdout_bytes\":{}",
            self.frames, self.resizes, self.stdout_bytes
        )
    }

    fn summary_json(&self, header: &TapeHeader<'_>) -> String {
        format!(
            "{},\"frames\":[{}],{}}}\n",
            header_json(header),
            self.listed,
            self.counts_json()
        )
    }

    fn manifest_json(&self, header: &TapeHeader<'_>) -> String {
        format!(
            "{},{},\"frames_dir\":\"{FRAMES_DIR}\"}}\n",
            header_json(header),
            self.counts_json()
        )
    }

    fn write_artifacts(&self, root: &Path, frame: &TapeFrame<'_>) -> Result<(), String> {
        let snapshot = self.grid.snapshot();
        let frames = root.join(FRAMES_DIR);
        write_file(
            &frames.join(format!("{:04}.json", self.frames)),
            self.frame_json(frame, &snapshot).as_bytes(),
        )?;
        write_file(
            &frames.join(format!("{:04}.grid.txt", self.frames)),
            &snapshot,
        )
    }

    fn frame_json(&self, frame: &TapeFrame<'_>, snapshot: &[u8]) -> String {
        let mut out = format!(
            "{{\"index\":{},\"delta_ms\":{},\"elapsed_ms\":{},\"kind\":\"{}\",\"payload_len\":{},\"size\":{{\"cols\":{},\"rows\":{}}},\"cursor\":{{\"row\":{},\"col\":{},\"visible\":{}}},\"footer_candidates\":",
            self.frames,
            frame.delta_ms,
            self.elapsed_ms,
            frame.kind.name(),
            frame.payload.len(),
            self.grid.cols(),
            self.grid.rows(),
            self.grid.cursor_row(),
            self.grid.cursor_col(),
            self.grid.cursor_visible()
        );
        push_footer_candidates(&mut out, snapshot);
        out.push_str(",\"visible_markers\":");
        push_visible_markers(&mut out, snapshot, &self.markers);
        out.push_str("}\n");
        out
    }
}

pub(crate) fn run(args: &ReplayArgs) -> ExitCode {
    crate::auto_upgrade::announce_and_schedule();
    match replay(args, &mut Terminal) {
        Ok(Status::Replayed) => ExitCode::SUCCESS,
        Ok(Status::Failed) => ExitCode::FAILURE,
        Err(Fault::Write(error)) if error.kind() == io::ErrorKind::BrokenPipe => {
            crate::die_by_signal(SIGPIPE)
        }
        Err(fault) => {
            let _ = Terminal.stderr(format!("oh-fx: {fault}\n").as_bytes());
            ExitCode::FAILURE
        }
    }
}

fn replay(args: &ReplayArgs, output: &mut impl Output) -> Result<Status, Fault> {
    match replay_tape(args, output) {
        Ok(()) => Ok(Status::Replayed),
        Err(Stop::Fault(fault)) => Err(fault),
        Err(Stop::Failed(failure)) => report_failure(output, args.json, &failure),
    }
}

fn replay_tape(args: &ReplayArgs, output: &mut impl Output) -> Result<(), Stop> {
    let bytes = read_tape(Path::new(&args.path))?;
    let mut parser = TapeParser::new(&bytes).map_err(|error| {
        Failure::new(MESSAGE_BYTES, "bad tape", &[b"bad tape"], error.to_string())
    })?;
    let header = parser.header;
    let mut replay = Replay::new(&header)?;
    let frames_dir = args.frames_dir.as_deref().map(Path::new);
    if let Some(dir) = frames_dir {
        prepare_frames_dir(dir).map_err(|error| {
            Failure::naming(
                "cannot prepare frames dir",
                dir,
                "cannot prepare frames dir",
                error.to_string(),
            )
        })?;
    }
    let incomplete_tail = loop {
        match parser.next_frame() {
            Ok(Some(frame)) => replay.apply(&frame, args, output)?,
            Ok(None) => break false,
            Err(_) => break true,
        }
    };
    if args.json {
        written(output.stdout(replay.summary_json(&header).as_bytes()))?;
    }
    if incomplete_tail {
        written(output.stderr(INCOMPLETE_TAIL))?;
    }
    if let Some(dir) = frames_dir {
        write_file(
            &dir.join(MANIFEST),
            replay.manifest_json(&header).as_bytes(),
        )
        .map_err(|code| {
            Failure::naming(
                "cannot write frames manifest to",
                dir,
                "cannot write frames manifest",
                code,
            )
        })?;
    }
    let screen = replay.grid.snapshot();
    if let Some(golden) = &args.golden {
        return write_golden(Path::new(golden), &screen);
    }
    if !args.frames && !args.json {
        written(output.stdout(&screen))?;
    }
    Ok(())
}

fn read_tape(path: &Path) -> Result<Vec<u8>, Failure> {
    let opened = |code: PathError| {
        Failure::new(
            PATH_MESSAGE_BYTES,
            "open failed",
            &[b"cannot open ", path.as_os_str().as_bytes()],
            code.to_string(),
        )
    };
    let unread = |code: &str| {
        Failure::new(
            MESSAGE_BYTES,
            "read failed",
            &[b"read failed"],
            code.to_owned(),
        )
    };
    let file = File::open(path).map_err(|error| opened(PathError::from(&error)))?;
    if file.metadata().is_ok_and(|metadata| metadata.is_dir()) {
        return Err(opened(PathError::IsDir));
    }
    let mut bytes = Vec::new();
    file.take(MAX_TAPE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unread("ReadFailed"))?;
    if u64::try_from(bytes.len()).map_or(true, |len| len > MAX_TAPE_BYTES) {
        return Err(unread("StreamTooLong"));
    }
    Ok(bytes)
}

fn write_golden(path: &Path, screen: &[u8]) -> Result<(), Stop> {
    let mut file = File::create(path).map_err(|error| {
        Failure::new(
            MESSAGE_BYTES,
            "write failed",
            &[b"cannot write ", path.as_os_str().as_bytes()],
            PathError::from(&error).to_string(),
        )
    })?;
    written(file.write_all(screen))
}

fn report_failure(
    output: &mut impl Output,
    json: bool,
    failure: &Failure,
) -> Result<Status, Fault> {
    let message = failure.text();
    let reported = if json {
        let mut line = String::from("{\"kind\":\"replay\",\"error\":");
        push_json(&mut line, without_line_endings(&message));
        line.push_str(",\"code\":");
        push_json(&mut line, failure.code.as_bytes());
        line.push_str("}\n");
        output.stdout(line.as_bytes())
    } else {
        output.stderr(&message)
    };
    match reported {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Err(Fault::Write(error)),
        _ => Ok(Status::Failed),
    }
}

fn without_line_endings(mut bytes: &[u8]) -> &[u8] {
    while let [rest @ .., b'\r' | b'\n'] = bytes {
        bytes = rest;
    }
    bytes
}

fn header_json(header: &TapeHeader<'_>) -> String {
    let mut out = format!(
        "{{\"cols\":{},\"rows\":{},\"epoch_ms\":{},\"version\":",
        header.cols, header.rows, header.epoch_ms
    );
    push_json(&mut out, header.version);
    out
}

fn push_json(out: &mut String, bytes: &[u8]) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        out.push('[');
        for (position, byte) in bytes.iter().enumerate() {
            if position > 0 {
                out.push(',');
            }
            let _ = write!(out, "{byte}");
        }
        out.push(']');
        return;
    };
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0'..='\u{1f}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(character));
            }
            _ => out.push(character),
        }
    }
    out.push('"');
}

mod frames_dir;
#[cfg(test)]
mod tests;
