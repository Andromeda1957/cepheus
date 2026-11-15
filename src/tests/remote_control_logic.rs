use super::*;
use crate::testsupport::TempDir;
use std::os::unix::{fs::symlink, net::UnixListener};

#[test]
fn saved_reports_are_contained_bounded_text() {
    let dir = TempDir::new("phone-report-boundary");
    let scans = dir.path().join("scans");
    let report = scans.join("20261103-120000-abcd/report/report.md");
    fs::create_dir_all(report.parent().unwrap()).unwrap();
    fs::write(&report, "# Saved report\nNo findings.").unwrap();
    assert!(read_report(&scans, "20261103-120000-abcd").unwrap().contains("No findings"));
    for id in ["../outside", "/etc/passwd", ".", "scan/report", ""] {
        assert!(read_report(&scans, id).is_err(), "{id}");
    }
    fs::write(dir.path().join("private.txt"), "private content").unwrap();
    fs::remove_file(&report).unwrap();
    symlink(dir.path().join("private.txt"), &report).unwrap();
    assert!(read_report(&scans, "20261103-120000-abcd").is_err(), "a report symlink must not escape the scan root");
    fs::remove_file(&report).unwrap();
    fs::write(&report, vec![b'x'; REPORT_LIMIT as usize + 1]).unwrap();
    assert!(read_report(&scans, "20261103-120000-abcd").is_err());
    fs::write(&report, [0xff, 0xfe]).unwrap();
    assert!(read_report(&scans, "20261103-120000-abcd").is_err());
}

#[test]
fn a_late_created_reply_cannot_answer_the_next_control() {
    let dir = TempDir::new("phone-late-control");
    let path = dir.path().join("s.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (first, _) = listener.accept().unwrap();
        let mut first = BufReader::new(first);
        assert!(matches!(read_msg::<_, ClientMsg>(&mut first).unwrap(), Some(ClientMsg::Hello { .. })));
        read_msg::<_, ClientMsg>(&mut first).unwrap();
        thread::sleep(Duration::from_millis(100));
        let _ = write_msg(first.get_mut(), &ServerMsg::Created { id: 11, request: None });
        let (second, _) = listener.accept().unwrap();
        let mut second = BufReader::new(second);
        read_msg::<_, ClientMsg>(&mut second).unwrap();
        read_msg::<_, ClientMsg>(&mut second).unwrap();
        write_msg(second.get_mut(), &ServerMsg::Created { id: 22, request: None }).unwrap();
    });
    let msg = ClientMsg::SecurityScan { host: None, scope: None };
    let accept = |m: &ServerMsg| matches!(m, ServerMsg::Created { .. });
    assert!(exchange(&path, &msg, Duration::from_millis(30), accept).is_err());
    let second = exchange(&path, &msg, Duration::from_secs(2), accept).unwrap();
    assert!(matches!(second, ServerMsg::Created { id: 22, request: None }), "{second:?}");
    server.join().unwrap();
}

#[test]
fn control_validation_rejects_unknown_fields_and_malformed_values() {
    for value in [
        json!({"action":"nope"}),
        json!({"action":"overview","extra":true}),
        json!({"action":"terminal_resize","id":1,"cols":65536,"rows":20}),
        json!({"action":"install","source":"gh:owner/repo","agents":"claude"}),
    ] {
        assert!(serde_json::from_value::<Action>(value.clone()).is_err(), "{value}");
    }
}
