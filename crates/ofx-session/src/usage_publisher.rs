use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::thread::{self, JoinHandle};

use crate::profile_usage_runtime::ProfilePublisher;
use crate::profile_usage_store::ProfileEvent;
use crate::session_log::WritableSession;

const DRAIN_THREAD: &str = "usage-publication";

pub struct UsagePublisher {
    shared: Arc<Shared>,
}

#[derive(Clone)]
pub struct PublicationScheduler {
    shared: Arc<Shared>,
}

struct Shared {
    session: Weak<Mutex<WritableSession>>,
    profile: ProfilePublisher,
    publishing: Mutex<()>,
    drain: Mutex<Option<JoinHandle<()>>>,
    epoch: AtomicU64,
    done: AtomicBool,
    cancel: AtomicBool,
    finished: AtomicBool,
}

impl UsagePublisher {
    pub fn new(session: &Arc<Mutex<WritableSession>>, profile: ProfilePublisher) -> Self {
        Self {
            shared: Arc::new(Shared {
                session: Arc::downgrade(session),
                profile,
                publishing: Mutex::new(()),
                drain: Mutex::new(None),
                epoch: AtomicU64::new(0),
                done: AtomicBool::new(true),
                cancel: AtomicBool::new(false),
                finished: AtomicBool::new(false),
            }),
        }
    }

    pub fn scheduler(&self) -> PublicationScheduler {
        PublicationScheduler {
            shared: Arc::clone(&self.shared),
        }
    }

    pub fn schedule(&self) {
        self.shared.schedule();
    }

    pub fn finish_before_shutdown(&self) {
        let shared = &self.shared;
        if shared.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        shared.cancel.store(true, Ordering::SeqCst);
        {
            let mut drain = lock(&shared.drain);
            if let Some(running) = drain.take() {
                let _ = running.join();
            }
            shared.done.store(true, Ordering::SeqCst);
            shared.cancel.store(false, Ordering::SeqCst);
        }
        shared.flush();
    }
}

impl Drop for UsagePublisher {
    fn drop(&mut self) {
        self.finish_before_shutdown();
    }
}

impl PublicationScheduler {
    pub fn schedule(&self) {
        self.shared.schedule();
    }
}

impl Shared {
    fn schedule(self: &Arc<Self>) {
        if self.finished.load(Ordering::SeqCst) {
            return;
        }
        let shared = self;
        shared.epoch.fetch_add(1, Ordering::SeqCst);
        let mut drain = lock(&shared.drain);
        if let Some(running) = drain.take() {
            if !shared.done.load(Ordering::SeqCst) {
                *drain = Some(running);
                return;
            }
            let _ = running.join();
        }
        shared.cancel.store(false, Ordering::SeqCst);
        shared.done.store(false, Ordering::SeqCst);
        let worker = Arc::clone(shared);
        if let Ok(running) = thread::Builder::new()
            .name(DRAIN_THREAD.to_owned())
            .spawn(move || worker.drain())
        {
            *drain = Some(running);
        } else {
            shared.done.store(true, Ordering::SeqCst);
            drop(drain);
            shared.flush();
        }
    }

    fn drain(&self) {
        let mut observed = self.epoch.load(Ordering::SeqCst);
        while !self.cancel.load(Ordering::SeqCst) {
            self.flush();
            if self.cancel.load(Ordering::SeqCst) {
                break;
            }
            let current = self.epoch.load(Ordering::SeqCst);
            if current != observed {
                observed = current;
                continue;
            }
            self.done.store(true, Ordering::SeqCst);
            let confirmed = self.epoch.load(Ordering::SeqCst);
            if self.cancel.load(Ordering::SeqCst) || confirmed == observed {
                return;
            }
            self.done.store(false, Ordering::SeqCst);
            observed = confirmed;
        }
        self.done.store(true, Ordering::SeqCst);
    }

    fn flush(&self) {
        let _publishing = lock(&self.publishing);
        let Some(session) = self.session.upgrade() else {
            return;
        };
        let batch = lock(&session).usage_publication_batch();
        let mut published = false;
        'publish: {
            for marker in &batch.pending {
                if let Err(error) = self.profile.publish(ProfileEvent::Pending(marker))
                    && error.ledger_unavailable()
                {
                    break 'publish;
                }
            }
            for incident in &batch.incidents {
                match self.profile.publish(ProfileEvent::Incident(incident)) {
                    Ok(()) => {
                        lock(&session).usage_incident_published(incident);
                        published = true;
                    }
                    Err(error) if error.ledger_unavailable() => break 'publish,
                    Err(_) => {}
                }
            }
            for fact in &batch.facts {
                match self.profile.publish(ProfileEvent::Generation(fact)) {
                    Ok(()) => {
                        lock(&session).usage_fact_published(fact);
                        published = true;
                    }
                    Err(error) if error.ledger_unavailable() => break 'publish,
                    Err(_) => {}
                }
            }
        }
        if batch.checkpoint_changed || published {
            lock(&session).save_published_usage();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
