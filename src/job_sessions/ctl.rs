//! Argv-only control interface; JSON stdin keeps manifests off process listings.
use super::*;
use std::io::Read;
pub(super) fn run(args: &[String]) -> Result<()> {
    let operation = args.first().map(String::as_str).unwrap_or("status");
    anyhow::ensure!(
        ["start", "status", "log", "stop", "dismiss"].contains(&operation),
        "job start|status|log|stop|dismiss"
    );
    let rest = &args[args.len().min(1)..];
    let mut value = serde_json::json!({});
    if rest.first().is_some_and(|s| s == "-" || s.starts_with('{')) {
        let text = if rest[0] == "-" {
            let mut text = String::new();
            std::io::stdin().take(262145).read_to_string(&mut text)?;
            text
        } else {
            rest[0].clone()
        };
        anyhow::ensure!(text.len() <= 262144, "job manifest exceeds 256KiB");
        value = serde_json::from_str(&text)?;
    } else if operation == "start" {
        value["key"] = serde_json::json!(crate::util::uuid_v4()?);
        value["cwd"] = serde_json::json!(std::env::current_dir()?);
        let mut i = 0;
        while i < rest.len() {
            let flag = &rest[i];
            if flag == "--" {
                value["argv"] = serde_json::json!(&rest[i + 1..]);
                break;
            }
            if flag == "--env-clear" {
                value["env_clear"] = serde_json::json!(true);
                i += 1;
                continue;
            }
            let next = rest.get(i + 1).context("job option requires a value")?;
            let field = match flag.as_str() {
                "--cwd" => "cwd",
                "--host" => "placement",
                "--title" => "title",
                "--provider" => "provider",
                "--account" => "account",
                "--request-key" => "key",
                _ => bail!("unknown job option"),
            };
            value[field] = serde_json::json!(next);
            i += 2;
        }
    } else if let Some(id) = rest.first() {
        value["job"] = serde_json::json!(id.parse::<u64>()?);
    }
    let mut conn = Conn::open(false)?;
    anyhow::ensure!(conn.version >= 14, "Update needed: jobs require protocol 14");
    println!("{}", fleet_call(&mut conn, &format!("job-{operation}"), value)?);
    Ok(())
}
