use std::fs::{self, File, ReadDir};
use std::io::{self, Read};
use std::str::FromStr;

use rustix::io::Errno;
use rustix::process::Pid;

use super::{Identity, InspectionError, ProcessSnapshot};
use crate::command_runner::error_name;

const STAT_BYTES: usize = 4096;
const THREADS_AFTER_PARENT: usize = 15;
const START_TICKS_AFTER_THREADS: usize = 1;
const IDENTITY_UNAVAILABLE: InspectionError = InspectionError::Failed("ProcessIdentityUnavailable");
const INSPECTION_FAILED: InspectionError = InspectionError::Failed("ProcessTreeInspectionFailed");

pub(super) fn capture_snapshot(pid: Pid) -> Result<ProcessSnapshot, InspectionError> {
    let mut file =
        open_file(&format!("/proc/{pid}/stat"))?.ok_or(InspectionError::ProcessNotFound)?;
    let mut buffer = [0; STAT_BYTES];
    let length = loop {
        match file.read(&mut buffer) {
            Ok(0) => return Err(InspectionError::ProcessNotFound),
            Ok(length) => break length,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if errno(&error) == Some(Errno::SRCH) => {
                return Err(InspectionError::ProcessNotFound);
            }
            Err(_) => return Err(IDENTITY_UNAVAILABLE),
        }
    };
    parse_stat(&buffer[..length]).ok_or(IDENTITY_UNAVAILABLE)
}

pub(super) fn tasks(pid: Pid) -> Result<Vec<Pid>, InspectionError> {
    let Some(entries) = open_directory(&format!("/proc/{pid}/task"))? else {
        return Ok(Vec::new());
    };
    let mut tasks = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => tasks.extend(entry.file_name().to_str().and_then(parse_pid)),
            Err(error) if vanished(&error) => break,
            Err(error) => return Err(InspectionError::Failed(error_name(&error))),
        }
    }
    Ok(tasks)
}

pub(super) fn task_children(pid: Pid, task: Pid) -> Result<Option<Vec<Pid>>, InspectionError> {
    let Some(mut file) = open_file(&format!("/proc/{pid}/task/{task}/children"))? else {
        return Ok(None);
    };
    let mut listed = String::new();
    match file.read_to_string(&mut listed) {
        Ok(_) => Ok(Some(
            listed
                .split_ascii_whitespace()
                .filter_map(parse_pid)
                .collect(),
        )),
        Err(error) if vanished(&error) => Ok(None),
        Err(_) => Err(INSPECTION_FAILED),
    }
}

fn parse_stat(stat: &[u8]) -> Option<ProcessSnapshot> {
    let name_end = stat.iter().rposition(|&byte| byte == b')')?;
    let mut fields = stat[name_end + 1..]
        .split(|&byte| byte == b' ')
        .filter(|field| !field.is_empty());
    let state = fields.next()?;
    let parent_pid = parse_field::<i32>(fields.next()?)?;
    let threads = parse_field::<u32>(fields.nth(THREADS_AFTER_PARENT)?)?;
    let start_ticks = parse_field::<u64>(fields.nth(START_TICKS_AFTER_THREADS)?)?;
    Some(ProcessSnapshot {
        identity: Identity { start_ticks },
        parent_pid: positive_pid(parent_pid),
        zombie: state == b"Z" && threads <= 1,
    })
}

fn parse_field<T: FromStr>(field: &[u8]) -> Option<T> {
    std::str::from_utf8(field).ok()?.parse().ok()
}

fn parse_pid(text: &str) -> Option<Pid> {
    text.parse().ok().and_then(positive_pid)
}

fn positive_pid(raw: i32) -> Option<Pid> {
    if raw > 0 { Pid::from_raw(raw) } else { None }
}

fn open_file(path: &str) -> Result<Option<File>, InspectionError> {
    match File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if vanished(&error) => Ok(None),
        Err(error) => Err(InspectionError::Failed(error_name(&error))),
    }
}

fn open_directory(path: &str) -> Result<Option<ReadDir>, InspectionError> {
    match fs::read_dir(path) {
        Ok(entries) => Ok(Some(entries)),
        Err(error) if vanished(&error) => Ok(None),
        Err(error) => Err(InspectionError::Failed(error_name(&error))),
    }
}

fn vanished(error: &io::Error) -> bool {
    matches!(errno(error), Some(Errno::NOENT | Errno::SRCH))
}

fn errno(error: &io::Error) -> Option<Errno> {
    error.raw_os_error().map(Errno::from_raw_os_error)
}

#[cfg(test)]
mod tests {
    use super::{open_directory, open_file, parse_stat};
    use crate::process_tree::{Identity, ProcessSnapshot};

    #[test]
    fn linux_proc_helpers_treat_missing_process_data_as_vanished() {
        assert!(matches!(
            open_directory("/proc/self/oh-fx-process-tree-missing"),
            Ok(None)
        ));
        assert!(matches!(
            open_file("/proc/self/oh-fx-process-tree-missing"),
            Ok(None)
        ));
    }

    #[test]
    fn stat_parsing_reads_fields_after_the_last_parenthesis() {
        let stat = b"42 (a) b) Z 7 42 42 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 98765 1 1 \n";
        assert_eq!(
            parse_stat(stat),
            Some(ProcessSnapshot {
                identity: Identity { start_ticks: 98765 },
                parent_pid: rustix::process::Pid::from_raw(7),
                zombie: true,
            })
        );
        assert_eq!(parse_stat(b"42 (sh) S 0 42"), None);
        assert_eq!(parse_stat(b"42 sh S 1"), None);
    }

    #[test]
    fn stat_parsing_counts_a_zombie_leader_with_live_threads_as_running() {
        let leader = b"42 (worker) Z 7 42 42 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 2 0 98765 1 1 \n";
        let exited = b"42 (worker) Z 7 42 42 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 98765 1 1 \n";
        let released = b"42 (worker) Z 7 42 42 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 0 0 98765 1 1 \n";
        assert_eq!(
            parse_stat(leader).map(|snapshot| snapshot.zombie),
            Some(false)
        );
        assert_eq!(
            parse_stat(exited).map(|snapshot| snapshot.zombie),
            Some(true)
        );
        assert_eq!(
            parse_stat(released).map(|snapshot| snapshot.zombie),
            Some(true)
        );
    }
}
