//! Bounded JSON controls; private tokens and payloads may be supplied on stdin.
use super::*;
use std::io::Read;
pub(super) fn json_argument(arg: Option<&String>) -> Result<serde_json::Value> {
    let Some(arg) = arg else { return Ok(serde_json::json!({})) };
    let text = if arg == "-" {
        let mut text = String::new();
        std::io::stdin().take(262145).read_to_string(&mut text)?;
        text
    } else {
        arg.clone()
    };
    anyhow::ensure!(text.len() <= 262144, "fleet manifest exceeds 256KiB");
    Ok(serde_json::from_str(&text)?)
}
pub(super) fn run(kind: &str, args: &[String]) -> Result<()> {
    let default = if kind == "queue" { "list" } else { "list" };
    let operation = args.first().map(String::as_str).unwrap_or(default);
    let allowed = match kind {
        "queue" => &["add", "list", "cancel", "reorder", "enable", "batch"][..],
        "source" => &["register", "reconcile", "publish", "fail", "list", "refresh", "queue"][..],
        "chore" => &["register", "list", "probe", "enable", "run", "claim", "complete", "update", "remove"][..],
        _ => bail!("invalid work control"),
    };
    anyhow::ensure!(allowed.contains(&operation), "unsupported work control operation");
    let mut conn = Conn::open(false)?;
    anyhow::ensure!(conn.version >= 14, "Update needed: work queues, sources and chores require protocol 14");
    let value = json_argument(args.get(1))?;
    println!("{}", fleet_call(&mut conn, &format!("{kind}-{operation}"), value)?);
    Ok(())
}
