//! Ordered, bounded write queue for one mounted catalog. Reads use redb's
//! committed snapshots and the cache overlays changes not yet acknowledged.
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use super::persistent::{CatalogPatch, PersistentDb};
use crate::core::types::FsEntry;
use crate::telemetry::{self, Event, Logger, OperationContext, Severity};

type Snapshot = (Vec<FsEntry>, Vec<String>);

#[derive(Default)]
struct Progress {
    committed: u64,
    failure: Option<String>,
}

enum Job {
    Patch {
        seq: u64,
        context: OperationContext,
        operation: &'static str,
        changes: Arc<CatalogPatch>,
    },
    Snapshot {
        context: OperationContext,
        owner: String,
        entries: Vec<FsEntry>,
        done: SyncSender<Result<Snapshot, String>>,
    },
    Barrier(SyncSender<()>),
}

pub(crate) struct CatalogWriter {
    sender: Option<SyncSender<Job>>,
    progress: Arc<Mutex<Progress>>,
    thread: Option<JoinHandle<()>>,
    last_submitted: u64,
}

impl CatalogWriter {
    pub(crate) fn start(db: Arc<PersistentDb>, logger: Option<Logger>) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(64);
        let progress = Arc::new(Mutex::new(Progress::default()));
        let thread_progress = Arc::clone(&progress);
        let thread = thread::Builder::new()
            .name("punkfs-catalog".into())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    match job {
                        Job::Patch {
                            seq,
                            context,
                            operation,
                            changes,
                        } => {
                            match db.apply_patch(&changes) {
                                Ok(()) => {
                                    thread_progress.lock().unwrap().committed = seq;
                                    if let Some(logger) =
                                        logger.as_ref().or_else(|| telemetry::global())
                                        && logger.enabled(Severity::Trace)
                                    {
                                        let mut event = Event::new(
                                            Severity::Trace,
                                            "metadata_db",
                                            "catalog write committed",
                                        );
                                        event.operation = Some(operation.into());
                                        event.fields.insert(
                                            "changes".into(),
                                            serde_json::json!(changes.len()),
                                        );
                                        logger.emit_in(&context, event);
                                    }
                                }
                                Err(error) => {
                                    let message = error.to_string();
                                    log_failure(logger.as_ref(), &context, operation, &message);
                                    thread_progress.lock().unwrap().failure = Some(message);
                                    // Never commit later jobs over failed earlier work.
                                    return;
                                }
                            }
                        }
                        Job::Snapshot {
                            context,
                            owner,
                            entries,
                            done,
                        } => {
                            let result = db
                                .reconcile_owner_snapshot(&owner, &entries)
                                .map_err(|error| error.to_string());
                            if let Err(message) = &result {
                                log_failure(
                                    logger.as_ref(),
                                    &context,
                                    "reconcile_snapshot",
                                    message,
                                );
                                thread_progress.lock().unwrap().failure = Some(message.clone());
                            }
                            let failed = result.is_err();
                            let _ = done.send(result);
                            if failed {
                                return;
                            }
                        }
                        Job::Barrier(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            progress,
            thread: Some(thread),
            last_submitted: 0,
        })
    }

    pub(crate) fn committed(&self) -> u64 {
        self.progress.lock().unwrap().committed
    }

    pub(crate) fn failure(&self) -> Option<String> {
        let failure = self.progress.lock().unwrap().failure.clone();
        failure.or_else(|| {
            self.thread
                .as_ref()
                .filter(|thread| thread.is_finished())
                .map(|_| "metadata writer stopped".into())
        })
    }

    fn send(&self, job: Job) -> Result<(), String> {
        self.sender.as_ref().unwrap().send(job).map_err(|_| {
            self.failure()
                .unwrap_or_else(|| "metadata writer disconnected".into())
        })
    }

    pub(crate) fn submit(
        &mut self,
        context: &OperationContext,
        operation: &'static str,
        changes: Arc<CatalogPatch>,
    ) -> Result<u64, String> {
        let seq = self.last_submitted + 1;
        self.send(Job::Patch {
            seq,
            context: context.clone(),
            operation,
            changes,
        })?;
        self.last_submitted = seq;
        Ok(seq)
    }

    pub(crate) fn flush(&self) -> Result<(), String> {
        if let Some(failure) = self.failure() {
            return Err(failure);
        }
        let (done, ack) = mpsc::sync_channel(1);
        self.send(Job::Barrier(done))?;
        ack.recv().map_err(|_| {
            self.failure()
                .unwrap_or_else(|| "metadata writer disconnected".into())
        })
    }

    pub(crate) fn reconcile_snapshot(
        &self,
        context: &OperationContext,
        owner: &str,
        entries: Vec<FsEntry>,
    ) -> Result<Snapshot, String> {
        let (done, ack) = mpsc::sync_channel(1);
        self.send(Job::Snapshot {
            context: context.clone(),
            owner: owner.into(),
            entries,
            done,
        })?;
        ack.recv().map_err(|_| {
            self.failure()
                .unwrap_or_else(|| "metadata writer disconnected".into())
        })?
    }
}

impl Drop for CatalogWriter {
    fn drop(&mut self) {
        // Close the input and let the worker drain already accepted jobs.
        self.sender.take();
        if self
            .thread
            .take()
            .is_some_and(|worker| worker.join().is_err())
            && let Some(logger) = telemetry::global()
        {
            logger.emit(Event::new(
                Severity::Error,
                "metadata_db",
                "metadata writer panicked",
            ));
        }
    }
}

fn log_failure(
    logger: Option<&Logger>,
    context: &OperationContext,
    operation: &str,
    message: &str,
) {
    if let Some(logger) = logger.or(telemetry::global()) {
        let mut event = Event::new(Severity::Error, "metadata_db", "catalog write failed");
        event.operation = Some(operation.into());
        event
            .fields
            .insert("error".into(), serde_json::json!(message));
        logger.emit_in(context, event);
    }
}
