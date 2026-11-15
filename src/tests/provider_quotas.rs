use super::*;

#[test]
fn gemini_usage_keeps_only_gemini_and_argon_pools() {
    // The shape `agy --print /usage --output-format json` replied with on
    // 2026-11-08, with a percentage changed and a bucket without one added.
    let value = json!({"status":"SUCCESS", "command":{"name":"usage", "data":{"groups":[
        {"name":"Gemini Models", "buckets":[
            {"id":"gemini-weekly", "window":"weekly", "remaining_fraction":0.0, "reset_time":"2033-05-18T03:33:20Z"},
            {"id":"gemini-5h", "window":"5h", "remaining_fraction":0.75, "reset_time":"2033-05-18T03:33:20Z"}
        ]},
        {"name":"Claude and GPT models", "buckets":[
            {"id":"3p-weekly", "window":"weekly", "remaining_fraction":0.5, "reset_time":"2033-05-18T03:33:20Z"},
            {"id":"3p-5h", "window":"5h", "remaining_fraction":0.5, "reset_time":"2033-05-18T03:33:20Z"}
        ]},
        {"name":"Gemini 4 Argon", "buckets":[
            {"id":"gemini-argon-wtu-weekly", "window":"weekly", "remaining_fraction":1, "reset_time":"2033-05-18T03:33:20Z"},
            {"id":"gemini-argon-5h", "window":"5h", "reset_time":"2033-05-18T03:33:20Z"}
        ]}
    ]}}});
    let windows = gemini_windows(&value, 100);
    assert_eq!(
        windows.iter().map(|w| (w.label.as_str(), w.used_percent, w.resets_at)).collect::<Vec<_>>(),
        vec![
            ("wk", Some(100.0), Some(2_000_000_000)),
            ("5h", Some(25.0), Some(2_000_000_000)),
            ("Argon wk", Some(0.0), Some(2_000_000_000)),
        ]
    );
    assert!(gemini_windows(&value, 2_000_000_000).is_empty(), "expired readings must not look current");
}

#[test]
fn claude_usage_keeps_only_the_per_model_rows() {
    // The rows Claude Code 2.1.322's get_usage replied with on 2026-11-12,
    // dates moved on, plus a surface-scoped row and a model row with no share.
    let value = json!({"subscription_type":"max", "rate_limits":{"five_hour":null, "limits":[
        {"kind":"weekly_all", "group":"weekly", "percent":6, "resets_at":"2033-05-18T03:33:20.130456+00:00",
            "severity":"normal", "is_active":true, "scope":null},
        {"kind":"weekly_scoped", "group":"weekly", "percent":12, "resets_at":"2033-05-18T03:33:20+00:00",
            "severity":"normal", "is_active":false, "scope":{"model":{"display_name":"Fable", "id":null}, "surface":null}},
        {"kind":"weekly_scoped", "group":"weekly", "percent":40, "resets_at":"2033-05-18T03:33:20Z",
            "scope":{"model":null, "surface":{"display_name":"Design"}}},
        {"kind":"session_scoped", "group":"session", "resets_at":"2033-05-18T03:33:20Z",
            "scope":{"model":{"display_name":"Fable"}}}
    ]}});
    assert_eq!(
        claude_model_windows(&value, 100),
        vec![UsageWindow {
            label: "Fable wk".into(),
            used_percent: Some(12.0),
            resets_at: Some(2_000_000_000),
            updated_at: 100
        }]
    );
    assert!(claude_model_windows(&value, 2_000_000_000).is_empty(), "expired readings must not look current");
}

#[test]
fn grok_shows_a_reading_only_with_its_reported_share() {
    // The reply Grok 1.0.77 gave on 2026-11-08 for a week without Grok turns.
    let mut value = json!({"config":{
        "currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY", "start":"2033-05-11T03:33:20.095482+00:00",
            "end":"2033-05-18T03:33:20.095482+00:00"},
        "isUnifiedBillingUser":true, "onDemandUsed":{"val":0}, "onDemandCap":{"val":0}, "prepaidBalance":{"val":0},
        "billingPeriodEnd":"2033-05-18T03:33:20.095482+00:00"
    }, "subscription_tier":"X Premium"});
    assert!(grok_windows(&value, 100).is_empty(), "a reset alone is not a reading");
    value["config"]["creditUsagePercent"] = json!(40.0);
    assert_eq!(
        grok_windows(&value, 100),
        vec![UsageWindow {
            label: "wk".into(),
            used_percent: Some(40.0),
            resets_at: Some(2_000_000_000),
            updated_at: 100
        }]
    );
    assert!(grok_windows(&value, 2_000_000_000).is_empty());
}

#[test]
fn malformed_provider_metadata_cannot_become_a_full_quota() {
    for reset in ["2033-05-18T03:33:20.ééZ", "999999999999999999-05-18T03:33:20Z", "2033-99-99T03:33:20Z"] {
        assert!(grok_windows(&json!({"config":{"currentPeriod":{"end":reset}}}), 100).is_empty());
    }
    assert!(grok_windows(&json!({"config":{"creditUsagePercent":-5}}), 100).is_empty());
    assert!(grok_windows(&json!({"config":{"creditUsagePercent":101}}), 100).is_empty());
    let mut value = json!({"status":"SUCCESS", "command":{"name":"usage", "data":{"groups":[
        {"name":"Gemini", "buckets":[{"id":"gemini-5h", "window":"5h", "remaining_fraction":1.5}]},
        {"name":"Gemini", "buckets":[{"id":"gemini-bad\nid", "window":"5h", "remaining_fraction":1.0}]},
        {"name":"Gemini", "buckets":[{"id":"gemini-weekly", "window":"bad\nwindow", "remaining_fraction":1.0}]},
        {"name":"Gemini", "buckets":[{"window":"5h", "remaining_fraction":1.0}]}
    ]}}});
    assert!(gemini_windows(&value, 100).is_empty());
    value["command"]["name"] = json!("model");
    assert!(gemini_windows(&value, 100).is_empty());
}

#[test]
fn polling_rejects_interactive_launch_arguments_before_execution() {
    let command =
        ["sh", "-c", "printf '%s' '{\"status\":\"SUCCESS\"}'", "--disable-slash-commands"].map(str::to_string);
    assert_eq!(gemini(&command, Path::new("/")), None);

    // The owner's agy launches skipping permissions in Default mode
    // (2026-11-12); its allowance is still read, without those arguments.
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::testsupport::TempDir::new("provider-quotas");
    let script = dir.write("agy", "#!/bin/sh\n[ \"$1\" = --print ] && printf '%s' '{\"status\":\"SUCCESS\"}'\n");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let program = script.to_string_lossy().into_owned();
    let launch = |args: &[&str]| [&[program.as_str()], args].concat().into_iter().map(String::from).collect::<Vec<_>>();
    let owner = launch(&["--dangerously-skip-permissions", "--settings", r#"{"agentMode":""}"#]);
    assert_eq!(gemini(&owner, dir.path()), Some(json!({"status":"SUCCESS"})));
    let other_settings = launch(&["--settings", r#"{"agentMode":"","disableSlashCommands":true}"#]);
    assert_eq!(gemini(&other_settings, dir.path()), None, "other settings can change what the print does");
}

#[test]
fn the_stdio_readers_take_only_their_own_reply() {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::testsupport::TempDir::new("provider-quotas-stdio");
    let cli = |name: &str, body: &str| {
        let script = dir.write(name, format!("#!/bin/sh\n{body}"));
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        vec![script.to_string_lossy().into_owned()]
    };
    // Claude answers the usage request after other lines, the reply to
    // another request among them; then it exits when its input closes, or not.
    let claude_cli = |name: &str, answer: &str, after: &str| {
        cli(
            name,
            &format!(
                r#"[ "$*" = "--print --input-format stream-json --output-format stream-json --verbose --setting-sources  --strict-mcp-config --no-session-persistence" ] || exit 3
read -r request
case "$request" in *'"get_usage"'*) ;; *) exit 4 ;; esac
case "$request" in *'"skip_behaviors":true'*) ;; *) exit 4 ;; esac
echo '{{"type":"system","subtype":"init"}}'
echo '{{"type":"control_response","response":{{"request_id":"other","subtype":"success","response":{{"other":true}}}}}}'
echo '{answer}'
{after}
"#
            ),
        )
    };
    let usage = r#"{"type":"control_response","response":{"request_id":"cepheus-usage","subtype":"success","response":{"rate_limits":{"limits":[]}}}}"#;
    let answered = Some(json!({"rate_limits": {"limits": []}}));
    assert_eq!(claude(&claude_cli("claude", usage, "while read -r _; do :; done"), dir.path()), answered);
    let failed =
        r#"{"type":"control_response","response":{"request_id":"cepheus-usage","subtype":"error","error":"x"}}"#;
    assert_eq!(claude(&claude_cli("claude-error", failed, ""), dir.path()), None);
    // A CLI still running after its answer is stopped, not waited for.
    let start = Instant::now();
    assert_eq!(claude(&claude_cli("claude-lingering", usage, "exec sleep 300"), dir.path()), answered);
    let bound = crate::testsupport::scaled_timeout(Duration::from_secs(10));
    assert!(start.elapsed() < bound, "{:?} waited for a lingering CLI", start.elapsed());

    let grok_cli = cli(
        "grok",
        r#"[ "$*" = "agent --no-leader stdio" ] || exit 3
read -r initialize
case "$initialize" in *'"initialize"'*) ;; *) exit 4 ;; esac
echo '{"jsonrpc":"2.0","method":"notice"}'
echo '{"jsonrpc":"2.0","id":1,"result":{}}'
read -r billing
case "$billing" in *'"_x.ai/billing"'*) ;; *) exit 5 ;; esac
echo '{"jsonrpc":"2.0","id":2,"result":{"creditUsagePercent":12}}'
while read -r _; do :; done
"#,
    );
    assert_eq!(grok(&grok_cli, dir.path()), Some(json!({"creditUsagePercent": 12})));
}

#[test]
fn allowance_reads_follow_ended_turns_at_most_once_per_gap() {
    let gap = Duration::from_secs(300);
    let start = Instant::now();
    let mut schedule = Schedule::new(gap);
    assert_eq!(schedule.next(start), Err(None), "nothing is read before a turn ends");
    schedule.turn_ended(Tool::Antigravity);
    schedule.turn_ended(Tool::Antigravity);
    assert_eq!(schedule.next(start), Ok(Tool::Antigravity));
    assert_eq!(schedule.next(start), Err(None), "turns that ended together share one read");
    // A turn that ends during the read or the gap waits for one later read;
    // each provider has its own gap.
    schedule.turn_ended(Tool::Antigravity);
    schedule.turn_ended(Tool::Grok);
    assert_eq!(schedule.next(start + Duration::from_secs(1)), Ok(Tool::Grok));
    schedule.turn_ended(Tool::Antigravity);
    assert_eq!(schedule.next(start + Duration::from_secs(60)), Err(Some(Duration::from_secs(240))));
    assert_eq!(schedule.next(start + gap), Ok(Tool::Antigravity));
    assert_eq!(schedule.next(start + gap), Err(None));
}

#[test]
fn carried_readings_show_until_their_reset() {
    let window = |label: &str, used: Option<f64>, reset: u64| UsageWindow {
        label: label.into(),
        used_percent: used,
        resets_at: Some(reset),
        updated_at: 1,
    };
    let carried = Usage {
        grok: vec![window("wk", Some(40.0), 1000), window("mo", None, 1000)],
        gemini: vec![
            window("5h", Some(25.0), 500),
            window("Argon wk", Some(10.0), 1000),
            window("Gemini Models weekly", Some(5.0), 1000),
        ],
        claude_models: vec![window("Fable wk", Some(30.0), 500)],
        ..Usage::default()
    };
    let refresh = Refresh::start(Vec::new(), PathBuf::from("/"), &carried);
    refresh.turn_ended(Tool::Grok);
    assert!(refresh.lock().schedule.due.is_empty(), "a provider without a command is never read");
    let mut usage = Usage::default();
    refresh.fill(&mut usage, 100);
    assert_eq!(usage.grok, vec![window("wk", Some(40.0), 1000)]);
    assert_eq!(usage.gemini.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(), ["5h", "Argon wk"]);
    assert_eq!(usage.claude_models, vec![window("Fable wk", Some(30.0), 500)]);
    refresh.fill(&mut usage, 500);
    assert_eq!(usage.gemini.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(), ["Argon wk"]);
    assert!(usage.claude_models.is_empty());
}

#[test]
fn the_reader_starts_the_cli_only_after_a_turn_ends() {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::testsupport::TempDir::new("provider-quotas");
    let count = dir.path().join("reads");
    let script = dir.write(
        "agy",
        format!(
            "#!/bin/sh\necho read >> '{}'\nprintf '%s' '{}'\n",
            count.display(),
            json!({"status":"SUCCESS", "command":{"name":"usage", "data":{"groups":[{"name":"Gemini Models",
                "buckets":[{"id":"gemini-5h", "window":"5h", "remaining_fraction":0.5, "reset_time":"2033-05-18T03:33:20Z"}]}]}}})
        ),
    );
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let command = vec![script.to_string_lossy().into_owned()];
    let refresh = Refresh::start(vec![(Tool::Antigravity, command)], dir.path().into(), &Usage::default());
    std::thread::sleep(Duration::from_millis(200));
    assert!(!count.exists(), "no read before a turn ends");
    refresh.turn_ended(Tool::Antigravity);
    let deadline = Instant::now() + Duration::from_secs(20);
    let gemini = || {
        let mut usage = Usage::default();
        refresh.fill(&mut usage, 100);
        usage.gemini
    };
    while gemini().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(gemini().iter().map(|w| (w.label.as_str(), w.used_percent)).collect::<Vec<_>>(), [("5h", Some(50.0))]);
    refresh.turn_ended(Tool::Antigravity);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 1, "the next read waits for the gap");
}
