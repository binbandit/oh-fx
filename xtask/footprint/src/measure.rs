use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ofx_testkit::{FakeServer, Reply};

use crate::build::{ARCHIVE_FILES, BINARY};
use crate::metric::{Metric, Readings};
use crate::peak;
use crate::repository;
use crate::scenario::{
    ASK_PROMPT, Profile, STREAM_LONG, STREAM_SHORT, TOOL_PROMPT, ask_replies, stream_replies,
    tool_replies,
};

pub(crate) const RUN_TIMEOUT: Duration = Duration::from_mins(5);
pub(crate) const POLL: Duration = Duration::from_millis(1);
const STREAM_RUNS: usize = 3;
const RSS_RUNS: usize = 3;
const CA_STORE_PREFIXES: [&str; 7] = [
    "\"/etc/ssl",
    "\"/etc/pki",
    "\"/etc/openssl",
    "\"/etc/security/certificates",
    "\"/usr/lib/ssl",
    "\"/usr/local/ssl",
    "\"/usr/share/ssl",
];

struct Finished {
    stderr: String,
}

pub(crate) struct Spawned {
    pub(crate) child: Child,
    pub(crate) program: String,
    pub(crate) stderr_path: PathBuf,
}

pub(crate) fn measure(release: &Path) -> Readings {
    let binary = release.join(BINARY);
    let mut readings = Readings::new();
    readings.insert(Metric::StrippedBytes, file_size(&binary));
    match Profile::create() {
        Ok(profile) => {
            readings.insert(Metric::ArchiveBytes, archive_size(&profile, release));
            measure_runs(&profile, &binary, &mut readings);
        }
        Err(error) => {
            for metric in Metric::ALL {
                readings.entry(metric).or_insert_with(|| Err(error.clone()));
            }
        }
    }
    readings
}

fn measure_runs(profile: &Profile, binary: &Path, readings: &mut Readings) {
    let ask = ["ask", ASK_PROMPT];
    let tools = ["ask", TOOL_PROMPT];
    if let Err(error) = settle(profile, binary) {
        for metric in Metric::ALL {
            readings.entry(metric).or_insert_with(|| Err(error.clone()));
        }
        return;
    }
    readings.insert(
        Metric::VersionInstructions,
        instructions(profile, binary, &["--version"]),
    );
    readings.insert(
        Metric::HelpInstructions,
        instructions(profile, binary, &["--help"]),
    );
    readings.insert(
        Metric::AskInstructions,
        served(profile, ask_replies(), || {
            instructions(profile, binary, &ask)
        }),
    );
    readings.insert(
        Metric::StreamDeltaInstructions,
        stream_delta_instructions(profile, binary),
    );
    let ask_trace = served(profile, ask_replies(), || {
        trace(profile, binary, &ask, None)
    });
    readings.insert(
        Metric::CaStoreSyscalls,
        ask_trace
            .as_deref()
            .map(ca_store_syscalls)
            .map_err(Clone::clone),
    );
    readings.insert(
        Metric::AskSyscalls,
        ask_trace.as_deref().map(syscalls).map_err(Clone::clone),
    );
    readings.insert(
        Metric::VersionSyscalls,
        trace(profile, binary, &["--version"], None).map(|log| syscalls(&log)),
    );
    readings.insert(
        Metric::HelpSyscalls,
        trace(profile, binary, &["--help"], None).map(|log| syscalls(&log)),
    );
    readings.insert(
        Metric::ToolGitProcesses,
        served(profile, tool_replies(), || {
            trace(profile, binary, &tools, Some("trace=execve"))
        })
        .map(|log| git_processes(&log)),
    );
    readings.insert(
        Metric::AskPeakRss,
        peak_rss(profile, binary, &ask, ask_replies),
    );
    readings.insert(
        Metric::ToolPeakRss,
        peak_rss(profile, binary, &tools, tool_replies),
    );
    readings.insert(
        Metric::StreamPeakRss,
        peak_rss(profile, binary, &ask, || stream_replies(STREAM_LONG)),
    );
}

fn settle(profile: &Profile, binary: &Path) -> Result<(), String> {
    served(profile, ask_replies(), || {
        let mut command = profile.command(binary);
        command.args(["ask", ASK_PROMPT]);
        run(profile, command).map(drop)
    })
}

fn file_size(path: &Path) -> Result<u64, String> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|error| format!("stat {}: {error}", path.display()))
}

fn archive_size(profile: &Profile, release: &Path) -> Result<u64, String> {
    let archive = profile.path().join("oh-fx-linux-x86_64.tar.gz");
    let mut command = Command::new("tar");
    command
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(release)
        .args(ARCHIVE_FILES);
    run(profile, command)?;
    file_size(&archive)
}

fn served<T>(
    profile: &Profile,
    replies: Vec<Reply>,
    measure: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let server = FakeServer::start(replies);
    profile.connect(&server.base_url())?;
    measure()
}

fn instructions(profile: &Profile, binary: &Path, args: &[&str]) -> Result<u64, String> {
    let mut command = profile.command(Path::new("valgrind"));
    command
        .args(["--tool=callgrind", "--callgrind-out-file=/dev/null", "--"])
        .arg(binary)
        .args(args);
    collected_instructions(&run(profile, command)?.stderr)
}

fn stream_delta_instructions(profile: &Profile, binary: &Path) -> Result<u64, String> {
    let short = fewest_stream_instructions(profile, binary, STREAM_SHORT)?;
    let long = fewest_stream_instructions(profile, binary, STREAM_LONG)?;
    Ok(long.saturating_sub(short) / (STREAM_LONG - STREAM_SHORT))
}

fn fewest_stream_instructions(
    profile: &Profile,
    binary: &Path,
    deltas: u64,
) -> Result<u64, String> {
    let mut fewest = u64::MAX;
    for _ in 0..STREAM_RUNS {
        let count = served(profile, stream_replies(deltas), || {
            instructions(profile, binary, &["ask", ASK_PROMPT])
        })?;
        fewest = fewest.min(count);
    }
    Ok(fewest)
}

fn trace(
    profile: &Profile,
    binary: &Path,
    args: &[&str],
    filter: Option<&str>,
) -> Result<String, String> {
    let logs = profile.path().join("strace");
    match fs::remove_dir_all(&logs) {
        Err(error) if error.kind() != ErrorKind::NotFound => {
            return Err(format!("clear {}: {error}", logs.display()));
        }
        _ => {}
    }
    fs::create_dir(&logs).map_err(|error| format!("create {}: {error}", logs.display()))?;
    let mut command = profile.command(Path::new("strace"));
    command.args(["-ff", "-qq"]);
    if let Some(filter) = filter {
        command.args(["-e", filter]);
    }
    command
        .arg("-o")
        .arg(logs.join("process"))
        .arg("--")
        .arg(binary)
        .args(args);
    run(profile, command)?;
    let mut combined = String::new();
    let entries =
        fs::read_dir(&logs).map_err(|error| format!("read {}: {error}", logs.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|error| format!("read {}: {error}", logs.display()))?
            .path();
        combined.push_str(&repository::read(&path)?);
    }
    Ok(combined)
}

fn peak_rss(
    profile: &Profile,
    binary: &Path,
    args: &[&str],
    replies: impl Fn() -> Vec<Reply>,
) -> Result<u64, String> {
    let mut peak = 0;
    for _ in 0..RSS_RUNS {
        let run_peak = served(profile, replies(), || {
            let mut command = profile.command(binary);
            command.args(args);
            peak::exit_peak_rss_kib(profile.path(), command)
        })?;
        peak = peak.max(run_peak);
    }
    Ok(peak)
}

fn run(profile: &Profile, command: Command) -> Result<Finished, String> {
    let Spawned {
        mut child,
        program,
        stderr_path,
    } = spawn_logged(profile.path(), command)?;
    let deadline = Instant::now() + RUN_TIMEOUT;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("wait for {program}: {error}"))?
        {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{program} ran longer than {RUN_TIMEOUT:?}"));
        }
        thread::sleep(POLL);
    };
    if status.success() {
        Ok(Finished {
            stderr: fs::read_to_string(&stderr_path).unwrap_or_default(),
        })
    } else {
        Err(exit_failure(&program, &status.to_string(), &stderr_path))
    }
}

pub(crate) fn spawn_logged(dir: &Path, mut command: Command) -> Result<Spawned, String> {
    let stdout = output_file(&dir.join("stdout.log"))?;
    let stderr_path = dir.join("stderr.log");
    let stderr = output_file(&stderr_path)?;
    command.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
    let program = command.get_program().to_string_lossy().into_owned();
    let child = command
        .spawn()
        .map_err(|error| format!("run {program}: {error}"))?;
    Ok(Spawned {
        child,
        program,
        stderr_path,
    })
}

pub(crate) fn exit_failure(program: &str, how: &str, stderr_path: &Path) -> String {
    let stderr = fs::read_to_string(stderr_path).unwrap_or_default();
    format!("{program} exited with {how}: {}", last_line(&stderr))
}

fn output_file(path: &Path) -> Result<File, String> {
    File::create(path).map_err(|error| format!("create {}: {error}", path.display()))
}

fn last_line(text: &str) -> &str {
    text.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("no output")
}

fn collected_instructions(stderr: &str) -> Result<u64, String> {
    stderr
        .lines()
        .find_map(|line| line.split_once("Collected : ").map(|(_, count)| count))
        .and_then(|count| count.trim().parse().ok())
        .ok_or_else(|| {
            format!(
                "callgrind reported no instruction count: {}",
                last_line(stderr)
            )
        })
}

fn trace_lines(log: &str) -> impl Iterator<Item = &str> {
    log.lines().filter_map(|line| {
        let call = line
            .trim_start()
            .trim_start_matches(|character: char| character.is_ascii_digit())
            .trim_start();
        let is_call = !call.is_empty()
            && !call.starts_with("<...")
            && !call.starts_with("---")
            && !call.starts_with("+++");
        is_call.then_some(call)
    })
}

fn syscalls(log: &str) -> u64 {
    count(trace_lines(log))
}

fn ca_store_syscalls(log: &str) -> u64 {
    count(
        trace_lines(log)
            .filter(|call| CA_STORE_PREFIXES.iter().any(|prefix| call.contains(prefix))),
    )
}

fn git_processes(log: &str) -> u64 {
    count(trace_lines(log).filter(|call| {
        call.starts_with("execve(")
            && call
                .split('"')
                .nth(1)
                .is_some_and(|program| program.ends_with("/git"))
            && !call.contains("= -1 ")
    }))
}

fn count<'a>(lines: impl Iterator<Item = &'a str>) -> u64 {
    lines.fold(0, |total, _| total + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASK_LOG: &str = "\
101   execve(\"/bin/oh-fx\", [\"oh-fx\"], 0x1 /* 9 vars */) = 0
101   stat(\"/etc/ssl/certs/ca-certificates.crt\", {st_mode=S_IFREG|0644}) = 0
102   open(\"/etc/ssl/certs/0a.0\", O_RDONLY <unfinished ...>
101   futex(0x1, FUTEX_WAKE, 1) = 0
102   <... open resumed>) = 9
101   stat(\"/etc/pki/tls/certs\", 0x7ffd) = -1 ENOENT (No such file or directory)
--- SIGCHLD {si_signo=SIGCHLD} ---
101   connect(8, {sa_family=AF_INET}, 16) = 0
";

    #[test]
    fn counts_calls_once_across_unfinished_and_resumed_lines() {
        assert_eq!(syscalls(ASK_LOG), 6);
        assert_eq!(ca_store_syscalls(ASK_LOG), 3);
    }

    #[test]
    fn counts_only_successful_git_executions() {
        let log = "\
7  execve(\"/usr/bin/git\", [\"git\", \"ls-files\"], 0x1 /* 8 vars */) = 0
8  execve(\"/usr/local/bin/git\", [\"git\"], 0x1 /* 8 vars */) = -1 ENOENT (No such file or directory)
8  execve(\"/usr/bin/git\", [\"git\", \"grep\"], 0x1 /* 8 vars */) = 0
9  execve(\"/bin/oh-fx\", [\"oh-fx\"], 0x1 /* 8 vars */) = 0
";
        assert_eq!(git_processes(log), 2);
    }

    #[test]
    fn reads_callgrind_totals() {
        let stderr = "==12== Callgrind\n==12== Events    : Ir\n==12== Collected : 86022\n==12==\n";
        assert_eq!(collected_instructions(stderr), Ok(86_022));
        assert!(collected_instructions("oh-fx: failed\n").is_err());
    }
}
