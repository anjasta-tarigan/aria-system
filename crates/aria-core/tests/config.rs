use aria_core::config::{ApprovalPolicy, Caps, Config, EmbeddingConfig, SecretBackend, Settings};

fn cfg() -> (tempfile::TempDir, Config) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::new(dir.path());
    (dir, config)
}

#[test]
fn defaults_when_no_config_file() {
    let (_d, c) = cfg();
    let s = c.load().unwrap();
    assert_eq!(s, Settings::default());
    assert_eq!(s.caps.max_turns, 50);
    assert_eq!(s.caps.max_prompt_tokens, 200_000);
    assert_eq!(s.vision_model, None);
}

#[test]
fn approval_policy_defaults_to_always_ask_and_parses() {
    assert_eq!(Settings::default().approval_policy, ApprovalPolicy::AlwaysAsk);
    let s: Settings = serde_json::from_str(
        r#"{"base_url":"https://example.test/v1","model":"m","shell_timeout_secs":120,"approval_policy":"auto_approve_safe"}"#,
    )
    .unwrap();
    assert_eq!(s.approval_policy, ApprovalPolicy::AutoApproveSafe);
}

#[test]
fn embedding_is_off_until_enabled() {
    assert!(!EmbeddingConfig::default().enabled);
    let cfg: EmbeddingConfig = serde_json::from_str(
        r#"{"providerId":"default","model":"m","dimensions":null,"batchSize":64,"timeoutSeconds":30}"#,
    )
    .unwrap();
    assert!(!cfg.enabled);
    let cfg: EmbeddingConfig = serde_json::from_str(
        r#"{"enabled":true,"providerId":"default","model":"m","dimensions":null,"batchSize":64,"timeoutSeconds":30}"#,
    )
    .unwrap();
    assert!(cfg.enabled);
}

#[test]
fn settings_roundtrip() {
    let (_d, c) = cfg();
    let mut s = Settings {
        base_url: "https://example.test/v1".into(),
        model: "some-model".into(),
        vision_model: Some("qwen-vl-plus".into()),
        ..Settings::default()
    };
    s.caps.max_turns = 7;
    c.save(&s).unwrap();
    assert_eq!(c.load().unwrap(), s);
}

#[test]
fn key_is_never_written_to_the_settings_file_or_db() {
    let (dir, c) = cfg();
    let backend = c.set_api_key("sk-super-secret-value").unwrap();
    assert!(matches!(
        backend,
        SecretBackend::Keyring | SecretBackend::FileFallback
    ));
    let settings_text = std::fs::read_to_string(dir.path().join("settings.json"))
        .unwrap_or_default();
    assert!(!settings_text.contains("sk-super-secret-value"));
}

#[test]
fn fallback_key_file_is_0600() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, c) = cfg();
    let backend = c.set_api_key("secret").unwrap();
    if backend == SecretBackend::FileFallback {
        let mode = std::fs::metadata(dir.path().join("api-key"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn empty_key_file_means_absent() {
    // An empty fallback file must read as "no key". Assert on the fallback reader
    // only: the OS secret service is shared machine state.
    let (dir, _c) = cfg();
    std::fs::write(dir.path().join("api-key"), "  \n").unwrap();
    let reported = { let c = Config::new(dir.path()); c.secret_status().unwrap() };
    if reported.backend == SecretBackend::FileFallback {
        assert!(!reported.present);
    }
    let key = { let c = Config::new(dir.path()); c.api_key().unwrap() };
    if let Some((k, SecretBackend::FileFallback)) = key {
        assert!(!k.trim().is_empty());
    }
}

#[test]
fn soft_and_hard_caps() {
    let caps = Caps {
        max_turns: 10,
        max_prompt_tokens: 1000,
    };
    assert!(!caps.soft_limit_reached(6, 500));
    assert!(caps.soft_limit_reached(7, 0));
    assert!(caps.soft_limit_reached(0, 700));
    assert!(!caps.hard_limit_reached(9, 999));
    assert!(caps.hard_limit_reached(10, 0));
    assert!(caps.hard_limit_reached(0, 1000));
}

#[test]
fn api_key_resolves_default_provider_key_and_falls_back() {
    let (_dir, c) = cfg();
    c.set_provider_api_key("prov-1", "sk-provider-key").unwrap();

    let mut s = Settings::default();
    s.default_ai_provider_id = Some("prov-1".into());
    c.save(&s).unwrap();

    let (key, _) = c.api_key().unwrap().expect("provider key resolved");
    assert_eq!(key, "sk-provider-key");

    // Falls back to legacy global key when default provider has no key
    s.default_ai_provider_id = Some("nonexistent".into());
    c.save(&s).unwrap();
    c.set_api_key("sk-legacy-global").unwrap();
    let (key, _) = c.api_key().unwrap().expect("legacy key fallback");
    assert_eq!(key, "sk-legacy-global");
}
