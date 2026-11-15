//! Reading a test runner's output: which tests failed, and whether the run
//! reached its summary. A run without a summary did not finish, whatever its
//! exit status, so its tests count as not run rather than passed.

use serde::Deserialize;

/// How a shard's tests are run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runner {
    /// `python -m unittest` with dotted module, class or test names.
    Unittest,
    /// `python manage.py test` with Django test labels.
    Django,
    /// `python -m pytest` with test files or node ids.
    Pytest,
}

impl Runner {
    pub fn parse(name: &str) -> Option<Runner> {
        match name {
            "unittest" => Some(Runner::Unittest),
            "django" => Some(Runner::Django),
            "pytest" => Some(Runner::Pytest),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Runner::Unittest => "unittest",
            Runner::Django => "django",
            Runner::Pytest => "pytest",
        }
    }

    /// The command for one shard: the runner, any extra arguments, then the
    /// shard's targets. `python` is the locked environment's.
    pub fn command(self, extra: &[String], targets: &[String]) -> Vec<String> {
        let base: &[&str] = match self {
            Runner::Unittest => &["python", "-m", "unittest"],
            Runner::Django => &["python", "manage.py", "test", "--noinput"],
            Runner::Pytest => &["python", "-m", "pytest", "-p", "no:cacheprovider", "-rfE"],
        };
        base.iter().map(|s| s.to_string()).chain(extra.iter().cloned()).chain(targets.iter().cloned()).collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    /// It printed its final summary.
    pub complete: bool,
    /// Tests it reports running, when it says.
    pub ran: Option<u64>,
    /// Each failing or erroring test as the runner names it.
    pub failures: Vec<String>,
}

pub fn parse(runner: Runner, output: &str) -> Outcome {
    match runner {
        Runner::Unittest | Runner::Django => parse_unittest(output),
        Runner::Pytest => parse_pytest(output),
    }
}

/// unittest's (and Django's) `FAIL: name (id)` and `ERROR: name (id)`
/// headings, then `Ran N tests in …` and `OK`, `FAILED (…)` or `NO TESTS RAN`.
fn parse_unittest(output: &str) -> Outcome {
    let mut outcome = Outcome::default();
    let mut lines = output.lines().map(str::trim_end).peekable();
    while let Some(line) = lines.next() {
        for heading in ["FAIL: ", "ERROR: ", "UNEXPECTED SUCCESS: "] {
            if let Some(name) = line.strip_prefix(heading) {
                let name = name.trim().to_string();
                if !name.is_empty() && !outcome.failures.contains(&name) {
                    outcome.failures.push(name);
                }
            }
        }
        if let Some(count) = line.strip_prefix("Ran ").and_then(|rest| rest.split_whitespace().next()) {
            if let Ok(count) = count.parse::<u64>() {
                // The status follows after a blank line.
                let status = lines.clone().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
                if status.starts_with("OK") || status.starts_with("FAILED") || status.starts_with("NO TESTS RAN") {
                    outcome.complete = true;
                    outcome.ran = Some(count);
                }
            }
        }
    }
    outcome
}

/// pytest's short summary (`FAILED id - …`, `ERROR id - …`, from `-rfE`)
/// and its closing `==== … in 1.23s ====` line.
fn parse_pytest(output: &str) -> Outcome {
    let mut outcome = Outcome::default();
    for line in output.lines().map(str::trim_end) {
        for heading in ["FAILED ", "ERROR "] {
            if let Some(rest) = line.strip_prefix(heading) {
                let name = rest.split(" - ").next().unwrap_or(rest).trim().to_string();
                if !name.is_empty() && !outcome.failures.contains(&name) {
                    outcome.failures.push(name);
                }
            }
        }
        if let Some(summary) = pytest_summary(line) {
            outcome.complete = true;
            outcome.ran = Some(summary);
        }
    }
    outcome
}

/// The test count in pytest's closing line, e.g. `=== 1 failed, 3 passed in
/// 0.12s ===` gives 4; `=== no tests ran in 0.01s ===` gives 0.
fn pytest_summary(line: &str) -> Option<u64> {
    let inner = line.strip_prefix('=')?.trim_start_matches('=').trim();
    let inner = inner.strip_suffix('=')?.trim_end_matches('=').trim();
    let (counts, time) = inner.rsplit_once(" in ")?;
    let seconds = time.split_whitespace().next()?.strip_suffix('s')?;
    seconds.parse::<f64>().ok()?;
    if counts.trim() == "no tests ran" {
        return Some(0);
    }
    let mut total = 0;
    let mut any = false;
    for part in counts.split(", ") {
        let mut words = part.split_whitespace();
        let number = words.next()?.parse::<u64>().ok()?;
        let kind = words.next()?;
        if ["passed", "failed", "error", "errors", "xfailed", "xpassed", "skipped"].contains(&kind) {
            total += number;
            any = true;
        }
    }
    any.then_some(total)
}

#[cfg(test)]
#[path = "../tests/offload_results.rs"]
mod tests;
