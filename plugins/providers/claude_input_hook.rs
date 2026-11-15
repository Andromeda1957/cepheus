//! `cepheus hook owner-input`: the program Claude's `cepheus-owner-input`
//! plugin runs; the service side is `claude_input.rs`. `listen` prints each
//! owner message for the session it runs under as one JSON line, for as long
//! as Claude runs, a broadcast's with `"steer": true` so the plugin delivers
//! it into the running turn; `listen <conversation> stop` also prints each owner Stop
//! (`{"message", "stop": true}`) for a plugin that ends turns. `receipt` reads
//! `{message, outcome, reason}` and reports it.
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::paths;
use crate::protocol::{read_msg, write_msg, ClientMsg, ClientRole, OwnerInputOutcome, ServerMsg, PROTOCOL_VERSION};

/// The largest receipt read from the plugin.
const RECEIPT_MAX: u64 = 16 * 1024;

pub fn run(args: &[String]) -> i32 {
    let result = match args {
        [command, conversation] if command == "listen" => listen(conversation, false),
        [command, conversation, stop] if command == "listen" && stop == "stop" => listen(conversation, true),
        [command] if command == "receipt" => receipt(),
        _ => Err(anyhow::anyhow!("usage: cepheus hook owner-input listen <conversation> [stop] | receipt")),
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("cepheus: {error:#}");
            1
        }
    }
}

fn connect() -> std::io::Result<UnixStream> {
    let mut stream = UnixStream::connect(paths::socket_path())?;
    write_msg(&mut stream, &ClientMsg::Hello { version: PROTOCOL_VERSION, role: ClientRole::Hook })?;
    Ok(stream)
}

/// Serve the session for as long as Claude runs, connecting again after the
/// service restarts or hands over to a new build.
fn listen(conversation: &str, stop: bool) -> Result<()> {
    let parent = std::os::unix::process::parent_id();
    let mut out = std::io::stdout().lock();
    loop {
        if let Ok(stream) = connect() {
            serve(stream, conversation, stop, &mut out)?;
        }
        // Claude has ended once this process has another parent.
        if std::os::unix::process::parent_id() != parent {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// One connection: each owner message, and each Stop when `stop`, as a JSON
/// line on `out`, until the service closes the connection. A refusal ends
/// the listener.
pub(super) fn serve(mut stream: impl Read + Write, conversation: &str, stop: bool, out: &mut impl Write) -> Result<()> {
    if write_msg(&mut stream, &ClientMsg::OwnerInputListen { conversation: conversation.into(), stop }).is_err() {
        return Ok(());
    }
    let mut reader = BufReader::new(stream);
    loop {
        let line = match read_msg::<_, ServerMsg>(&mut reader) {
            Ok(Some(ServerMsg::OwnerInput { message, conversation, text, files, steer })) => {
                let mut line =
                    serde_json::json!({"message": message, "conversation": conversation, "text": text, "files": files});
                if steer {
                    line["steer"] = true.into();
                }
                line
            }
            Ok(Some(ServerMsg::OwnerStop { message })) if stop => serde_json::json!({"message": message, "stop": true}),
            Ok(Some(ServerMsg::Notice { text, error: true })) => bail!("{text}"),
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return Ok(()),
        };
        writeln!(out, "{line}").and_then(|()| out.flush()).context("Claude's plugin stopped reading")?;
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    message: String,
    outcome: OwnerInputOutcome,
    #[serde(default)]
    reason: Option<String>,
}

fn receipt() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().take(RECEIPT_MAX + 1).read_to_string(&mut input)?;
    anyhow::ensure!(input.len() as u64 <= RECEIPT_MAX, "the receipt is too long");
    let stream = connect().context("the Cepheus service is not running")?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    report(stream, &input)
}

/// Pass one receipt to the service and wait until it takes or refuses it.
pub(super) fn report(mut stream: impl Read + Write, input: &str) -> Result<()> {
    let Receipt { message, outcome, reason } = serde_json::from_str(input).context("invalid receipt")?;
    write_msg(&mut stream, &ClientMsg::OwnerInputReceipt { message, outcome, reason })?;
    let mut reader = BufReader::new(stream);
    loop {
        match read_msg::<_, ServerMsg>(&mut reader)? {
            Some(ServerMsg::Notice { error: false, .. }) => return Ok(()),
            Some(ServerMsg::Notice { text, error: true }) => bail!("{text}"),
            Some(_) => {}
            None => bail!("the Cepheus service closed before taking the receipt"),
        }
    }
}

#[cfg(test)]
#[path = "../../src/tests/claude_input_hook.rs"]
mod tests;
