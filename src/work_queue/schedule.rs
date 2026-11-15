//! Calendar schedules with host-local or UTC time; no 24-hour arithmetic.
use super::Schedule;
use anyhow::{Context, Result};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Calendar {
    pub day: String,
    pub minute: u32,
    pub hour: u32,
    pub month: u32,
    pub month_day: u32,
    pub weekday: u32,
}
fn field(text: &str, value: u32, low: u32, high: u32) -> Result<bool> {
    let mut matched = false;
    anyhow::ensure!(!text.is_empty() && text.len() <= 64, "invalid cron field");
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>()?),
            None => (part, 1),
        };
        anyhow::ensure!(step > 0 && step <= high - low + 1, "invalid cron step");
        let (start, end) = if range == "*" {
            (low, high)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse::<u32>()?, b.parse::<u32>()?)
        } else {
            let n = range.parse::<u32>()?;
            (n, n)
        };
        anyhow::ensure!(start >= low && end <= high && start <= end, "cron field outside range");
        matched |= value >= start && value <= end && (value - start) % step == 0;
    }
    Ok(matched)
}
impl Schedule {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(matches!(self.timezone.as_str(), "local" | "UTC"), "schedule timezone must be local or UTC");
        let f: Vec<_> = self.cron.split_whitespace().collect();
        anyhow::ensure!(f.len() == 5, "schedule requires five cron fields");
        for (s, (low, high)) in f.iter().zip([(0, 59), (0, 23), (1, 31), (1, 12), (0, 6)]) {
            field(s, low, low, high)?;
        }
        Ok(())
    }
    pub fn occurrence(&self, c: &Calendar) -> Result<Option<String>> {
        self.validate()?;
        let f: Vec<_> = self.cron.split_whitespace().collect();
        // A single daily wall time catches up only within its current local date.
        // A skipped spring-forward minute therefore runs later that same day.
        let daily = f[0].parse::<u32>().ok().zip(f[1].parse::<u32>().ok()).filter(|_| f[2..].iter().all(|s| *s == "*"));
        if let Some((minute, hour)) = daily {
            return Ok(
                (c.hour * 60 + c.minute >= hour * 60 + minute).then(|| format!("{}T{hour:02}:{minute:02}", c.day))
            );
        }
        let dom = field(f[2], c.month_day, 1, 31)?;
        let dow = field(f[4], c.weekday, 0, 6)?;
        let date = if f[2] != "*" && f[4] != "*" { dom || dow } else { dom && dow };
        let ok = field(f[0], c.minute, 0, 59)? && field(f[1], c.hour, 0, 23)? && date && field(f[3], c.month, 1, 12)?;
        Ok(ok.then(|| format!("{}T{:02}:{:02}", c.day, c.hour, c.minute)))
    }
    pub fn claim_occurrence(&self, c: &Calendar) -> Result<String> {
        self.validate()?;
        let f: Vec<_> = self.cron.split_whitespace().collect();
        if f[2..].iter().all(|s| *s == "*") {
            if let (Ok(minute), Ok(hour)) = (f[0].parse::<u32>(), f[1].parse::<u32>()) {
                return Ok(format!("{}T{hour:02}:{minute:02}", c.day));
            }
        }
        Ok(format!("{}T{:02}:{:02}", c.day, c.hour, c.minute))
    }
    pub fn next_unclaimed(&self, now: u64, claimed: &std::collections::BTreeSet<String>) -> Result<Option<u64>> {
        let mut at = now / 60 * 60;
        for _ in 0..=527040 {
            let c = calendar(at, &self.timezone)?;
            if let Some(key) = self.occurrence(&c)? {
                if !claimed.contains(&key) {
                    return Ok(Some(at.max(now)));
                }
            }
            at = at.checked_add(60).context("schedule time overflow")?;
        }
        Ok(None)
    }
    pub fn next(&self, now: u64) -> Result<Option<u64>> {
        self.validate()?;
        let mut at = now / 60 * 60;
        for _ in 0..=527040 {
            let c = calendar(at, &self.timezone)?;
            if self.occurrence(&c)?.is_some() {
                return Ok(Some(at.max(now)));
            }
            at = at.checked_add(60).context("schedule time overflow")?;
        }
        Ok(None)
    }
}
pub fn calendar(at: u64, zone: &str) -> Result<Calendar> {
    let at: libc::time_t = at.try_into().context("invalid calendar time")?;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: libc writes one tm into the valid allocation; no global TZ mutation.
    let result = unsafe {
        if zone == "UTC" {
            libc::gmtime_r(&at, tm.as_mut_ptr())
        } else {
            libc::localtime_r(&at, tm.as_mut_ptr())
        }
    };
    anyhow::ensure!(!result.is_null(), "calendar unavailable");
    let tm = unsafe { tm.assume_init() };
    Ok(Calendar {
        day: format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday),
        minute: tm.tm_min as u32,
        hour: tm.tm_hour as u32,
        month: tm.tm_mon as u32 + 1,
        month_day: tm.tm_mday as u32,
        weekday: tm.tm_wday as u32,
    })
}
#[cfg(test)]
#[path = "../tests/work_queue/schedule.rs"]
mod tests;
