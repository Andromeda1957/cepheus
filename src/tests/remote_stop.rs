use super::*;

#[test]
fn phone_stop_preserves_live_pty_and_draft_and_reports_the_service_result() {
    let dir = TempDir::new("stop-phone");
    let opts = service_options(&dir, "true");
    let service = thread::spawn(move || crate::service::run(opts));
    let (hub, mut ws) = paired_phone(hub_options(&dir, vec![PHONE]));
    expect(&mut ws, "authoritative service state", |value| value["t"] == "state" && value["ready"] == true);
    let created = control(
        &mut ws,
        1,
        "terminal",
        json!({"command":["sh","-c","echo stand-in ready; read x; echo accepted:$x; read y"]}),
    );
    let id = created["data"]["id"].as_u64().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !screen_shows(&hub, id, "stand-in ready") {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(control(&mut ws, 2, "terminal_input", json!({"id":id,"text":"unsent draft"}))["t"], "control");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !screen_shows(&hub, id, "unsent draft") {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(30));
    }
    let stopped = control(&mut ws, 3, "service", json!({"operation":"stop","confirm":true}));
    assert_eq!(stopped["t"], "err", "Stop must report refusal, not a fire-and-forget success: {stopped}");
    assert!(screen_shows(&hub, id, "unsent draft"));
    assert_eq!(control(&mut ws, 4, "terminal_input", json!({"id":id,"key":"enter"}))["t"], "control");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !screen_shows(&hub, id, "accepted:unsent draft") {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(30));
    }
    send(&mut ws, json!({"t":"close","rid":5,"id":id}));
    assert_eq!(reply(&mut ws, 5)["t"], "ok");
    let deadline = Instant::now() + Duration::from_secs(10);
    while hub.session(id).is_some_and(|s| s.alive) {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(control(&mut ws, 6, "service", json!({"operation":"stop","confirm":true}))["t"], "control");
    service.join().unwrap().unwrap();
}
