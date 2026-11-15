//! When each Dot message was sent.
use super::*;

/// Seconds since the epoch of a local wall-clock time.
fn local(year: i32, month: i32, day: i32, hour: i32, minute: i32) -> u64 {
    // SAFETY: mktime reads and normalises a tm this function owns.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = year - 1900;
    tm.tm_mon = month - 1;
    tm.tm_mday = day;
    tm.tm_hour = hour;
    tm.tm_min = minute;
    tm.tm_isdst = -1;
    (unsafe { libc::mktime(&mut tm) }) as u64
}

// Owner, 2026-11-12: "also give the dot time stamps for each message"; the
// phone writes the time alone today, else the short date before it.
#[test]
fn a_message_time_is_the_clock_today_and_gains_its_date_otherwise() {
    let midnight = local(2025, 3, 4, 0, 5) * 1000;
    let afternoon = local(2025, 3, 4, 13, 30) * 1000;
    assert_eq!(sent_label(midnight, "2025-03-04").as_deref(), Some("12:05 AM"));
    assert_eq!(sent_label(afternoon, "2025-03-04").as_deref(), Some("1:30 PM"));
    assert_eq!(sent_label(afternoon, "2025-07-01").as_deref(), Some("Mar 4, 1:30 PM"));
    assert_eq!(sent_label(midnight, "2026-01-02").as_deref(), Some("Mar 4, 2025, 12:05 AM"));
}
