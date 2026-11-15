//! Headless source refreshes use the existing durable supervisor, privately.
use super::{sources::Source, store::Queue, Task};
use anyhow::{Context, Result};
use std::{io::Read, path::Path};
impl Queue {
    fn save_source(&self, source: &Source) -> Result<()> {
        crate::job_sessions::store::write(&self.source_path(&source.spec.id), source)
    }
    pub fn refresh_sources(&mut self, exe: &Path, now: u64, admitted: bool) -> Result<()> {
        self.refresh_jobs.refresh()?;
        let sources: Vec<_> = self.sources.values().cloned().collect();
        for source in sources {
            if self.refresh_one(source.clone(), exe, now, admitted).is_err() {
                let mut next = self.sources.get(&source.spec.id).cloned().unwrap_or(source);
                let changed = next.error.as_deref() != Some("source refresh failed");
                next.error = Some("source refresh failed".into());
                next.attempted_at = now;
                if changed {
                    let _ = self.save_source(&next);
                }
                self.sources.insert(next.spec.id.clone(), next);
            }
        }
        // The source publication clears its durable job reference first. Only then
        // may a reaped known-exit private refresh receipt be removed.
        let referenced: std::collections::BTreeSet<_> = self.sources.values().filter_map(|s| s.refresh_job).collect();
        let obsolete: Vec<_> =
            self.refresh_jobs.records.keys().copied().filter(|id| !referenced.contains(id)).collect();
        for id in obsolete {
            let r = &self.refresh_jobs.records[&id];
            if matches!(r.info.status.as_str(), "starting" | "working") {
                self.refresh_jobs.stop(id)?;
            }
            if self.refresh_jobs.records[&id].info.exit_code.is_some()
                || self.refresh_jobs.records[&id].info.signal.is_some()
            {
                let _ = self.refresh_jobs.purge_completed(id);
            }
        }
        Ok(())
    }
    fn refresh_one(&mut self, source: Source, exe: &Path, now: u64, admitted: bool) -> Result<()> {
        if !source.owner_live || !source.spec.enabled {
            if let Some(job) = source.refresh_job {
                self.refresh_jobs.stop(job)?;
                if !self.refresh_jobs.workload_pending(job)? {
                    let mut next = source.clone();
                    next.refresh_job = None;
                    next.sequence = next.sequence.saturating_add(1);
                    self.save_source(&next)?;
                    self.sources.insert(next.spec.id.clone(), next);
                }
            }
            return Ok(());
        }

        let id = source.spec.id.clone();
        if let Some(job) = source.refresh_job {
            let Some(record) = self.refresh_jobs.records.get(&job) else {
                let mut next = source.clone();
                next.refresh_job = None;
                next.error = Some("source recovery required".into());
                self.save_source(&next)?;
                self.sources.insert(id, next);
                return Ok(());
            };
            if !matches!(record.info.status.as_str(), "finished" | "failed") {
                if admitted && !self.refresh_jobs.spawned(job)? {
                    self.refresh_jobs.start(job, exe, record.intent.spec.clone())?;
                }
                return Ok(());
            }
            if record.info.exit_code.is_none() && self.refresh_jobs.workload_pending(job)? {
                self.refresh_jobs.stop(job)?;
                return Ok(());
            }
            let result = (|| -> Result<Vec<Task>> {
                anyhow::ensure!(record.info.exit_code == Some(0), "source exited unsuccessfully");
                let mut bytes = Vec::new();
                crate::job_sessions::store::open_log(&self.refresh_jobs.dir(job).join("output.log"))?
                    .take(262145)
                    .read_to_end(&mut bytes)?;
                anyhow::ensure!(bytes.len() <= 262144, "source output too large");
                let value: serde_json::Value = serde_json::from_slice(&bytes)?;
                Ok(serde_json::from_value(value.get("tasks").context("source tasks missing")?.clone())?)
            })();
            let next_sequence = source.sequence.checked_add(1).context("source sequence exhausted")?;
            let tasks = result.ok();
            if self.source_publish(&id, &source.spec.owner, &source.token, next_sequence, tasks, now).is_err() {
                // Malformed snapshots invalidate freshness but never publish partial rows or raw stderr.
                self.source_publish(&id, &source.spec.owner, &source.token, next_sequence, None, now)?;
            }
            return Ok(());
        }
        if source.spec.mode != "command"
            || !source.spec.enabled
            || !admitted
            || now.saturating_sub(source.attempted_at) < source.spec.interval_seconds
        {
            return Ok(());
        }
        let timeout = Path::new("/usr/bin/timeout");
        if !timeout.is_file() {
            let mut next = source.clone();
            next.error = Some("source timeout utility unavailable".into());
            next.attempted_at = now;
            self.save_source(&next)?;
            self.sources.insert(id, next);
            return Ok(());
        }
        let mut argv = vec![
            timeout.to_string_lossy().into_owned(),
            "--signal=TERM".into(),
            "--kill-after=2".into(),
            "--".into(),
            "60".into(),
        ];
        argv.extend(source.spec.argv.clone());
        let key = format!("source-{:010x}-{}-{}", super::store::identity(&id), source.sequence + 1, source.token);
        let spec = crate::job_sessions::Spec {
            argv,
            cwd: source.spec.cwd.clone().context("source cwd missing")?,
            ..Default::default()
        };
        let info = self.refresh_jobs.add(key, "Task source refresh".into(), spec.clone(), "local".into(), None)?;
        let mut next = source.clone();
        next.refresh_job = Some(info.id);
        next.attempted_at = now;
        self.save_source(&next)?;
        self.sources.insert(id, next);
        self.refresh_jobs.start(info.id, exe, spec)?;

        Ok(())
    }
    pub fn stop_source_jobs(&mut self) -> Result<()> {
        let ids: Vec<_> = self.refresh_jobs.records.keys().copied().collect();
        for id in ids {
            self.refresh_jobs.stop(id)?;
        }
        Ok(())
    }
}
