use std::sync::atomic::{AtomicBool, Ordering};

use ofx_trace::trace_log;
use rustix::io::Errno;
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

const TERMINAL_CLIENT: &str = "terminal_client";
const DESIRED_DESCRIPTOR_LIMIT: u64 = 1024;

static CHECKED: AtomicBool = AtomicBool::new(false);

pub(super) fn ensure_descriptor_budget() {
    if CHECKED.swap(true, Ordering::AcqRel) {
        return;
    }
    let limit = getrlimit(Resource::Nofile);
    let Some(target) = descriptor_limit_target(limit.current, limit.maximum) else {
        return;
    };
    let raised = Rlimit {
        current: Some(target),
        maximum: limit.maximum,
    };
    match setrlimit(Resource::Nofile, raised) {
        Ok(()) => trace_log!(
            TERMINAL_CLIENT,
            "file descriptor limit raised soft={target}"
        ),
        Err(errno) => trace_log!(
            TERMINAL_CLIENT,
            "file descriptor limit unchanged target={target} err={}",
            setrlimit_error_name(errno)
        ),
    }
}

fn setrlimit_error_name(errno: Errno) -> &'static str {
    match errno {
        Errno::INVAL => "LimitTooBig",
        Errno::PERM => "PermissionDenied",
        _ => "Unexpected",
    }
}

fn descriptor_limit_target(current: Option<u64>, maximum: Option<u64>) -> Option<u64> {
    let target = maximum.map_or(DESIRED_DESCRIPTOR_LIMIT, |maximum| {
        maximum.min(DESIRED_DESCRIPTOR_LIMIT)
    });
    current
        .is_some_and(|current| current < target)
        .then_some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_target_is_bounded_by_the_hard_limit() {
        assert_eq!(descriptor_limit_target(Some(256), None), Some(1024));
        assert_eq!(descriptor_limit_target(Some(256), Some(512)), Some(512));
        assert_eq!(descriptor_limit_target(Some(1024), None), None);
        assert_eq!(descriptor_limit_target(Some(4096), Some(8192)), None);
        assert_eq!(descriptor_limit_target(None, None), None);
    }

    #[test]
    fn a_refused_raise_is_named_as_upstream_names_it() {
        assert_eq!(setrlimit_error_name(Errno::INVAL), "LimitTooBig");
        assert_eq!(setrlimit_error_name(Errno::PERM), "PermissionDenied");
        assert_eq!(setrlimit_error_name(Errno::NOMEM), "Unexpected");
    }
}
