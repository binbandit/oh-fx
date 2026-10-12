use std::io::Write;

use super::*;
use crate::session_manager::test_support::{fixture_log, line};

fn file_with(bytes: &[u8]) -> (tempfile::TempDir, File) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("log.jsonl");
    File::create(&path).unwrap().write_all(bytes).unwrap();
    let file = File::open(&path).unwrap();
    (directory, file)
}

fn read_all(bytes: &[u8]) -> (Vec<u64>, Option<ReadError>) {
    let (_directory, file) = file_with(bytes);
    let end = last_line_end(&file, bytes.len() as u64).unwrap();
    let mut reader = ForwardReader::new(&file, 0, end, Some(1));
    let mut seqs = Vec::new();
    loop {
        match reader.next() {
            Ok(Some(line)) => seqs.push(line.header.seq),
            Ok(None) => return (seqs, None),
            Err(error) => return (seqs, Some(error)),
        }
    }
}

fn three_lines() -> Vec<u8> {
    [
        line(1, "turn_started", ",\"turn\":1"),
        line(2, "turn_committed", ",\"turn\":1"),
        line(3, "closed", ""),
    ]
    .concat()
}

#[test]
fn the_forward_reader_gives_every_line_of_a_log_fx_wrote() {
    let log = fixture_log("snapshot_small", "PsjMeU3Vl5Ld");
    let (seqs, error) = read_all(&log);
    assert_eq!(error, None);
    assert_eq!(seqs, (1..=25).collect::<Vec<u64>>());
}

#[test]
fn the_forward_reader_stops_before_a_torn_tail() {
    let mut bytes = three_lines();
    bytes.extend_from_slice(b"{\"v\":1,\"seq\":4");
    assert_eq!(read_all(&bytes), (vec![1, 2, 3], None));
    let mut bad_last = three_lines();
    let at = bad_last.len() - 5;
    bad_last[at] ^= 1;
    assert_eq!(read_all(&bad_last), (vec![1, 2], None));
}

#[test]
fn the_forward_reader_reports_a_damaged_middle_line_and_a_seq_gap() {
    let mut damaged = three_lines();
    let second = line(1, "turn_started", ",\"turn\":1").len();
    damaged[second + 3] ^= 1;
    assert_eq!(
        read_all(&damaged),
        (vec![1], Some(ReadError::Corrupt(second as u64)))
    );
    let gap = [
        line(1, "turn_started", ",\"turn\":1"),
        line(3, "closed", ""),
    ]
    .concat();
    assert_eq!(
        read_all(&gap),
        (vec![1], Some(ReadError::Corrupt(second as u64)))
    );
}

#[test]
fn the_last_line_end_ignores_a_partial_line() {
    let mut bytes = three_lines();
    let whole = bytes.len() as u64;
    bytes.extend_from_slice(&[b'x'; 9000]);
    let (_directory, file) = file_with(&bytes);
    assert_eq!(last_line_end(&file, bytes.len() as u64), Ok(whole));
    let (_empty, file) = file_with(b"");
    assert_eq!(last_line_end(&file, 0), Ok(0));
}
