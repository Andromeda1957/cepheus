//! Injectable resource measurements. Automatic admission requires measured headroom.
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Headroom {
    pub available_mib: Option<u64>,
    pub total_mib: Option<u64>,
    pub memory_pressure: Option<f64>,
    pub cpu_pressure: Option<f64>,
    pub load: Option<f64>,
    pub cpus: u64,
    pub at: u64,
}
impl Headroom {
    pub fn memory_free(&self) -> bool {
        self.available_mib.zip(self.total_mib).is_some_and(|(a, t)| a >= 2048.max(t.saturating_mul(15) / 100))
            && self.memory_pressure.is_some_and(|v| v.is_finite() && (0.0..5.0).contains(&v))
    }
    pub fn admits(&self) -> bool {
        self.memory_free() && self.cpu_pressure.is_some_and(|v| v.is_finite() && (0.0..80.0).contains(&v))
    }
    pub fn calm(&self, limit: f64) -> bool {
        limit == 0.0 || self.load.is_some_and(|v| v.is_finite() && v >= 0.0 && v / self.cpus.max(1) as f64 <= limit)
    }
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Probe {
    pub root: Option<PathBuf>,
}
impl Probe {
    pub fn read(&self) -> Headroom {
        let root = self.root.clone().unwrap_or_else(|| PathBuf::from("/proc"));
        let mem = fs::read_to_string(root.join("meminfo")).unwrap_or_default();
        let field = |key: &str| {
            mem.lines()
                .find_map(|l| l.strip_prefix(key)?.split_whitespace().next()?.parse::<u64>().ok())
                .map(|v| v / 1024)
        };
        let pressure = |file: &str, kind: &str, window: &str| {
            fs::read_to_string(root.join(file)).ok().and_then(|v| {
                v.lines().find(|l| l.starts_with(kind)).and_then(|l| {
                    l.split_whitespace().find_map(|p| {
                        p.strip_prefix(window)?.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0)
                    })
                })
            })
        };
        Headroom {
            available_mib: field("MemAvailable:"),
            total_mib: field("MemTotal:"),
            memory_pressure: pressure("pressure/memory", "full ", "avg10="),
            cpu_pressure: pressure("pressure/cpu", "some ", "avg60="),
            load: fs::read_to_string(root.join("loadavg"))
                .ok()
                .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0)),
            cpus: std::thread::available_parallelism().map_or(1, |v| v.get() as u64),
            at: crate::util::now_secs(),
        }
    }
}
