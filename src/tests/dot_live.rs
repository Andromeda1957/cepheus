use super::*;

#[test]
fn live_activity_requires_the_current_room_subscription_and_increasing_attached_state() {
    let config = Config {
        token: "fixture".into(),
        account_id: "account".into(),
        room_id: "room".into(),
        app_version: "fixture".into(),
    };
    let announced = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&announced);
    let shared = Arc::new(Shared {
        config: Mutex::new(config.clone()),
        status: Mutex::new(Status::unknown()),
        generation: AtomicU64::new(0),
        stopped: AtomicBool::new(false),
        wake: Condvar::new(),
        notify: Some(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })),
    });
    let announced = move || announced.load(Ordering::SeqCst);
    let settings = Settings::production("https://chatgpt.com/backend-api");
    let mut cycle = Cycle::new(&shared, &settings, &config, Arc::new(|| true));
    cycle.ready_id = Some("subscription".into());
    let state = json!({"type":"calpico-room-live-state","version":1,"room_id":"room","subscription_id":"subscription","revision":1,
        "threads":[{"stream_state":"attached","turn_status":"in_progress","is_active":true,"turn_id":"turn","activity_status":{"status_type":"working"}}]});
    cycle.apply(&state);
    assert!(lock(&shared.status).working);
    assert_eq!(announced(), 1);
    for (key, value) in
        [("room_id", json!("elsewhere")), ("subscription_id", json!("other")), ("revision", json!(i64::MIN))]
    {
        let mut rejected = state.clone();
        rejected[key] = value;
        rejected["threads"][0]["turn_status"] = "completed".into();
        rejected["threads"][0]["is_active"] = false.into();
        cycle.apply(&rejected);
        assert!(lock(&shared.status).working);
    }
    let mut stale = state.clone();
    stale["threads"][0]["activity_status"]["status_type"] = "ready".into();
    cycle.apply(&stale);
    assert!(lock(&shared.status).working);
    assert_eq!(announced(), 1, "rejected and stale states are not announced");
    let mut detached = state.clone();
    detached["revision"] = 2.into();
    detached["threads"][0]["stream_state"] = "detached".into();
    cycle.apply(&detached);
    assert!(!lock(&shared.status).known);
    assert_eq!(announced(), 2);
    let mut finished = state;
    finished["revision"] = 3.into();
    finished["threads"][0]["turn_status"] = "completed".into();
    finished["threads"][0]["is_active"] = false.into();
    finished["threads"][0]["activity_status"]["status_type"] = "ready".into();
    cycle.apply(&finished);
    let status = lock(&shared.status).clone();
    assert!(status.known);
    assert!(!status.working);
    assert_eq!(status.revision, 3);
    assert_eq!(announced(), 3);
    // A new revision of the same state is kept but not announced, and a
    // stopped consumer announces nothing.
    finished["revision"] = 4.into();
    cycle.apply(&finished);
    assert_eq!(lock(&shared.status).revision, 4);
    assert_eq!(announced(), 3);
    shared.stopped.store(true, Ordering::SeqCst);
    publish(&shared, Status::unknown());
    assert_eq!(announced(), 3);
}
