//! Durable session-registry writes on their own thread, so the service loop
//! that echoes keystrokes and draws frames never waits on a disk sync.
//! Writes are coalesced to the newest snapshot. A barrier (input whose
//! recovery record must be durable before an agent receives it) waits for
//! its own snapshot; everything else is handed over and reported back.
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread;

use super::Ev;
use crate::util;

struct Job {
    bytes: Vec<u8>,
    reply: Option<Sender<Result<(), String>>>,
}

pub(super) struct Writer {
    jobs: Option<Sender<Job>>,
    thread: Option<thread::JoinHandle<()>>,
    /// The newest snapshot handed over, cleared when a write fails so the
    /// next save retries it.
    queued: Option<Vec<u8>>,
    /// Snapshots handed over without waiting whose outcome has not arrived.
    pub(super) in_flight: usize,
}

impl Writer {
    pub(super) fn start(path: PathBuf, events: Sender<Ev>, existing: Option<Vec<u8>>) -> Self {
        let (jobs, rx) = mpsc::channel::<Job>();
        let thread = thread::Builder::new()
            .name("registry-writer".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    let mut replies = Vec::new();
                    let mut newest = first;
                    let mut background = 0usize;
                    loop {
                        match newest.reply.take() {
                            Some(reply) => replies.push(reply),
                            None => background += 1,
                        }
                        match rx.try_recv() {
                            Ok(next) => newest = next,
                            Err(_) => break,
                        }
                    }
                    let result = util::write_atomic(&path, &newest.bytes).map_err(|e| format!("{e}"));
                    for reply in replies {
                        let _ = reply.send(result.clone());
                    }
                    for _ in 0..background {
                        let _ = events.send(Ev::RegistrySaved(result.clone()));
                    }
                }
            })
            .ok();
        Self { jobs: Some(jobs), thread, queued: existing, in_flight: 0 }
    }

    /// Whether `bytes` is already the newest snapshot handed over.
    pub(super) fn current(&self, bytes: &[u8]) -> bool {
        self.queued.as_deref() == Some(bytes)
    }

    /// Hand a snapshot over and return at once; its outcome arrives as
    /// `Ev::RegistrySaved`.
    pub(super) fn queue(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        let jobs = self.jobs.as_ref().ok_or("the registry writer stopped")?;
        jobs.send(Job { bytes: bytes.clone(), reply: None }).map_err(|_| "the registry writer stopped")?;
        self.queued = Some(bytes);
        self.in_flight += 1;
        Ok(())
    }

    /// Hand a snapshot over and wait until it, or a newer one, is durable.
    pub(super) fn barrier(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        let jobs = self.jobs.as_ref().ok_or("the registry writer stopped")?;
        let (reply, done) = mpsc::channel();
        jobs.send(Job { bytes: bytes.clone(), reply: Some(reply) }).map_err(|_| "the registry writer stopped")?;
        self.queued = Some(bytes);
        let result = done.recv().unwrap_or_else(|_| Err("the registry writer stopped".into()));
        if result.is_err() {
            self.queued = None;
        }
        result
    }

    /// A write failed: the next save must not take its snapshot as current.
    pub(super) fn failed(&mut self) {
        self.queued = None;
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Finish what was handed over before the process moves on.
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
