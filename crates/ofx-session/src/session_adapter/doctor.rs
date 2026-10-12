use ofx_trace::trace_log;

use super::listing::LIST_PAGE_SIZE;
use crate::session_manager::{IoFault, ListCursor, Manager, VerifyError};
use crate::session_trace::SESSION;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoctorReport {
    pub sessions: usize,
    pub checked: usize,
    pub latest: Option<String>,
    pub damaged: Vec<String>,
}

pub(super) fn doctor(manager: &Manager, limit: usize) -> Result<DoctorReport, IoFault> {
    let mut report = DoctorReport::default();
    let mut latest_ms = 0;
    let mut cursor: Option<ListCursor> = None;
    loop {
        let page = manager.list(cursor.as_ref(), LIST_PAGE_SIZE)?;
        for item in &page.items {
            report.sessions += 1;
            if report.latest.is_none() || item.updated_ms > latest_ms {
                report.latest = Some(item.id.clone());
                latest_ms = item.updated_ms;
            }
            if report.checked == limit {
                continue;
            }
            report.checked += 1;
            match manager.verify(&item.id) {
                Ok(verified) => {
                    if verified.damaged_at.is_some()
                        || verified.bad_snapshots > 0
                        || verified.bad_blobs > 0
                    {
                        report.damaged.push(item.id.clone());
                    }
                }
                Err(VerifyError::NotFound) => {}
                Err(error) => {
                    trace_log!(
                        SESSION,
                        "event=sessions_v2_doctor_unreadable session={} err={error}",
                        item.id
                    );
                    report.damaged.push(item.id.clone());
                }
            }
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(report),
        }
    }
}

#[cfg(test)]
mod tests;
