use super::*;
fn c(day: &str, hour: u32, minute: u32) -> Calendar {
    Calendar { day: day.into(), hour, minute, month: 11, month_day: 1, weekday: 0 }
}
#[test]
fn daily_dst_repeated_minute_has_one_identity_and_gap_catches_up() {
    let s = Schedule { cron: "30 1 * * *".into(), timezone: "local".into() };
    assert_eq!(s.occurrence(&c("2026-11-01", 1, 30)).unwrap(), s.occurrence(&c("2026-11-01", 1, 30)).unwrap());
    assert_ne!(s.occurrence(&c("2026-11-01", 2, 0)).unwrap(), s.occurrence(&c("2026-11-02", 2, 0)).unwrap());
    let spring = Schedule { cron: "30 2 * * *".into(), timezone: "local".into() };
    assert!(spring.occurrence(&c("2026-03-08", 1, 59)).unwrap().is_none());
    assert_eq!(spring.occurrence(&c("2026-03-08", 3, 0)).unwrap(), Some("2026-03-08T02:30".into()));
}
#[test]
fn cron_ranges_steps_and_invalid_bounds() {
    let s = Schedule { cron: "*/5 8-17 * * 1-5".into(), timezone: "UTC".into() };
    let mut day = c("2026-11-02", 10, 15);
    day.weekday = 1;
    assert!(s.occurrence(&day).unwrap().is_some());
    day.minute = 16;
    assert!(s.occurrence(&day).unwrap().is_none());
    for cron in ["60 * * * *", "*/0 * * * *", "* 24 * * *", "* * *", "* * * * 7"] {
        assert!(Schedule { cron: cron.into(), timezone: "UTC".into() }.validate().is_err());
    }
    assert!(Schedule { cron: "0 0 * * *".into(), timezone: "arbitrary".into() }.validate().is_err());
}

#[test]
fn manual_before_daily_time_claims_same_slot_and_cron_minutes_stay_distinct() {
    let daily = Schedule { cron: "0 9 * * *".into(), timezone: "UTC".into() };
    let early = daily.claim_occurrence(&c("2026-11-06", 8, 0)).unwrap();
    assert_eq!(Some(early), daily.occurrence(&c("2026-11-06", 9, 0)).unwrap());
    let cron = Schedule { cron: "*/5 * * * *".into(), timezone: "local".into() };
    assert_ne!(cron.occurrence(&c("2026-11-06", 9, 0)).unwrap(), cron.occurrence(&c("2026-11-06", 9, 5)).unwrap());
    assert_eq!(cron.occurrence(&c("2026-11-01", 1, 30)).unwrap(), cron.occurrence(&c("2026-11-01", 1, 30)).unwrap());
    let restricted = Schedule { cron: "0 9 1 * 1".into(), timezone: "UTC".into() };
    let mut monday = c("2026-11-02", 9, 0);
    monday.month_day = 2;
    monday.weekday = 1;
    assert!(
        restricted.occurrence(&monday).unwrap().is_some(),
        "standard cron joins restricted month-day and weekday with OR"
    );
}
