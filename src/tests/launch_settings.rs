use super::*;
use crate::testsupport::TempDir;

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn model(id: &str, label: &str, efforts: &[&str]) -> Model {
    Model { id: id.into(), label: label.into(), efforts: strings(efforts) }
}

/// Claude's efforts are the program's; each Codex model has its own, and
/// the smaller one lacks the high ones.
fn tools() -> Vec<ToolModels> {
    let plain = |tool| ToolModels {
        tool,
        label: None,
        cloud: false,
        models: Vec::new(),
        efforts: Vec::new(),
        switch: false,
        default_model: None,
        default_effort: None,
    };
    vec![
        ToolModels {
            models: vec![model("opus", "Opus", &[]), model("sonnet", "Sonnet", &[])],
            efforts: strings(&["low", "medium", "high", "xhigh", "max"]),
            ..plain("claude")
        },
        ToolModels {
            models: vec![
                model("gpt-6.1-sol", "GPT-6.1 Sol", &["low", "medium", "high", "xhigh"]),
                model("gpt-6.1-mini", "GPT-6.1 Mini", &["low", "medium"]),
            ],
            default_model: Some("gpt-6.1-sol".into()),
            default_effort: Some("medium".into()),
            ..plain("codex")
        },
        plain("grok"),
        // opencode's efforts are its models' variants.
        ToolModels {
            models: vec![model("google/gemini-4-hydrogen", "Gemini 4 Hydrogen", &["low", "high"])],
            ..plain("opencode")
        },
    ]
}

fn launch(model: Option<&str>, effort: Option<&str>) -> Launch {
    Launch { model: model.map(Into::into), effort: effort.map(Into::into) }
}

fn saved(entries: &[(&str, Option<&str>, Option<&str>)]) -> Settings {
    entries.iter().map(|(tool, model, effort)| (tool.to_string(), launch(*model, *effort))).collect()
}

fn resolved(tool: Tool, model: Option<&str>, effort: Option<&str>, settings: &Settings) -> Launch {
    let start = StartModel { model: model.map(Into::into), effort: effort.map(Into::into) };
    let StartModel { model, effort } = resolve(tool, &start, settings, tools);
    Launch { model, effort }
}

fn run(line: &str, settings: &mut Settings) -> Result<String, String> {
    set(&strings(&line.split_whitespace().collect::<Vec<_>>()), &tools(), settings)
}

#[test]
fn a_start_takes_the_saved_model_and_effort_only_where_it_names_none() {
    let settings = saved(&[("claude", Some("opus"), Some("xhigh")), ("codex", None, Some("xhigh"))]);
    assert_eq!(resolved(Tool::Claude, None, None, &settings), launch(Some("opus"), Some("xhigh")));
    assert_eq!(resolved(Tool::Claude, Some("sonnet"), None, &settings), launch(Some("sonnet"), Some("xhigh")));
    assert_eq!(resolved(Tool::Claude, None, Some("low"), &settings), launch(Some("opus"), Some("low")));
    assert_eq!(resolved(Tool::Grok, None, None, &settings), launch(None, None), "nothing is saved for Grok");
    // Codex's own default model offers xhigh; the smaller one would refuse
    // it, so that start is left at the program's own effort.
    assert_eq!(resolved(Tool::Codex, None, None, &settings), launch(None, Some("xhigh")));
    assert_eq!(resolved(Tool::Codex, Some("gpt-6.1-mini"), None, &settings), launch(Some("gpt-6.1-mini"), None));
    // opencode's New refuses a variant without the model that has it.
    let settings = saved(&[("opencode", None, Some("high"))]);
    assert_eq!(resolved(Tool::OpenCode, None, None, &settings), launch(None, None));
    let hydrogen = Some("google/gemini-4-hydrogen");
    assert_eq!(resolved(Tool::OpenCode, hydrogen, None, &settings), launch(hydrogen, Some("high")));
}

#[test]
fn model_and_effort_commands_save_what_each_program_starts_with() {
    let mut settings = Settings::new();
    run("OPUS", &mut settings).unwrap();
    run("xhigh", &mut settings).unwrap();
    assert_eq!(settings, saved(&[("claude", Some("opus"), Some("xhigh")), ("codex", None, Some("xhigh"))]));
    run("max", &mut settings).unwrap();
    run("low codex", &mut settings).unwrap();
    assert_eq!(settings, saved(&[("claude", Some("opus"), Some("max")), ("codex", None, Some("low"))]));
    run("sonnet high", &mut settings).unwrap();
    run("gpt-6.1-mini medium", &mut settings).unwrap();
    // Codex now starts on a model without xhigh, so the bare effort skips it.
    run("xhigh", &mut settings).unwrap();
    assert_eq!(
        settings,
        saved(&[("claude", Some("sonnet"), Some("xhigh")), ("codex", Some("gpt-6.1-mini"), Some("medium"))])
    );
}

#[test]
fn a_command_that_does_not_fit_saves_nothing() {
    let mut settings = saved(&[("codex", None, Some("xhigh"))]);
    let before = settings.clone();
    for line in [
        "gpt-6.1-mini",
        "gpt-6.1-mini high",
        "max codex",
        "opus codex",
        "grok-5",
        "high grok",
        "opus low high",
        "high opencode",
        "google/gemini-4-hydrogen xhigh",
    ] {
        assert!(run(line, &mut settings).is_err(), ":{line} must be refused");
    }
    assert_eq!(settings, before);
    run("google/gemini-4-hydrogen high", &mut settings).unwrap();
    run("low opencode", &mut settings).unwrap();
    assert_eq!(settings["opencode"], launch(Some("google/gemini-4-hydrogen"), Some("low")));
}

#[test]
fn a_model_two_programs_list_needs_its_program_named() {
    let mut tools = tools();
    tools[1].models.push(model("opus", "Opus", &["low"]));
    let mut settings = Settings::new();
    assert!(set(&strings(&["opus"]), &tools, &mut settings).is_err());
    assert!(settings.is_empty());
    set(&strings(&["opus", "codex"]), &tools, &mut settings).unwrap();
    assert_eq!(settings, saved(&[("codex", Some("opus"), None)]));
}

#[test]
fn reset_forgets_one_choice_one_program_or_everything() {
    let mut settings = saved(&[("claude", Some("opus"), Some("xhigh")), ("codex", None, Some("high"))]);
    reset(&strings(&["claude", "effort"]), &tools(), &mut settings).unwrap();
    assert_eq!(settings, saved(&[("claude", Some("opus"), None), ("codex", None, Some("high"))]));
    reset(&strings(&["codex"]), &tools(), &mut settings).unwrap();
    assert_eq!(settings, saved(&[("claude", Some("opus"), None)]));
    reset(&[], &tools(), &mut settings).unwrap();
    assert!(settings.is_empty());
}

#[test]
fn a_settings_file_that_does_not_hold_valid_settings_is_an_error() {
    let dir = TempDir::new("launch-settings");
    let file = path(dir.path());
    assert!(load(&file).unwrap().is_empty());
    let settings = saved(&[("claude", Some("opus"), Some("xhigh"))]);
    save(&file, &settings).unwrap();
    assert_eq!(load(&file).unwrap(), settings);
    for broken in [&b"{\"claude\": {\"model\": \"--resume\"}}"[..], b"{\"claude\": {\"modle\": \"opus\"}}", b"not json"]
    {
        fs::write(&file, broken).unwrap();
        assert!(load(&file).is_err(), "{}", String::from_utf8_lossy(broken));
    }
    save(&file, &Settings::new()).unwrap();
    assert!(!file.exists());
}
