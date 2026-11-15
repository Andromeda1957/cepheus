//! Native workspace-aware placement, using fresh cached probes only.
use super::Service;
use crate::{protocol::Tool, scheduler::placement};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

impl Service {
    pub(super) fn reserve_placement(&self, host: Option<String>) {
        self.placement_reserved.borrow_mut().entry(host).or_default().push(crate::util::now_secs());
    }

    pub(super) fn workspace_host(
        &self,
        cwd: Option<&Path>,
        preferred: &str,
        tool: Option<Tool>,
    ) -> Result<(Option<String>, PathBuf)> {
        let requested = cwd.unwrap_or(&self.opts.start_dir);
        let canonical;
        let cwd = if matches!(preferred, "local" | "@here") {
            requested
        } else {
            anyhow::ensure!(requested.is_absolute(), "placement cwd must be an absolute directory");
            canonical = requested.canonicalize().context("placement cwd is unavailable")?;
            anyhow::ensure!(canonical.is_dir(), "placement cwd must be a directory");
            canonical.as_path()
        };
        let now = crate::util::now_secs();
        let mut reservations = self.placement_reserved.borrow_mut();
        for starts in reservations.values_mut() {
            starts.retain(|at| now.saturating_sub(*at) < placement::RESERVATION_SECONDS);
        }
        reservations.retain(|_, v| !v.is_empty());
        let found = self.scheduler.repository_for(cwd);
        let local = self.scheduler.settings.probe.read();
        let mut candidates = Vec::new();
        let local_tools = self.launchable_tools();
        if matches!(preferred, "auto" | "@auto" | "local" | "@here")
            && tool.is_none_or(|t| !t.is_agent() || local_tools.iter().any(|v| v == t.key()))
        {
            if let Some(score) = placement::score(&local, now, reservations.get(&None).map_or(0, Vec::len), true) {
                candidates.push((score, None, cwd.to_path_buf()));
            }
        }
        if !matches!(preferred, "local" | "@here") {
            if let Some((id, root)) = found {
                // A linked tree is a native workspace, with its own uncommitted
                // input and task state. Never substitute another host's primary.
                let portable = !root.join(".git").is_file();
                if portable {
                    let relative = cwd.strip_prefix(&root).context("placement cwd escaped its repository")?;
                    for slot in self.hosts.values() {
                        if !matches!(preferred, "auto" | "@auto") && preferred != slot.config.name {
                            continue;
                        }
                        let Some(state) = slot.placement_state() else {
                            continue;
                        };
                        if !tool.is_none_or(|t| {
                            !t.is_agent()
                                || state
                                    .available_tools
                                    .as_ref()
                                    .is_some_and(|tools| tools.iter().any(|v| v == t.key()))
                        }) {
                            continue;
                        }
                        let Some(path) = state.scheduler.repositories.get(&id) else {
                            continue;
                        };
                        let path = Path::new(path);
                        if !path.is_absolute()
                            || path.components().any(|part| matches!(part, std::path::Component::ParentDir))
                        {
                            continue;
                        }
                        let key = Some(slot.config.name.clone());
                        let Some(score) = placement::score(
                            &state.scheduler.headroom,
                            now,
                            reservations.get(&key).map_or(0, Vec::len),
                            false,
                        ) else {
                            continue;
                        };
                        candidates.push((score, key, PathBuf::from(path).join(relative)));
                    }
                }
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        candidates.into_iter().next().map(|(_,host,path)|(host,path))
            .context("No capable online host has this native workspace and fresh capacity; workspace operations can recover with portable source inputs")
    }
}
