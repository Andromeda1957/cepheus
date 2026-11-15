use super::*;

#[test]
fn podman_listing_groups_task_containers() {
    // A project's task tool labels each container with its task under the
    // project's own label (`Conventions::container_task_label`), beside
    // labels of its own.
    let label = "io.cepheus.environment.task";
    let json = br#"[
        {"Names": ["cepheus-task-deck-1"], "State": "running", "Status": "Up 5 minutes", "Labels": {"io.cepheus.environment": "9953c37a", "io.cepheus.environment.repository": "97e302a0", "io.cepheus.environment.task": "cepheus"}},
        {"Names": "cepheus-task-deck-2", "State": "exited", "Status": "Exited (0)", "Labels": {"io.cepheus.environment.task": "cepheus"}},
        {"Names": ["invented"], "State": "running", "Labels": {"io.cepheus.unrelated-task": "cepheus"}},
        {"Names": ["unrelated"], "State": "running", "Labels": {"other": "x"}},
        {"Names": ["nolabels"], "State": "running", "Labels": null}
    ]"#;
    let got = parse_podman_ps(json, label);
    assert_eq!(got.len(), 1);
    let deck = &got["cepheus"];
    assert_eq!(deck.len(), 2);
    assert_eq!(
        deck[0],
        ContainerInfo { name: "cepheus-task-deck-1".into(), state: "running".into(), status: "Up 5 minutes".into() }
    );
    assert_eq!(deck[1].name, "cepheus-task-deck-2");
    assert!(parse_podman_ps(b"not json", label).is_empty());
}

#[test]
fn podman_task_label_defaults_to_the_decks_own_and_empty_looks_up_nothing() {
    let json = br#"[
        {"Names": ["build-1"], "State": "running", "Status": "Up", "Labels": {"io.cepheus.task": "alpha"}},
        {"Names": ["other"], "State": "running", "Status": "Up", "Labels": {"io.cepheus.environment.task": "beta"}}
    ]"#;
    let label = crate::config::Conventions::default().container_task_label;
    assert_eq!(label, "io.cepheus.task");
    let got = parse_podman_ps(json, &label);
    assert_eq!(got.len(), 1);
    assert_eq!(got["alpha"][0].name, "build-1");
    // No label: nothing is parsed, and podman is not run.
    assert!(parse_podman_ps(json, "").is_empty());
    assert!(crate::extensions::conventions::containers("").is_empty());
}

#[test]
fn hyprland_clients_map_pid_to_workspace() {
    let json = br#"[
        {"pid": 100, "workspace": {"id": 3, "name": "3"}},
        {"pid": 200, "workspace": {"id": 10}},
        {"workspace": {"id": 1, "name": "1"}}
    ]"#;
    let got = parse_hypr_clients(json);
    assert_eq!(got.get(&100).map(String::as_str), Some("3"));
    assert_eq!(got.get(&200).map(String::as_str), Some("10"));
    assert_eq!(got.len(), 2);
}

#[test]
fn run_with_timeout_returns_output_and_bounds_hangs() {
    assert_eq!(run_with_timeout("sh", &["-c", "printf ok"], Duration::from_secs(5)), Some(b"ok".to_vec()));
    assert_eq!(run_with_timeout("sh", &["-c", "exit 3"], Duration::from_secs(5)), None);
    assert_eq!(run_with_timeout("cepheus-no-such-program", &[], Duration::from_secs(1)), None);
    let started = Instant::now();
    assert_eq!(run_with_timeout("sh", &["-c", "exec sleep 30"], Duration::from_millis(200)), None);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn desktop_notification_text_cannot_be_parsed_as_options() {
    let args = notify_args("Agent finished", "-Codex #2");
    assert_eq!(args[args.len() - 3..], ["--", "Agent finished", "-Codex #2"], "text is never an option");
}

#[test]
fn desktop_notification_names_the_installed_icon_as_its_app_icon() {
    // Owner, 2026-11-12: "notfications keeps breaking, the icon for cepheus
    // does" not show; Omarchy's toast drew the letter "C" instead. notify-send
    // 0.8 sends `--icon` as the image-path hint and leaves app_icon empty, so
    // the shell took the desktop entry's absolute icon path, rejected it, and
    // lettered the app name; only the old app name had matched the icon ID.
    // `--app-icon` fills app_icon whatever the app is called.
    let args = notify_args("Claude finished", "Read-only API gap audit");
    let options = &args[..args.iter().position(|a| *a == "--").unwrap()];
    assert!(options.contains(&"--app-icon=agent-deck"), "{options:?}"); // legacy-name: installed icon identity
}

#[test]
fn the_same_desktop_notification_within_five_minutes_shows_once() {
    // Owner, 2026-11: "the notifications firing non stop".
    let start = Instant::now();
    let summary = "the_same_desktop_notification_within_five_minutes_shows_once";
    assert!(notify_is_new(summary, "a", start));
    assert!(!notify_is_new(summary, "a", start + Duration::from_secs(60)));
    assert!(notify_is_new(summary, "b", start + Duration::from_secs(60)), "anything different still shows");
    assert!(notify_is_new(summary, "a", start + NOTIFY_REPEAT + Duration::from_secs(1)));
}

#[test]
fn kitty_close_refuses_non_numeric_window_ids() {
    assert!(!kitty_close_window("unix:@nowhere", "1 --match all"));
    assert!(!kitty_close_window("unix:@nowhere", "id:3"));
}

#[test]
fn kitty_screen_is_bound_to_the_live_foreground_process() {
    use crate::testsupport::{kitty_replies, kitty_windows, TempDir};
    let dir = TempDir::new("kitty");
    for (case, first_pid, last_pid, expected) in
        [(0, 42, 42, Some("screen text")), (1, 43, 43, None), (2, 42, 43, None)]
    {
        let path = dir.path().join(format!("k{case}"));
        let mut replies = vec![("ls", kitty_windows(first_pid))];
        if first_pid == 42 {
            replies.push(("get-text", serde_json::json!({"ok":true,"data":"screen text"})));
            replies.push(("ls", kitty_windows(last_pid)));
        }
        let server = kitty_replies(&path, replies);
        assert_eq!(kitty_window_screen(&format!("unix:{}", path.display()), "2", 42).as_deref(), expected);
        server.join().unwrap();
    }
    assert_eq!(kitty_window_screen("tcp:localhost:8766", "2", 42), None);
    assert_eq!(kitty_window_screen("unix:@unused", "2 --match all", 42), None);
}
