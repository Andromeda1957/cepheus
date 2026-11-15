use super::*;

/// Owner, 2026-10-27: "set default agent settings to xhigh for both
/// claude/codex". Both programs' own settings already said xhigh; the phone
/// only ever showed "Default", so it names the default from those settings.
#[test]
fn each_program_names_its_default_from_its_own_settings() {
    let dir = crate::testsupport::TempDir::new("models-defaults");
    let homes = Homes::under(dir.path());
    let (claude, codex) = (&homes.claude, &homes.codex);
    std::fs::create_dir_all(claude).unwrap();
    std::fs::create_dir_all(codex).unwrap();
    std::fs::write(claude.join("settings.json"), r#"{"model": "opus", "effortLevel": "xhigh", "permissions": {}}"#)
        .unwrap();
    std::fs::write(
        codex.join("config.toml"),
        "# main settings\nmodel = \"gpt-6.1-sol\"  # the default\nmodel_reasoning_effort = 'xhigh'\n\n\
         [profiles.fast]\nmodel_reasoning_effort = \"low\"\n",
    )
    .unwrap();
    let tools = all(&homes);
    let named = |tool: &str| {
        let t = tools.iter().find(|t| t.tool == tool).unwrap();
        (t.default_model.clone(), t.default_effort.clone())
    };
    assert_eq!(named("claude"), (Some("opus".into()), Some("xhigh".into())));
    assert_eq!(
        named("codex"),
        (Some("gpt-6.1-sol".into()), Some("xhigh".into())),
        "a profile's value is not the default"
    );
    assert_eq!(named("grok"), (None, None));

    // Settings that are missing or could pass as a flag name nothing.
    std::fs::write(claude.join("settings.json"), r#"{"model": "--dangerous", "effortLevel": 3}"#).unwrap();
    std::fs::write(codex.join("config.toml"), "[profiles.fast]\nmodel = \"gpt-6.1-sol\"\n").unwrap();
    let tools = all(&homes);
    for t in &tools {
        assert_eq!((t.default_model.as_deref(), t.default_effort.as_deref()), (None, None), "{}", t.tool);
    }
}

fn efforts(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// Owner, 2026-11-12: launch settings for "codex, agy etc". Each list comes
/// from a third-party format, shaped here as the programs wrote them that
/// day (Grok 1.0.41, Antigravity's `agy models`, opencode 2.2.7).
#[test]
fn grok_lists_its_cached_models_and_the_one_it_starts_on() {
    let dir = crate::testsupport::TempDir::new("models-grok");
    let homes = Homes::under(dir.path());
    std::fs::create_dir_all(&homes.grok).unwrap();
    let info = |name: &str, hidden: bool, levels: &[&str]| {
        serde_json::json!({"info": {"id": name, "name": name.to_uppercase(), "hidden": hidden,
            "reasoning_effort": "high", "supports_reasoning_effort": true,
            "reasoning_efforts": levels.iter().map(|l| serde_json::json!({"id": l, "value": l, "default": *l == "high"})).collect::<Vec<_>>()},
            "api_key": null})
    };
    let cache = serde_json::json!({"grok_version": "1.0.41", "models": {
        "grok-4.7": info("grok-4.7", false, &["xhigh", "high", "medium", "low"]),
        "grok-4.5": info("grok-4.5", false, &["high", "medium", "low"]),
        "grok-secret": info("grok-secret", true, &["high"]),
    }});
    std::fs::write(homes.grok.join("models_cache.json"), cache.to_string()).unwrap();
    let account = serde_json::json!({"settings": {"default_model": "grok-4.7"}}).to_string();
    std::fs::write(homes.grok.join("settings_cache.json"), serde_json::json!({"payload": account}).to_string())
        .unwrap();
    let (models, model, effort) = grok_models(&homes.grok);
    let ids: Vec<(&str, &[String])> = models.iter().map(|m| (m.id.as_str(), m.efforts.as_slice())).collect();
    assert_eq!(
        ids,
        [
            ("grok-4.5", &efforts(&["high", "medium", "low"])[..]),
            ("grok-4.7", &efforts(&["xhigh", "high", "medium", "low"])[..])
        ],
        "a hidden model is not offered"
    );
    assert_eq!((model.as_deref(), effort.as_deref()), (Some("grok-4.7"), Some("high")), "the account's default");
    std::fs::write(homes.grok.join("config.toml"), "[ui]\nyolo = true\n\n[models]\ndefault = \"grok-4.5\"\n").unwrap();
    assert_eq!(grok_models(&homes.grok).1.as_deref(), Some("grok-4.5"), "Grok's own config wins");
}

#[test]
fn antigravity_folds_effort_variants_into_the_model_its_settings_name() {
    let listed = "Fetching available models...\n\
        gemini-4-argon-medium\tGemini 4 Argon (Medium)\n\
        gemini-4-argon-high\tGemini 4 Argon (High)\n\
        gemini-4-argon-low\tGemini 4 Argon (Low)\n\
        gemini-4-hydrogen-xhigh\tGemini 4 Hydrogen (XHigh)\n\
        gpt-oss-120b-medium\tGPT-OSS 120B (Medium)\n\
        plain-model\tPlain Model\n";
    let models = antigravity_models(listed);
    let shown: Vec<(&str, &str, &[String])> =
        models.iter().map(|m| (m.id.as_str(), m.label.as_str(), m.efforts.as_slice())).collect();
    assert_eq!(
        shown,
        [
            ("gemini-4-argon", "Gemini 4 Argon", &efforts(&["low", "medium", "high"])[..]),
            ("gemini-4-hydrogen", "Gemini 4 Hydrogen", &efforts(&["xhigh"])[..]),
            ("gpt-oss-120b", "GPT-OSS 120B", &efforts(&["medium"])[..]),
            ("plain-model", "Plain Model", &[][..]),
        ]
    );
    let dir = crate::testsupport::TempDir::new("models-agy");
    let homes = Homes::under(dir.path());
    std::fs::create_dir_all(&homes.antigravity).unwrap();
    std::fs::write(homes.antigravity.join("settings.json"), r#"{"model": "Gemini 4 Argon (High)"}"#).unwrap();
    assert_eq!(antigravity_defaults(&homes.antigravity, &models), (Some("gemini-4-argon".into()), Some("high".into())));
}

#[test]
fn opencode_lists_enabled_catalog_models_as_provider_and_model() {
    let catalog = serde_json::json!({"location": {}, "data": [
        {"providerID": "google", "id": "gemini-4-hydrogen", "name": "Gemini 4 Hydrogen", "enabled": true,
         "status": "active", "variants": [{"id": "low"}, {"id": "high"}]},
        {"providerID": "cloudflare-ai-gateway", "id": "anthropic/claude-opus-5.5", "name": "Claude Opus 5.5",
         "enabled": true, "status": "active", "variants": []},
        {"providerID": "opencode", "id": "off-model", "name": "Off", "enabled": false, "status": "active"},
        {"providerID": "opencode", "id": "old-model", "name": "Old", "enabled": true, "status": "deprecated"},
    ]});
    let models = opencode_models(&catalog);
    let shown: Vec<(&str, &[String])> = models.iter().map(|m| (m.id.as_str(), m.efforts.as_slice())).collect();
    assert_eq!(
        shown,
        [
            ("google/gemini-4-hydrogen", &efforts(&["low", "high"])[..]),
            ("cloudflare-ai-gateway/anthropic/claude-opus-5.5", &[][..]),
        ]
    );
}
