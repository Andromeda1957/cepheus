//! API-equivalent accounting for the `api-cost` plugin. Reads usage metadata,
//! never conversation text. SQLite keeps usage and byte offsets, so a pass
//! reads only what logs gained since the last one; exported request
//! identities deduplicate copies of a log across hosts. The service runs
//! passes on a thread of its own; the hub pulls each host's new entries over
//! its peer link (`HostReq::ApiCost`).
//!
//! The database, its rows and `summary.json` keep the format the Python
//! collector used, so an existing total carries over.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rusqlite::types::ValueRef;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::HostConfig;
use crate::hosts::wire::{Channel, HostReq, HostResp};

/// USD per million tokens: input, output, cache read. Verified 2026-11-03.
/// <https://platform.claude.com/docs/en/about-claude/pricing>
/// <https://developers.openai.com/api/docs/pricing> (and individual model pages)
/// Written as the Python collector wrote them: its rates-version hash, and so
/// the recorded totals, carry over unchanged.
const RATES: &[(&str, [&str; 3])] = &[
    ("claude-opus-5-5", ["4", "20", "0.2"]),
    ("claude-sonnet-5-5", ["2", "10", "0.1"]),
    ("claude-haiku-5-5", ["0.1", "0.5", "0.01"]),
    ("claude-fable-5-1", ["10", "50", "0.25"]),
    ("claude-mythos-5-1", ["10", "50", "0.25"]),
    ("claude-fable-5", ["10", "50", "1"]),
    ("claude-mythos-5", ["10", "50", "1"]),
    ("claude-opus-5", ["5", "25", "0.5"]),
    ("claude-opus-4-8", ["5", "25", "0.5"]),
    ("claude-opus-4-7", ["5", "25", "0.5"]),
    ("claude-opus-4-6", ["5", "25", "0.5"]),
    ("claude-opus-4-5", ["5", "25", "0.5"]),
    ("claude-opus-4-1", ["15", "75", "1.5"]),
    ("claude-opus-4", ["15", "75", "1.5"]),
    ("claude-sonnet-5", ["2", "10", "0.2"]),
    ("claude-sonnet-4-6", ["3", "15", "0.3"]),
    ("claude-sonnet-4-5", ["3", "15", "0.3"]),
    ("claude-sonnet-4", ["3", "15", "0.3"]),
    ("claude-haiku-4-5", ["1", "5", "0.1"]),
    ("claude-3-5-haiku", ["0.8", "4", "0.08"]),
    ("gpt-6-astra", ["10", "50", "1"]),
    ("gpt-6.1-sol", ["2", "10", "0.1"]),
    ("gpt-6.1-terra", ["1", "5", "0.05"]),
    ("gpt-6-sol", ["2", "10", "0.2"]),
    ("gpt-6-luna", ["0.1", "0.5", "0.01"]),
    ("gpt-5.6-sol", ["4", "20", "0.4"]),
    ("gpt-5.6", ["4", "20", "0.4"]),
    ("gpt-5.6-terra", ["2", "12", "0.2"]),
    ("gpt-5.6-luna", ["0.2", "1.2", "0.02"]),
    // Official Daybreak Blue alias: gpt-5.6-sol.
    ("gpt-daybreak-blue-latest", ["4", "20", "0.4"]),
];
const FIELDS: [&str; 5] =
    ["input_tokens", "cached_input_tokens", "cache_write_input_tokens", "output_tokens", "reasoning_output_tokens"];
pub const SESSION_MODEL: &str = "Claude internal usage";
/// The deck's folders were named this before its rename to Cepheus.
const LEGACY_NAME: &str = "agent-deck"; // legacy-name
/// How long one pass may spend reading logs.
pub const BUDGET: Duration = Duration::from_millis(1500);
/// How long a host may take to answer.
const PEER_TIMEOUT: Duration = Duration::from_secs(20);
/// Entries a host sends per exchange.
const PAGE: i64 = 4000;

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS entries (
      key TEXT PRIMARY KEY, model TEXT NOT NULL, usage TEXT NOT NULL,
      seq INTEGER NOT NULL UNIQUE);
    CREATE TABLE IF NOT EXISTS files (
      path TEXT PRIMARY KEY, inode TEXT, offset INTEGER, size INTEGER,
      mtime INTEGER, context TEXT);
    CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
    CREATE TABLE IF NOT EXISTS peers (
      name TEXT PRIMARY KEY, instance TEXT, cursor INTEGER, status TEXT,
      updated REAL, pending INTEGER);
    CREATE TABLE IF NOT EXISTS totals (
      model TEXT PRIMARY KEY, cost REAL NOT NULL, unpriced INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS claude_requests (
      key TEXT PRIMARY KEY, session TEXT NOT NULL, model TEXT NOT NULL,
      stamp REAL NOT NULL, cost REAL NOT NULL);
    CREATE INDEX IF NOT EXISTS claude_requests_session
      ON claude_requests(session,model,stamp);
    CREATE TABLE IF NOT EXISTS claude_checkpoints (
      session TEXT NOT NULL, model TEXT NOT NULL, reported REAL NOT NULL,
      through REAL NOT NULL, extra REAL NOT NULL DEFAULT 0,
      dirty INTEGER NOT NULL DEFAULT 1, PRIMARY KEY(session,model));
";

/// A host's entries after a cursor, as `HostReq::ApiCost` answers.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Export {
    pub instance: String,
    pub cursor: i64,
    /// Key, model, usage JSON and sequence number.
    pub entries: Vec<(String, String, String, i64)>,
    pub pending: i64,
    pub more: bool,
}

// ---- Numbers and JSON as the Python collector saw them -------------------

/// A JSON number as Python held it: integers stay exact.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    fn f(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }
    fn plus(self, other: Num) -> Num {
        match (self, other) {
            (Num::Int(a), Num::Int(b)) => Num::Int(a.saturating_add(b)),
            _ => Num::Float(self.f() + other.f()),
        }
    }
    fn minus(self, other: Num) -> Num {
        match (self, other) {
            (Num::Int(a), Num::Int(b)) => Num::Int(a.saturating_sub(b)),
            _ => Num::Float(self.f() - other.f()),
        }
    }
    /// Python's `max(0, value)`.
    fn floor0(self) -> Num {
        if self.f() > 0.0 {
            self
        } else {
            Num::Int(0)
        }
    }
    /// Python's `max(self, other)`: the first unless the second is larger.
    fn max(self, other: Num) -> Num {
        if other.f() > self.f() {
            other
        } else {
            self
        }
    }
    fn value(self) -> Value {
        match self {
            Num::Int(i) => Value::from(i),
            Num::Float(f) => Value::from(f),
        }
    }
}

/// The collector's `number`: a non-negative number, else 0.
fn number(value: Option<&Value>) -> Num {
    let found = match value {
        Some(Value::Number(n)) => match n.as_i64() {
            Some(i) => Num::Int(i),
            None => Num::Float(n.as_f64().unwrap_or(0.0)),
        },
        Some(Value::Bool(b)) => Num::Int(*b as i64),
        _ => Num::Int(0),
    };
    found.floor0()
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Python's `a or b`.
fn or<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    if truthy(a) {
        a
    } else {
        b
    }
}

/// Python's `==` between two JSON values: numbers compare by value.
fn same(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
        (None, Some(Value::Null)) | (Some(Value::Null), None) => true,
        (a, b) => a == b,
    }
}

/// Python's `str()` of a JSON value, for keys.
fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => python_json(other, ", ", ": "),
    }
}

/// Python's `repr` of a float.
pub fn python_float(f: f64) -> String {
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    let shortest = format!("{f:e}");
    let (mantissa, exponent) = shortest.split_once('e').unwrap_or((&shortest, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let body = if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        if point <= 0 {
            format!("0.{}{digits}", "0".repeat(-point as usize))
        } else if point as usize >= digits.len() {
            format!("{digits}{}.0", "0".repeat(point as usize - digits.len()))
        } else {
            format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
        }
    } else {
        let mantissa = if digits.len() == 1 { digits.clone() } else { format!("{}.{}", &digits[..1], &digits[1..]) };
        format!("{mantissa}e{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.abs())
    };
    if negative {
        format!("-{body}")
    } else {
        body
    }
}

/// Python's `json.dumps(value, sort_keys=True)` with these separators: ASCII
/// only, keys sorted, floats as `repr`.
pub fn python_json(value: &Value, item: &str, key: &str) -> String {
    let mut out = String::new();
    write_json(&mut out, value, item, key);
    out
}

fn write_json(out: &mut String, value: &Value, item: &str, key: &str) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => out.push_str(&i.to_string()),
            (_, Some(u)) => out.push_str(&u.to_string()),
            _ => out.push_str(&python_float(n.as_f64().unwrap_or(0.0))),
        },
        Value::String(s) => write_string(out, s),
        Value::Array(values) => {
            out.push('[');
            for (i, v) in values.iter().enumerate() {
                if i > 0 {
                    out.push_str(item);
                }
                write_json(out, v, item, key);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(item);
                }
                write_string(out, k);
                out.push_str(key);
                write_json(out, &map[k], item, key);
            }
            out.push('}');
        }
    }
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// The compact form entries are stored in.
fn encode(usage: &Map<String, Value>) -> String {
    python_json(&Value::Object(usage.clone()), ",", ":")
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// The version of the rate table, as the Python collector computed it.
pub fn rates_version() -> String {
    let mut models: Vec<&(&str, [&str; 3])> = RATES.iter().collect();
    models.sort_by_key(|rate| rate.0);
    let body: Vec<String> =
        models.iter().map(|(model, r)| format!("\"{model}\": [{}, {}, {}]", r[0], r[1], r[2])).collect();
    sha256_hex(format!("1{{{}}}", body.join(", ")).as_bytes())
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// An ISO timestamp's Unix seconds, as `datetime.timestamp()`; 0 when invalid.
fn timestamp(value: Option<&Value>) -> Num {
    let Some(text) = value.and_then(Value::as_str) else { return Num::Int(0) };
    match time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339) {
        Ok(at) => {
            let micros = at.unix_timestamp() as i128 * 1_000_000 + (at.nanosecond() / 1000) as i128;
            Num::Float(micros as f64 / 1e6)
        }
        Err(_) => Num::Int(0),
    }
}

/// A model id without its date or context suffix.
pub fn canonical(model: &str) -> &str {
    let bytes = model.as_bytes();
    let dated = |pattern: &[u8]| {
        let len = pattern.len();
        bytes.len() > len
            && bytes[bytes.len() - len - 1] == b'-'
            && bytes[bytes.len() - len..]
                .iter()
                .zip(pattern)
                .all(|(b, p)| if *p == b'd' { b.is_ascii_digit() } else { b == p })
    };
    let model = if dated(b"dddddddd") {
        &model[..model.len() - 9]
    } else if dated(b"dddd-dd-dd") {
        &model[..model.len() - 11]
    } else {
        model
    };
    model.strip_suffix("[1m]").unwrap_or(model)
}

fn rates(model: &str) -> Option<(f64, f64, f64)> {
    let (_, r) = RATES.iter().find(|(m, _)| *m == model)?;
    let parse = |s: &str| s.parse::<f64>().unwrap_or(0.0);
    Some((parse(r[0]), parse(r[1]), parse(r[2])))
}

/// What one entry's usage costs at API rates; none for an unknown model.
pub fn price(model: &str, usage: &Map<String, Value>) -> Option<f64> {
    let model = canonical(model);
    let (mut inp, mut out, mut read) = rates(model)?;
    let get = |k: &str| number(usage.get(k)).f();
    let (i, r, w, h, o, s) = (get("i"), get("r"), get("w"), get("h"), get("o"), get("s"));
    if model == "claude-haiku-5-5" && i + r + w + h > 100_000.0 {
        (inp, out, read) = (0.5, 2.5, 0.05);
    }
    let (mut input_mult, mut output_mult) = (1.0, 1.0);
    if model.starts_with("gpt-") && get("prompt") > 272_000.0 {
        (input_mult, output_mult) = (2.0, 1.5);
    }
    let mut mult = match usage.get("speed").and_then(Value::as_str) {
        Some("fast" | "priority") => 2.0,
        Some("ultrafast") => 6.0,
        Some("flex" | "batch") => 0.5,
        _ => 1.0,
    };
    if usage.get("geo").and_then(Value::as_str) == Some("us") {
        mult *= 1.1;
    }
    let hour = if model.starts_with("claude-") { 2.0 } else { 2.5 };
    Some(
        ((i * inp + r * read + w * inp * 1.25 + h * inp * hour) * input_mult + o * out * output_mult) / 1_000_000.0
            * mult
            + s * 0.01,
    )
}

// ---- The database ----------------------------------------------------------

fn real(value: ValueRef) -> f64 {
    match value {
        ValueRef::Integer(i) => i as f64,
        ValueRef::Real(f) => f,
        ValueRef::Text(t) => std::str::from_utf8(t).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0.0),
        _ => 0.0,
    }
}

fn integer(value: ValueRef) -> i64 {
    match value {
        ValueRef::Integer(i) => i,
        ValueRef::Real(f) => f as i64,
        ValueRef::Text(t) => std::str::from_utf8(t).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0),
        _ => 0,
    }
}

fn meta(db: &Connection, key: &str) -> Result<Option<String>> {
    Ok(db
        .query_row("SELECT value FROM meta WHERE key=?1", [key], |row| {
            Ok(match row.get_ref(0)? {
                ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                ValueRef::Integer(i) => i.to_string(),
                ValueRef::Real(f) => f.to_string(),
                _ => String::new(),
            })
        })
        .optional()?)
}

fn pending(db: &Connection) -> Result<i64> {
    Ok(meta(db, "pending")?.and_then(|p| p.trim().parse().ok()).unwrap_or(0))
}

/// Run `body` in one transaction, as the Python collector's implicit ones.
fn transaction<T>(db: &Connection, body: impl FnOnce() -> Result<T>) -> Result<T> {
    db.execute_batch("BEGIN")?;
    match body() {
        Ok(value) => {
            db.execute_batch("COMMIT")?;
            Ok(value)
        }
        Err(error) => {
            let _ = db.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn private_dir(root: &Path) -> Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(root).with_context(|| format!("{}", root.display()))
}

pub fn connect(root: &Path) -> Result<Connection> {
    private_dir(root)?;
    let path = root.join("usage.sqlite3");
    if !path.exists() {
        OpenOptions::new().create(true).append(true).mode(0o600).open(&path)?;
    }
    let db = Connection::open(&path)?;
    db.busy_timeout(Duration::from_secs(2))?;
    db.execute_batch(SCHEMA)?;
    transaction(&db, || {
        let mut instance = [0u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut instance)
            .map_err(|_| anyhow::anyhow!("no random instance id"))?;
        let instance: String = instance.iter().map(|b| format!("{b:02x}")).collect();
        db.execute("INSERT OR IGNORE INTO meta VALUES ('instance', ?1)", [instance])?;
        if meta(&db, "claude-accounting")?.as_deref() != Some("2") {
            // Re-read existing logs to attach session/timestamp metadata and
            // discover saved costs. Keep every receipt, peer cursor and total.
            db.execute("UPDATE files SET offset=0,context='{}'", [])?;
            db.execute("INSERT OR REPLACE INTO meta VALUES ('claude-accounting','2')", [])?;
        }
        let version = rates_version();
        if meta(&db, "rates-version")?.as_deref() != Some(version.as_str()) {
            db.execute("DELETE FROM totals", [])?;
            let entries: Vec<(String, String)> = db
                .prepare("SELECT model,usage FROM entries")?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (model, encoded) in entries {
                if let Ok(Value::Object(usage)) = serde_json::from_str(&encoded) {
                    adjust_total(&db, &model, &usage, 1.0)?;
                }
            }
            let requests: Vec<(String, String, String)> = db
                .prepare("SELECT e.key,e.model,e.usage FROM entries e JOIN claude_requests r ON e.key=r.key")?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (key, model, encoded) in requests {
                let cost = match serde_json::from_str(&encoded) {
                    Ok(Value::Object(usage)) => price(&model, &usage).unwrap_or(0.0),
                    _ => 0.0,
                };
                db.execute("UPDATE claude_requests SET cost=?1 WHERE key=?2", params![cost, key])?;
            }
            db.execute("UPDATE claude_checkpoints SET dirty=1", [])?;
            db.execute("INSERT OR REPLACE INTO meta VALUES ('rates-version',?1)", [version])?;
        }
        Ok(())
    })?;
    Ok(db)
}

fn adjust_total(db: &Connection, model: &str, usage: &Map<String, Value>, direction: f64) -> Result<()> {
    if usage.contains_key("reported_cost") {
        return Ok(());
    }
    let cost = price(model, usage);
    db.prepare_cached(
        "INSERT INTO totals VALUES (?1,?2,?3) ON CONFLICT(model) DO UPDATE SET \
         cost=cost+excluded.cost,unpriced=unpriced+excluded.unpriced",
    )?
    .execute(params![model, direction * cost.unwrap_or(0.0), direction as i64 * cost.is_none() as i64])?;
    Ok(())
}

fn irwho(usage: &Map<String, Value>) -> Num {
    ["i", "r", "w", "h", "o"].iter().fold(Num::Int(0), |sum, k| sum.plus(number(usage.get(*k))))
}

/// Record one entry: a newer streaming snapshot, a saved session total or a
/// copy from another host, adjusting the running totals.
pub fn put(db: &Connection, key: &str, model: &str, usage: Map<String, Value>) -> Result<()> {
    let (mut model, mut usage) = (model.to_string(), usage);
    let old: Option<(String, String)> = db
        .prepare_cached("SELECT model, usage FROM entries WHERE key=?1")?
        .query_row([key], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    if let Some((old_model, old_usage)) = &old {
        let previous = match serde_json::from_str(old_usage) {
            Ok(Value::Object(previous)) => previous,
            _ => Map::new(),
        };
        if usage.contains_key("reported_cost") {
            // An idle/resumed session may save the same total again after
            // newer receipts; its original coverage boundary must not move.
            let reported = usage.get("reported_cost");
            if number(reported).f() < number(previous.get("reported_cost")).f()
                || (same(reported, previous.get("reported_cost"))
                    && number(usage.get("through")).f() >= number(previous.get("through")).f())
            {
                return Ok(());
            }
        } else {
            // Keep the largest streaming snapshot while allowing older
            // receipts to gain the metadata checkpoint reconciliation needs.
            let metadata: Vec<(&str, Option<Value>)> = ["session", "stamp"]
                .into_iter()
                .map(|k| (k, previous.get(k).or_else(|| usage.get(k)).cloned()))
                .collect();
            if irwho(&usage).f() <= irwho(&previous).f() {
                usage = previous.clone();
                model = old_model.clone();
            }
            for (k, v) in metadata {
                if let Some(v) = v.filter(|v| !v.is_null()) {
                    usage.insert(k.into(), v);
                }
            }
        }
        if *old_model == model && *old_usage == encode(&usage) {
            return Ok(());
        }
        adjust_total(db, old_model, &previous, -1.0)?;
    }
    let encoded = encode(&usage);
    let seq: i64 = db.prepare_cached("SELECT COALESCE(MAX(seq),0)+1 FROM entries")?.query_row([], |row| row.get(0))?;
    db.prepare_cached(
        "INSERT INTO entries VALUES (?1,?2,?3,?4) ON CONFLICT(key) DO UPDATE SET \
         model=excluded.model,usage=excluded.usage,seq=excluded.seq",
    )?
    .execute(params![key, model, encoded, seq])?;
    adjust_total(db, &model, &usage, 1.0)?;
    let session = usage.get("session").filter(|s| truthy(Some(s))).map(text);
    if let Some(session) = session {
        if key.starts_with("claude-checkpoint:") {
            db.prepare_cached(
                "INSERT INTO claude_checkpoints(session,model,reported,through) VALUES (?1,?2,?3,?4) \
                 ON CONFLICT(session,model) DO UPDATE SET reported=excluded.reported,through=excluded.through,dirty=1",
            )?
            .execute(params![
                session,
                model,
                usage.get("reported_cost").and_then(Value::as_f64).unwrap_or(0.0),
                number(usage.get("through")).f()
            ])?;
        } else if key.starts_with("claude:") {
            db.prepare_cached(
                "INSERT INTO claude_requests VALUES (?1,?2,?3,?4,?5) ON CONFLICT(key) DO UPDATE SET \
                 session=excluded.session,model=excluded.model,stamp=excluded.stamp,cost=excluded.cost",
            )?
            .execute(params![
                key,
                session,
                model,
                number(usage.get("stamp")).f(),
                price(&model, &usage).unwrap_or(0.0)
            ])?;
            db.prepare_cached("UPDATE claude_checkpoints SET dirty=1 WHERE session=?1 AND model IN (?2,?3)")?
                .execute(params![session, model, SESSION_MODEL])?;
        }
    }
    Ok(())
}

// ---- Reading logs ------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
}

fn object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

/// `value or {}`: none when it is truthy but not an object.
fn object_or_empty(value: Option<&Value>) -> Option<Map<String, Value>> {
    if !truthy(value) {
        Some(Map::new())
    } else {
        object(value).cloned()
    }
}

fn usage_map(pairs: Vec<(&str, Value)>) -> Map<String, Value> {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// Fold one log record into the database. Records of other kinds, and
/// malformed ones, are skipped.
pub fn consume(
    db: &Connection,
    record: &Value,
    context: &mut Map<String, Value>,
    provider: Provider,
    fallback: &str,
) -> Result<()> {
    let Some(record) = record.as_object() else { return Ok(()) };
    match provider {
        Provider::Claude => claude(db, record, context, fallback),
        Provider::Codex => codex(db, record, context, fallback),
    }
}

fn claude(
    db: &Connection,
    record: &Map<String, Value>,
    context: &mut Map<String, Value>,
    fallback: &str,
) -> Result<()> {
    let through = number(context.get("through")).max(timestamp(record.get("timestamp")));
    context.insert("through".into(), through.value());
    let fallback_value = Value::String(fallback.into());
    let session = or(or(record.get("sessionId"), context.get("session")), Some(&fallback_value)).cloned().unwrap();
    let kind = record.get("type").and_then(Value::as_str);
    if kind == Some("cost-state") {
        // Claude counts internal summarization/helper calls that never become
        // assistant messages. Save its cumulative session dollars, rather
        // than adding the whole session on top of individual receipts.
        let cost = record.get("totalCostUSD").filter(|c| c.is_number());
        if let Some(value) = cost.and_then(Value::as_f64) {
            if value.is_finite() && value >= 0.0 {
                let through = match record.get("through") {
                    Some(through) => number(Some(through)),
                    None => number(context.get("through")),
                };
                let usage = usage_map(vec![
                    ("reported_cost", cost.cloned().unwrap()),
                    ("session", session.clone()),
                    ("through", through.value()),
                ]);
                put(db, &format!("claude-checkpoint:{}:@total", text(&session)), SESSION_MODEL, usage)?;
            }
        }
        return Ok(());
    }
    if kind != Some("assistant") {
        return Ok(());
    }
    let Some(message) = object_or_empty(record.get("message")) else { return Ok(()) };
    let Some(u) = object_or_empty(message.get("usage")) else { return Ok(()) };
    let unknown = Value::String("unknown-claude".into());
    let Some(model) = or(message.get("model"), Some(&unknown)).and_then(Value::as_str) else { return Ok(()) };
    if u.is_empty() || model == "<synthetic>" {
        return Ok(());
    }
    let Some(cache) = object_or_empty(u.get("cache_creation")) else { return Ok(()) };
    let hour = number(cache.get("ephemeral_1h_input_tokens"));
    let write = number(u.get("cache_creation_input_tokens")).minus(hour).floor0();
    let Some(tools) = object_or_empty(u.get("server_tool_use")) else { return Ok(()) };
    let usage = usage_map(vec![
        ("i", number(u.get("input_tokens")).value()),
        ("r", number(u.get("cache_read_input_tokens")).value()),
        ("w", write.value()),
        ("h", hour.value()),
        ("o", number(u.get("output_tokens")).value()),
        ("s", number(tools.get("web_search_requests")).value()),
        ("speed", u.get("speed").cloned().unwrap_or_else(|| "standard".into())),
        ("geo", u.get("inference_geo").cloned().unwrap_or_else(|| "global".into())),
        ("session", session),
        ("stamp", timestamp(record.get("timestamp")).value()),
    ]);
    let identity = or(or(message.get("id"), record.get("uuid")), Some(&fallback_value)).unwrap();
    put(db, &format!("claude:{}", text(identity)), model, usage)
}

fn codex(db: &Connection, record: &Map<String, Value>, context: &mut Map<String, Value>, fallback: &str) -> Result<()> {
    let kind = record.get("type").and_then(Value::as_str);
    if !matches!(kind, Some("session_meta" | "turn_context" | "event_msg")) {
        return Ok(());
    }
    let Some(p) = object_or_empty(record.get("payload")) else { return Ok(()) };
    match kind {
        Some("session_meta") => {
            context.insert("session".into(), p.get("id").cloned().unwrap_or_else(|| fallback.into()));
        }
        Some("turn_context") => {
            let model =
                p.get("model").or_else(|| context.get("model")).cloned().unwrap_or_else(|| "unknown-codex".into());
            context.insert("model".into(), model);
            context.insert("speed".into(), p.get("service_tier").cloned().unwrap_or_else(|| "standard".into()));
        }
        _ => {
            if p.get("type").and_then(Value::as_str) != Some("token_count") || !truthy(p.get("info")) {
                return Ok(());
            }
            let Some(info) = object(p.get("info")) else { return Ok(()) };
            let (total, last) = (info.get("total_token_usage"), info.get("last_token_usage"));
            let previous = context.get("total").cloned();
            if !truthy(total) {
                return Ok(());
            }
            let total = total.cloned().unwrap();
            context.insert("total".into(), total.clone());
            if Some(&total) == previous.as_ref() {
                return Ok(());
            }
            let Some(totals) = total.as_object() else { return Ok(()) };
            // Cumulative counters are repeated on rate-limit updates and
            // resumes. Delta the counters; use the last request after an
            // explicit reset.
            let grown =
                previous.as_ref().filter(|p| truthy(Some(p))).and_then(Value::as_object).filter(|previous| {
                    FIELDS.iter().all(|k| number(totals.get(*k)).f() >= number(previous.get(*k)).f())
                });
            let u: Map<String, Value> = match grown {
                Some(previous) => FIELDS
                    .iter()
                    .map(|k| (k.to_string(), number(totals.get(*k)).minus(number(previous.get(*k))).value()))
                    .collect(),
                None => match object(or(last, Some(&total))) {
                    Some(u) => u.clone(),
                    None => return Ok(()),
                },
            };
            if !FIELDS.iter().any(|k| number(u.get(*k)).f() != 0.0) {
                return Ok(());
            }
            let Some(prompt_source) = (if truthy(last) { object(last) } else { Some(&u) }) else { return Ok(()) };
            let prompt = number(prompt_source.get("input_tokens"));
            let cached = number(u.get("cached_input_tokens"));
            let write = number(u.get("cache_write_input_tokens"));
            let mut output = number(u.get("output_tokens"));
            if info.get("last_usage_includes_reasoning") == Some(&Value::Bool(false)) {
                output = output.plus(number(u.get("reasoning_output_tokens")));
            }
            let speed = or(context.get("speed"), None).cloned().unwrap_or_else(|| "standard".into());
            let usage = usage_map(vec![
                ("i", number(u.get("input_tokens")).minus(cached).minus(write).floor0().value()),
                ("r", cached.value()),
                ("w", write.value()),
                ("h", Value::from(0)),
                ("o", output.value()),
                ("s", Value::from(0)),
                ("prompt", prompt.value()),
                ("speed", speed),
            ]);
            // Original timestamps and cumulative usage survive forks and host copies.
            let session = context.get("session").cloned().unwrap_or_else(|| fallback.into());
            let first = or(record.get("timestamp"), Some(&session)).cloned().unwrap();
            let identity = python_json(&Value::Array(vec![first, total.clone()]), ", ", ": ");
            let key = format!("codex:{}", sha256_hex(identity.as_bytes()));
            let unknown = Value::String("unknown-codex".into());
            let Some(model) = context.get("model").unwrap_or(&unknown).as_str() else { return Ok(()) };
            put(db, &key, model, usage)?;
        }
    }
    Ok(())
}

/// Where usage logs and status-line snapshots are found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Roots {
    pub logs: Vec<(PathBuf, Provider)>,
    /// Folders of `claude-*.json` status-line snapshots.
    pub snapshots: Vec<PathBuf>,
}

impl Roots {
    pub fn from_env() -> Roots {
        let home = crate::paths::home();
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        let mut logs = vec![
            (home.join(".claude/projects"), Provider::Claude),
            (home.join(".codex/sessions"), Provider::Codex),
            (home.join(".codex/archived_sessions"), Provider::Codex),
        ];
        // A launcher can override config paths (including a deliberately
        // nonexistent Claude config). Historical logs in the normal home
        // still belong in this read-only accounting pass.
        if let Some(dir) = var("CLAUDE_CONFIG_DIR") {
            logs.push((dir.join("projects"), Provider::Claude));
        }
        if let Some(dir) = var("CODEX_HOME") {
            logs.push((dir.join("sessions"), Provider::Codex));
            logs.push((dir.join("archived_sessions"), Provider::Codex));
        }
        let mut bases = vec![home.join(".local/state")];
        bases.extend(var("XDG_STATE_HOME"));
        let mut snapshots: Vec<PathBuf> = bases
            .iter()
            .flat_map(|base| ["cepheus", LEGACY_NAME].map(|name| base.join(name).join("api-cost")))
            .collect();
        for name in ["CEPHEUS_STATE_DIR", "AGENT_DECK_STATE_DIR"] {
            // legacy-name
            // legacy-name
            snapshots.extend(var(name).map(|dir| dir.join("api-cost")));
        }
        let mut seen = HashSet::new();
        snapshots.retain(|dir| seen.insert(dir.clone()));
        Roots { logs, snapshots }
    }

    /// Every log and snapshot file, with its provider and metadata.
    fn files(&self) -> Vec<(PathBuf, Provider, fs::Metadata)> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for (root, provider) in &self.logs {
            if !seen.insert(fs::canonicalize(root).unwrap_or_else(|_| root.clone())) {
                continue;
            }
            let mut folders = vec![root.clone()];
            while let Some(folder) = folders.pop() {
                let Ok(entries) = fs::read_dir(&folder) else { continue };
                for entry in entries.flatten() {
                    let Ok(kind) = entry.file_type() else { continue };
                    let path = entry.path();
                    if kind.is_dir() {
                        folders.push(path);
                    } else if path.extension().is_some_and(|e| e == "jsonl") {
                        if let Some(meta) = fs::metadata(&path).ok().filter(fs::Metadata::is_file) {
                            out.push((path, *provider, meta));
                        }
                    }
                }
            }
        }
        // Status-line snapshots contain only accounting metadata, never messages.
        for folder in &self.snapshots {
            let Ok(entries) = fs::read_dir(folder) else { continue };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("claude-") && name.ends_with(".json") {
                    if let Some(meta) = fs::metadata(entry.path()).ok().filter(fs::Metadata::is_file) {
                        out.push((entry.path(), Provider::Claude, meta));
                    }
                }
            }
        }
        out
    }
}

fn mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.mtime().saturating_mul(1_000_000_000).saturating_add(meta.mtime_nsec())
}

/// Whether a line can hold usage. Large tool outputs and images stay
/// unparsed; small Claude metadata records also anchor saved cost coverage.
fn relevant(line: &[u8], provider: Provider) -> bool {
    let has = |haystack: &[u8], needle: &[u8]| memchr::memmem::find(haystack, needle).is_some();
    match provider {
        Provider::Claude => {
            (has(line, b"\"usage\"") && has(line, b"\"model\""))
                || has(&line[..line.len().min(100)], b"\"cost-state\"")
                || (line.len() <= 65536 && has(line, b"\"timestamp\""))
        }
        Provider::Codex => {
            let head = &line[..line.len().min(250)];
            has(head, b"\"turn_context\"")
                || has(head, b"\"session_meta\"")
                || (has(head, b"\"event_msg\"") && has(head, b"\"token_count\""))
        }
    }
}

struct Known {
    inode: Option<String>,
    offset: i64,
    size: i64,
    mtime: i64,
    context: String,
}

/// Read the complete lines a file gained after `offset`, until `deadline`:
/// the new offset, or none when the file could not be read.
fn read_lines(
    db: &Connection,
    path: &Path,
    mut offset: i64,
    context: &mut Map<String, Value>,
    provider: Provider,
    deadline: Instant,
) -> Result<Option<i64>> {
    let Ok(mut file) = File::open(path) else { return Ok(None) };
    if file.seek(SeekFrom::Start(offset as u64)).is_err() {
        return Ok(None);
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    while Instant::now() < deadline {
        let start = offset;
        line.clear();
        let Ok(read) = reader.read_until(b'\n', &mut line) else { return Ok(None) };
        if read == 0 || line.last() != Some(&b'\n') {
            break;
        }
        offset += read as i64;
        if relevant(&line, provider) {
            if let Ok(record) = serde_json::from_slice::<Value>(&line) {
                consume(db, &record, context, provider, &format!("{name}:{start}"))?;
            }
        }
    }
    Ok(Some(offset))
}

/// Read what logs gained since the last pass, for at most `budget`, files
/// already part-read first, then the most recently changed. Returns how many
/// files still have unread bytes.
pub fn scan(db: &Connection, roots: &Roots, budget: Duration) -> Result<i64> {
    let deadline = Instant::now() + budget;
    let known: HashMap<String, Known> = db
        .prepare("SELECT path,inode,offset,size,mtime,context FROM files")?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Known {
                    inode: row.get(1)?,
                    offset: integer(row.get_ref(2)?),
                    size: integer(row.get_ref(3)?),
                    mtime: integer(row.get_ref(4)?),
                    context: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut candidates = Vec::new();
    for (path, provider, meta) in roots.files() {
        let name = path.to_string_lossy().into_owned();
        let old = known.get(&name);
        let (size, mtime) = (meta.size() as i64, mtime_ns(&meta));
        if old.is_some_and(|old| old.offset >= size && old.size == size && old.mtime == mtime) {
            continue;
        }
        // Finish partly indexed files before beginning more historical files.
        let order = if old.is_some_and(|old| old.offset < old.size) { 0 } else { 1 };
        candidates.push(((order, -mtime), name, path, provider, meta));
    }
    candidates.sort_by_key(|candidate| candidate.0);
    let mut unread = candidates.len() as i64;
    transaction(db, || {
        for (_, name, path, provider, meta) in candidates {
            if Instant::now() >= deadline {
                break;
            }
            let inode = format!("{}:{}", meta.dev(), meta.ino());
            let size = meta.size() as i64;
            let (mut offset, mut context) = (0i64, Map::new());
            let resumable = known.get(&name).filter(|old| old.inode.as_deref() == Some(&inode) && size >= old.offset);
            if let Some(old) = resumable {
                offset = old.offset;
                if let Ok(Value::Object(saved)) = serde_json::from_str(&old.context) {
                    context = saved;
                }
            }
            if provider == Provider::Claude && !context.contains_key("session") {
                let parent = path.parent();
                let session = if parent.and_then(Path::file_name).is_some_and(|n| n == "subagents") {
                    parent.and_then(Path::parent).and_then(Path::file_name)
                } else {
                    path.file_stem()
                };
                context.insert(
                    "session".into(),
                    session.map(|n| n.to_string_lossy().into_owned()).unwrap_or_default().into(),
                );
            }
            let Some(offset) = read_lines(db, &path, offset, &mut context, provider, deadline)? else { continue };
            db.prepare_cached("INSERT OR REPLACE INTO files VALUES (?1,?2,?3,?4,?5,?6)")?.execute(params![
                name,
                inode,
                offset,
                size,
                mtime_ns(&meta),
                python_json(&Value::Object(context), ", ", ": ")
            ])?;
            if offset >= size {
                unread -= 1;
            }
        }
        db.execute("INSERT OR REPLACE INTO meta VALUES ('pending',?1)", [unread.to_string()])?;
        Ok(())
    })?;
    Ok(unread)
}

/// This host's entries after `cursor`; from the start when `instance` names
/// another database.
pub fn export(db: &Connection, instance: &str, cursor: i64) -> Result<Export> {
    let own = meta(db, "instance")?.unwrap_or_default();
    let cursor = if own == instance { cursor } else { 0 };
    let entries: Vec<(String, String, String, i64)> = db
        .prepare("SELECT key,model,usage,seq FROM entries WHERE seq>?1 ORDER BY seq LIMIT ?2")?
        .query_map(params![cursor, PAGE], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let end = entries.last().map_or(cursor, |e| e.3);
    let more = db.query_row("SELECT 1 FROM entries WHERE seq>?1 LIMIT 1", [end], |_| Ok(())).optional()?.is_some();
    Ok(Export { instance: own, cursor: end, entries, pending: pending(db)?, more })
}

/// Take in a host's exported entries and record its standing.
pub fn import(db: &Connection, name: &str, export: &Export) -> Result<()> {
    transaction(db, || {
        for (key, model, encoded, _) in &export.entries {
            let Value::Object(usage) = serde_json::from_str(encoded)? else { bail!("invalid exported usage") };
            put(db, key, model, usage)?;
        }
        db.execute(
            "INSERT OR REPLACE INTO peers VALUES (?1,?2,?3,?4,?5,?6)",
            params![name, export.instance, export.cursor, "online", now(), export.pending + export.more as i64],
        )?;
        Ok(())
    })
}

fn offline(db: &Connection, name: &str) -> Result<()> {
    transaction(db, || {
        db.execute("INSERT OR IGNORE INTO peers VALUES (?1,'',0,'offline',0,0)", [name])?;
        db.execute("UPDATE peers SET status='offline' WHERE name=?1", [name])?;
        Ok(())
    })
}

/// The grand total by model, with each host's standing. Once nothing is
/// left to index, saved session totals add what their receipts missed.
pub fn summary(db: &Connection) -> Result<Value> {
    let mut models: Vec<(String, f64)> = Vec::new();
    let mut unknown = Map::new();
    let totals: Vec<(String, f64, i64)> = db
        .prepare("SELECT model,cost,unpriced FROM totals")?
        .query_map([], |row| Ok((row.get(0)?, real(row.get_ref(1)?), integer(row.get_ref(2)?))))?
        .collect::<rusqlite::Result<_>>()?;
    for (model, cost, unpriced) in totals {
        if unpriced != 0 {
            unknown.insert(model.clone(), unpriced.into());
        }
        if cost != 0.0 {
            models.push((model, cost.max(0.0)));
        }
    }
    let peers: Vec<(String, String, f64, i64)> = db
        .prepare("SELECT name,status,updated,pending FROM peers")?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                real(row.get_ref(2)?),
                integer(row.get_ref(3)?),
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let unread = pending(db)?;
    // Wait for backfill/import to finish before deriving corrections: old raw
    // receipts already contribute to totals while their metadata is replayed.
    if unread == 0 && peers.iter().all(|p| p.3 == 0) {
        transaction(db, || {
            let dirty: Vec<(String, String, f64, f64)> = db
                .prepare("SELECT session,model,reported,through FROM claude_checkpoints WHERE dirty=1")?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, real(row.get_ref(2)?), real(row.get_ref(3)?))))?
                .collect::<rusqlite::Result<_>>()?;
            for (session, model, reported, through) in dirty {
                let covered = if model == SESSION_MODEL {
                    db.query_row(
                        "SELECT COALESCE(SUM(cost),0) FROM claude_requests WHERE session=?1 AND stamp<=?2",
                        params![session, through],
                        |row| Ok(real(row.get_ref(0)?)),
                    )?
                } else {
                    db.query_row(
                        "SELECT COALESCE(SUM(cost),0) FROM claude_requests WHERE session=?1 AND model=?2 AND stamp<=?3",
                        params![session, model, through],
                        |row| Ok(real(row.get_ref(0)?)),
                    )?
                };
                db.execute(
                    "UPDATE claude_checkpoints SET extra=?1,dirty=0 WHERE session=?2 AND model=?3",
                    params![(reported - covered).max(0.0), session, model],
                )?;
            }
            Ok(())
        })?;
    }
    let extras: Vec<(String, f64)> = db
        .prepare(
            "SELECT c.model,SUM(c.extra) FROM claude_checkpoints c WHERE c.model=?1 OR NOT EXISTS \
             (SELECT 1 FROM claude_checkpoints s WHERE s.session=c.session AND s.model=?1) GROUP BY c.model",
        )?
        .query_map([SESSION_MODEL], |row| Ok((row.get(0)?, real(row.get_ref(1)?))))?
        .collect::<rusqlite::Result<_>>()?;
    for (model, extra) in extras {
        if extra != 0.0 {
            match models.iter_mut().find(|(m, _)| *m == model) {
                Some((_, cost)) => *cost += extra,
                None => models.push((model, extra)),
            }
        }
    }
    let total: f64 = models.iter().map(|(_, cost)| cost).sum();
    let peers: Vec<Value> = peers
        .into_iter()
        .map(|(name, status, updated, pending)| {
            serde_json::json!({"name": name, "status": status, "updated": updated, "pending": pending})
        })
        .collect();
    Ok(serde_json::json!({
        "total": total,
        "models": models.into_iter().map(|(m, c)| (m, Value::from(c))).collect::<Map<_, _>>(),
        "unknown": unknown,
        "peers": peers,
        "pending": unread,
        "updated": now(),
        "currency": "USD",
    }))
}

// ---- Passes ----------------------------------------------------------------------

/// `collector.lock`, held for a pass so two collectors never index at once.
struct Lock(File);

impl Lock {
    fn try_take(root: &Path) -> Result<Option<Lock>> {
        let file = OpenOptions::new().create(true).append(true).mode(0o600).open(root.join("collector.lock"))?;
        // SAFETY: a valid open descriptor.
        let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        Ok(taken.then_some(Lock(file)))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: a valid open descriptor.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// A host's half of an exchange: index what it can, then export.
pub fn peer_export(state_dir: &Path, roots: &Roots, instance: &str, cursor: i64) -> Result<Export> {
    let root = state_dir.join("api-cost");
    private_dir(&root)?;
    let lock = Lock::try_take(&root)?;
    let db = connect(&root)?;
    if lock.is_some() {
        scan(&db, roots, BUDGET)?;
    }
    export(&db, instance, cursor)
}

/// How the hub reaches a host: its config, `instance` and `cursor` in, the
/// host's export out.
pub type Exchange<'a> = &'a mut dyn FnMut(&HostConfig, &str, i64) -> Result<Export>;

/// The service's collector: one database connection and one peer link per
/// host, kept between passes.
pub struct Collector {
    root: PathBuf,
    roots: Roots,
    db: Option<Connection>,
    written: Option<Value>,
    links: HashMap<String, (HostConfig, UnixStream)>,
}

impl Collector {
    pub fn new(root: PathBuf, roots: Roots) -> Collector {
        Collector { root, roots, db: None, written: None, links: HashMap::new() }
    }

    /// One pass over this host's logs and each host's new entries. Another
    /// collector holding the lock leaves the last saved summary.
    pub fn pass(&mut self, hosts: &[HostConfig], exchange: Exchange) -> Result<Value> {
        private_dir(&self.root)?;
        let Some(_lock) = Lock::try_take(&self.root)? else {
            return Ok(fs::read(self.root.join("summary.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_else(
                    || serde_json::json!({"total": 0, "pending": 1, "models": {}, "unknown": {}, "peers": []}),
                ));
        };
        if self.db.is_none() {
            self.db = Some(connect(&self.root)?);
        }
        let db = self.db.as_ref().unwrap();
        scan(db, &self.roots, BUDGET)?;
        for host in hosts {
            let standing: Option<(Option<String>, i64)> = db
                .query_row("SELECT instance,cursor FROM peers WHERE name=?1", [&host.name], |row| {
                    Ok((row.get(0)?, integer(row.get_ref(1)?)))
                })
                .optional()?;
            let (instance, cursor) = standing.map_or((String::new(), 0), |(i, c)| (i.unwrap_or_default(), c));
            if exchange(host, &instance, cursor).and_then(|export| import(db, &host.name, &export)).is_err() {
                offline(db, &host.name)?;
            }
        }
        let result = summary(db)?;
        let mut unchanged = result.clone();
        unchanged["updated"] = Value::Null;
        if self.written.as_ref() != Some(&unchanged) {
            let staging = self.root.join("summary.next");
            let mut file = OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&staging)?;
            file.write_all(serde_json::to_string(&result)?.as_bytes())?;
            drop(file);
            fs::rename(&staging, self.root.join("summary.json"))?;
            self.written = Some(unchanged);
        }
        Ok(result)
    }

    /// A pass that reaches hosts over their peer links, kept open between
    /// passes and reopened once when broken.
    pub fn pass_over_links(&mut self, hosts: &[HostConfig], key: Option<String>) -> Result<Value> {
        let mut links = std::mem::take(&mut self.links);
        links.retain(|name, (config, _)| hosts.iter().any(|h| &h.name == name && h == config));
        let result = self.pass(hosts, &mut |host, instance, cursor| {
            let key = key.as_deref().context("no hub key")?;
            let request = HostReq::ApiCost { instance: instance.into(), cursor };
            let mut attempt = 0;
            loop {
                attempt += 1;
                if !links.contains_key(&host.name) {
                    let (stream, _) = crate::hosts::open(host, key, Channel::Host)?;
                    stream.set_read_timeout(Some(PEER_TIMEOUT))?;
                    stream.set_write_timeout(Some(PEER_TIMEOUT))?;
                    links.insert(host.name.clone(), (host.clone(), stream));
                }
                let (_, stream) = links.get_mut(&host.name).unwrap();
                match crate::hosts::upkeep::ask(stream, &request, None) {
                    Ok(HostResp::ApiCost { export }) => return Ok(export),
                    Ok(HostResp::Error { text }) => bail!("{text}"),
                    Ok(other) => {
                        links.remove(&host.name);
                        bail!("{}", crate::hosts::upkeep::unexpected(&other));
                    }
                    Err(broken) => {
                        links.remove(&host.name);
                        if attempt == 2 {
                            bail!("{broken}");
                        }
                    }
                }
            }
        });
        self.links = links;
        result
    }
}

#[cfg(test)]
#[path = "../../src/tests/api_cost.rs"]
mod tests;
