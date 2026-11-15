use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;
use crate::protocol::{Status, Tool};
use crate::remote::chat::{Cache, Dirs, Source};
use crate::testsupport::TempDir;

/// A protobuf field, as Antigravity writes its steps.
enum F {
    V(u64, u64),
    B(u64, Vec<u8>),
}

fn put_varint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn msg(fields: &[F]) -> Vec<u8> {
    let mut out = Vec::new();
    for f in fields {
        match f {
            F::V(n, v) => {
                put_varint(n << 3, &mut out);
                put_varint(*v, &mut out);
            }
            F::B(n, b) => {
                put_varint(n << 3 | 2, &mut out);
                put_varint(b.len() as u64, &mut out);
                out.extend_from_slice(b);
            }
        }
    }
    out
}

fn s(n: u64, text: &str) -> F {
    F::B(n, text.as_bytes().to_vec())
}

fn m(n: u64, fields: &[F]) -> F {
    F::B(n, msg(fields))
}

/// Step metadata created at `secs` (and `nanos`), naming tool call `call`.
fn meta(secs: u64, nanos: u64, call: Option<&str>) -> F {
    let mut fields = vec![m(1, &[F::V(1, secs), F::V(2, nanos)])];
    if let Some(call) = call {
        fields.push(m(4, &[s(1, call), s(2, "run_command")]));
    }
    F::B(5, msg(&fields))
}

fn prompt(text: &str) -> Vec<u8> {
    msg(&[F::V(1, 14), F::V(4, 3), meta(1_793_567_781, 840_549_013, None), m(19, &[s(2, text), m(3, &[s(1, text)])])])
}

fn reply(said: &str) -> Vec<u8> {
    msg(&[
        F::V(1, 15),
        meta(1_793_567_790, 0, None),
        m(
            20,
            &[
                s(1, said),
                s(3, "Checking the tests first."),
                m(
                    7,
                    &[
                        s(1, "call_1"),
                        s(2, "run_command"),
                        s(3, r#"{"CommandLine":"cargo test","toolSummary":"Run tests"}"#),
                    ],
                ),
                m(7, &[s(1, "call_2"), s(2, "view_file"), s(3, r#"{"AbsolutePath":"/srv/repo/src/main.rs"}"#)]),
                // The thinking's signature: bytes that are not text.
                F::B(14, vec![0x12, 0xcf, 0x01, 0xff]),
                s(8, said),
                F::V(12, 2),
            ],
        ),
    ])
}

/// The conversation of the owner's report, one step of each kind.
fn conversation() -> Vec<(i64, i64, i64, Vec<u8>)> {
    vec![
        (0, 14, 3, prompt("compare sampleDB with what an enterprise would adopt")),
        (1, 15, 3, reply("SampleDB stands up well.")),
        (
            2,
            132,
            3,
            msg(&[
                F::V(1, 132),
                meta(1_793_567_791, 0, Some("call_1")),
                m(
                    140,
                    &[
                        m(1, &[s(1, "CommandLine"), s(2, "cargo test")]),
                        m(2, &[s(1, "Command exited with code 0.\nok")]),
                    ],
                ),
            ]),
        ),
        (
            3,
            132,
            7,
            msg(&[
                F::V(1, 132),
                meta(1_793_567_792, 0, Some("call_2")),
                m(31, &[s(2, "invalid tool call"), s(3, "invalid tool call")]),
                m(140, &[m(1, &[s(1, "AbsolutePath"), s(2, "/srv/repo/src/main.rs")])]),
            ]),
        ),
        (4, 17, 3, msg(&[F::V(1, 17), m(24, &[m(3, &[s(1, "Individual quota reached."), F::V(7, 429)])])])),
        (5, 23, 3, msg(&[F::V(1, 23), m(30, &[s(5, "<summary>\nearlier steps\n</summary>")])])),
        (
            6,
            101,
            3,
            msg(&[
                F::V(1, 101),
                m(
                    114,
                    &[
                        s(1, "[Message] sender=6125b17d content=..."),
                        m(
                            2,
                            &[
                                s(1, "Message from Backend Researcher (research)"),
                                m(10, &[s(1, "# Findings\nAll good.")]),
                            ],
                        ),
                        s(3, "agent_message"),
                    ],
                ),
            ]),
        ),
        // A kind this reader does not know, and a payload cut short.
        (7, 999, 3, msg(&[F::V(1, 999)])),
        (8, 15, 3, vec![0xa2, 0x01, 0xff]),
    ]
}

/// A database of `steps` at `db`, made with Python's SQLite: the test
/// container has no `sqlite3` program.
fn write_database(db: &Path, steps: &[(i64, i64, i64, Vec<u8>)]) {
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let mut sql = String::from(
        "drop table if exists steps;
         create table steps (idx integer primary key, step_type integer not null default 0, status integer not null default 0, step_payload blob);",
    );
    for (idx, kind, status, payload) in steps {
        sql.push_str(&format!(
            "insert into steps values ({idx}, {kind}, {status}, x'{}');",
            crate::prompt::hex(payload)
        ));
    }
    let made = Command::new("python3")
        .args([
            "-I",
            "-c",
            "import sqlite3, sys; c = sqlite3.connect(sys.argv[1]); c.executescript(sys.argv[2]); c.commit()",
        ])
        .arg(db)
        .arg(sql)
        .status()
        .unwrap();
    assert!(made.success());
}

/// A stand-in for the `sqlite3` program that accepts only the read-only JSON
/// call the gateway makes and logs each query beside itself.
fn reader(dir: &TempDir) -> (Vec<String>, PathBuf) {
    let script = dir.write(
        "sqlite3.py",
        "import json, os, sqlite3, sys\n\
         if sys.argv[1:6] != ['-readonly', '-bail', '-json', '-cmd', '.timeout 500'] or len(sys.argv) != 8:\n    sys.exit(2)\n\
         with open(os.path.join(os.path.dirname(sys.argv[0]), 'queries.log'), 'a') as log:\n    log.write(sys.argv[7] + '\\n')\n\
         con = sqlite3.connect('file:' + sys.argv[6] + '?mode=ro', uri=True)\n\
         cur = con.execute(sys.argv[7])\n\
         names = [d[0] for d in cur.description]\n\
         rows = [dict(zip(names, row)) for row in cur]\n\
         print(json.dumps(rows) if rows else '')\n",
    );
    let log = dir.path().join("queries.log");
    (vec!["python3".into(), "-I".into(), script.to_string_lossy().into_owned()], log)
}

fn expected() -> Vec<Item> {
    vec![
        Item::User {
            client_id: None,
            text: "compare sampleDB with what an enterprise would adopt".into(),
            images: Vec::new(),
            at: Some(1_793_567_781_840),
        },
        Item::Thinking { text: "Checking the tests first.".into() },
        Item::Assistant { text: "SampleDB stands up well.".into(), questions: Vec::new(), at: Some(1_793_567_790_000) },
        Item::Tool {
            call: "call_1".into(),
            name: "run_command".into(),
            summary: "cargo test".into(),
            input: "{\n  \"CommandLine\": \"cargo test\",\n  \"toolSummary\": \"Run tests\"\n}".into(),
            label: None,
            diff: None,
        },
        Item::Tool {
            call: "call_2".into(),
            name: "view_file".into(),
            summary: "/srv/repo/src/main.rs".into(),
            input: "{\n  \"AbsolutePath\": \"/srv/repo/src/main.rs\"\n}".into(),
            label: None,
            diff: None,
        },
        Item::Result { call: "call_1".into(), text: "Command exited with code 0.\nok".into(), error: false },
        Item::Result { call: "call_2".into(), text: "invalid tool call".into(), error: true },
        Item::Notice { text: "Individual quota reached.".into() },
        Item::Notice { text: "Conversation compacted".into() },
        Item::Tool {
            call: "agy-step-6".into(),
            name: "agent".into(),
            summary: "Message from Backend Researcher (research)".into(),
            input: String::new(),
            label: None,
            diff: None,
        },
        Item::Result { call: "agy-step-6".into(), text: "# Findings\nAll good.".into(), error: false },
    ]
}

/// Owner, 2026-11-02, with a screenshot of an idle Antigravity session that
/// showed only agy's banner in a "Terminal" card: "agy looks like a
/// terminal". The conversation it had held 35 steps.
#[test]
fn an_antigravity_session_shows_its_conversation_as_chat() {
    let dir = TempDir::new("agy-chat");
    let (sqlite3, _) = reader(&dir);
    let dirs = Dirs {
        claude: dir.path().join("claude"),
        codex: dir.path().join("codex"),
        opencode_db: dir.path().join("opencode.db"),
        antigravity: dir.path().join("antigravity"),
        start_dir: dir.path().join("repo"),
        sqlite3,
    };
    let mut info = crate::tui::tests::session(57, Tool::Antigravity, Status::Idle, true);
    let mut cache = Cache::default();
    // Before its first prompt names a conversation, and while that
    // conversation has no database yet, the screen is all there is.
    assert_eq!(cache.get(&info, &dirs).map(|l| l.source.clone()), Some(Source::Screen));
    let id = "86888adb-5a7e-4f3e-843b-ae61190b4fb1";
    info.conversation = Some(id.into());
    assert_eq!(cache.get(&info, &dirs).map(|l| l.source.clone()), Some(Source::Screen));
    let screen_generation = cache.peek(57).unwrap().generation;

    let db = crate::antigravity::conversation_db(&dirs.antigravity, id);
    write_database(&db, &conversation());
    let before = std::fs::read(&db).unwrap();
    let log = cache.get(&info, &dirs).expect("the conversation");
    assert_eq!(log.source, Source::Antigravity(db.clone()));
    assert_eq!(log.items, expected());
    assert!(log.generation > screen_generation, "the phone replaces the screen it showed");
    assert_eq!(std::fs::read(&db).unwrap(), before, "read only");
}

#[test]
fn only_new_and_changed_steps_are_read_again() {
    let dir = TempDir::new("agy-steps");
    let (sqlite3, log) = reader(&dir);
    let sqlite3: Vec<&str> = sqlite3.iter().map(String::as_str).collect();
    let db = dir.path().join("conversations/c.db");
    let mut steps = conversation();
    steps.truncate(2);
    // The reply is still being written: no text yet.
    steps[1] = (1, 15, 2, msg(&[F::V(1, 15), meta(1_793_567_790, 0, None), m(20, &[s(3, "Checking")])]));
    write_database(&db, &steps);

    let mut read = Steps::default();
    assert_eq!(read.refresh(&sqlite3, &db), Some(true));
    assert_eq!(read.items()[1], Item::Thinking { text: "Checking".into() });
    assert_eq!(read.refresh(&sqlite3, &db), Some(false), "nothing changed");

    // The reply finishes and a result follows.
    steps[1] = (1, 15, 3, reply("SampleDB stands up well."));
    steps.push(conversation().remove(2));
    write_database(&db, &steps);
    std::fs::remove_file(&log).unwrap();
    assert_eq!(read.refresh(&sqlite3, &db), Some(true));
    assert_eq!(read.items(), [&expected()[..6]].concat());
    let queries = std::fs::read_to_string(&log).unwrap();
    let payload_reads: Vec<&str> = queries.lines().filter(|q| q.contains("hex(step_payload)")).collect();
    assert_eq!(payload_reads.len(), 1, "{queries}");
    assert!(payload_reads[0].ends_with("where idx in (1,2)"), "{queries}");

    // A step taken away (agy rewinds a conversation) goes from the chat.
    steps.truncate(1);
    write_database(&db, &steps);
    assert_eq!(read.refresh(&sqlite3, &db), Some(true));
    assert_eq!(read.items(), [&expected()[..1]].concat());

    assert_eq!(read.refresh(&sqlite3, &dir.path().join("conversations/missing.db")), None);
}
