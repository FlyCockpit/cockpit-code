use code_config::{ConfigStore, Modality, ModelConfig, PermissionMode, Provider};
use code_engine::model::Model;

/// Problems that stop a session from starting (`errors`) or that the user
/// should know about (`warnings`).
#[derive(Debug, Default, PartialEq)]
pub struct Findings {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// What a model reference is used for; each role is checked like the engine does.
#[derive(Clone, Copy)]
pub enum Role {
    /// The session model, built with `Model::new` (text output, limits, URL, thinking).
    Chat,
    /// The safety reviewer: session startup requires it to be configured with
    /// text input and output (`permissions::validate_model`); `active` when the
    /// permission mode reviews actions with it (auto, unattended).
    Safety { active: bool },
}

/// Why a model reference can't be used.
#[derive(Debug, PartialEq)]
pub enum Problem {
    /// No session can start with it (or, for an active safety model, review).
    Broken(String),
    /// Worth knowing, but a session can start: a chat model the provider has
    /// not discovered yet, or a disabled provider for an unused safety model.
    Pending(String),
}
impl Problem {
    pub fn text(&self) -> &str {
        match self {
            Self::Broken(text) | Self::Pending(text) => text,
        }
    }
}

/// Why `id` (`provider/model`) can't be used in `role`, if it can't.
pub fn unresolved(store: &ConfigStore, id: &str, role: Role) -> Option<Problem> {
    let Some((provider_id, model)) = id.split_once('/') else {
        return Some(Problem::Broken("not in provider/model form".into()));
    };
    let file = format!("providers/{provider_id}.json");
    let provider = match store.provider(provider_id) {
        None if store.errors().iter().any(|e| e.contains(&file)) => {
            return Some(Problem::Broken(format!(
                "provider {provider_id} was skipped (its {file} did not load)"
            )));
        }
        None => {
            return Some(Problem::Broken(format!(
                "provider {provider_id} is not configured (no {file})"
            )));
        }
        Some(provider) => provider,
    };
    if !provider.enabled {
        let why = format!("provider {provider_id} is disabled");
        // Startup does not check enablement for the safety model; only its use does.
        return Some(match role {
            Role::Safety { active: false }
                if store
                    .model(id)
                    .is_some_and(|(_, config)| safety_capable(config)) =>
            {
                Problem::Pending(why)
            }
            _ => Problem::Broken(why),
        });
    }
    let Some((_, config)) = store.model(id) else {
        // Sessions refresh discovery after they validate the safety model.
        return Some(
            if provider.discovery().is_some() && matches!(role, Role::Chat) {
                Problem::Pending(format!(
                    "{model} is not in provider {provider_id}'s discovery cache; run `cockpit-code discover {provider_id}`"
                ))
            } else {
                Problem::Broken(format!("provider {provider_id} has no model {model}"))
            },
        );
    };
    match role {
        Role::Chat => chat_error(provider_id, model, provider, config).map(Problem::Broken),
        Role::Safety { .. } => (!safety_capable(config))
            .then(|| Problem::Broken("a safety model needs text input and text output".into())),
    }
}

/// What `permissions::validate_model` requires of the safety model at session startup.
fn safety_capable(config: &ModelConfig) -> bool {
    let caps = &config.capabilities;
    caps.input.contains(&Modality::Text) && caps.output.contains(&Modality::Text)
}

/// The engine's own reason a session could not use this model, if any.
fn chat_error(
    provider_id: &str,
    model: &str,
    provider: &Provider,
    config: &ModelConfig,
) -> Option<String> {
    Model::new(
        provider_id.into(),
        model.into(),
        provider.clone(),
        config.clone(),
        None,
    )
    .err()
    .map(|e| e.to_string())
}

/// Checks that a session could start: some enabled provider has a model the
/// engine accepts for chat (or discovers its models), and the configured
/// default and safety models resolve. Session startup rejects a missing or
/// text-incapable safety model in every mode.
pub fn check(store: &ConfigStore) -> Findings {
    let mut findings = Findings::default();
    let has_chat_model = store
        .models()
        .any(|(provider_id, provider, model_id, model)| {
            provider.enabled && chat_error(provider_id, model_id, provider, model).is_none()
        });
    let discovers = store
        .providers()
        .any(|(_, provider)| provider.enabled && provider.discovery().is_some());
    if !has_chat_model && !discovers {
        findings.errors.push(if store.errors().is_empty() {
            "no usable model: no enabled provider defines a model a session can use; run `cockpit-code init --provider <template> --default-model <provider>/<model>`".into()
        } else {
            "no usable model: no enabled provider with a model a session can use was loaded; fix the skipped files above".into()
        });
    }
    let config = store.config();
    if config.default_model.is_none() {
        findings.warnings.push(
            "config.json has no defaultModel; sessions need --model <provider>/<model>".into(),
        );
    }
    let active = matches!(
        config.permissions.mode,
        PermissionMode::Auto | PermissionMode::Unattended
    );
    for (key, id, role) in [
        ("defaultModel", &config.default_model, Role::Chat),
        (
            "permissions.autoModel",
            &config.permissions.auto_model,
            Role::Safety { active },
        ),
    ] {
        let Some(id) = id else { continue };
        match unresolved(store, id, role) {
            Some(Problem::Broken(why)) => findings.errors.push(format!("{key} {id}: {why}")),
            Some(Problem::Pending(why)) => findings.warnings.push(format!("{key} {id}: {why}")),
            None => {}
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_config::Directories;
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    /// A fresh empty directory, removed when dropped.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("cockpit-code-doctor-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn store(dir: &Path, config: &str, providers: &[(&str, &str)]) -> ConfigStore {
        fs::create_dir_all(dir.join("providers")).unwrap();
        fs::write(dir.join("config.json"), config).unwrap();
        for (name, body) in providers {
            fs::write(dir.join("providers").join(format!("{name}.json")), body).unwrap();
        }
        ConfigStore::load(Directories {
            config: dir.into(),
            data: dir.into(),
            cache: dir.join("cache"),
        })
        .unwrap()
    }

    const MODEL: &str = r#"{"maxOutputTokens":8192,"capabilities":{"input":["text"],"output":["text"],"tools":true}}"#;

    fn provider(key: &str, enabled: bool) -> String {
        format!(
            r#"{{"type":"openai-chat","baseUrl":"https://example.invalid/v1","headers":{{"Authorization":"Bearer ${{{key}}}"}},"enabled":{enabled},"models":{{"m":{MODEL}}}}}"#
        )
    }

    #[test]
    fn every_provider_skipped_is_an_error() {
        let dir = TempDir::new("skipped");
        let p = provider("COCKPIT_CODE_DOCTOR_TEST_UNSET_KEY", true);
        let store = store(dir.path(), r#"{"defaultModel":"p/m"}"#, &[("p", &p)]);
        assert_eq!(store.errors().len(), 1, "{:?}", store.errors());
        let findings = check(&store);
        assert_eq!(findings.errors.len(), 2, "{findings:?}");
        assert!(findings.errors[0].starts_with("no usable model"));
        assert!(findings.errors[1].contains("provider p was skipped"));
    }

    #[test]
    fn no_provider_at_all_is_an_error() {
        let dir = TempDir::new("empty");
        let findings = check(&store(dir.path(), "{}", &[]));
        assert_eq!(findings.errors.len(), 1, "{findings:?}");
        assert!(findings.errors[0].contains("cockpit-code init"));
        assert_eq!(findings.warnings.len(), 1);
    }

    #[test]
    fn default_model_must_resolve_to_an_enabled_provider_model() {
        let dir = TempDir::new("default");
        let ok = provider("PATH", true);
        let off = provider("PATH", false);
        let cases = [
            (r#"{"defaultModel":"p/m"}"#, None),
            (
                r#"{"defaultModel":"p/x"}"#,
                Some("provider p has no model x"),
            ),
            (
                r#"{"defaultModel":"q/m"}"#,
                Some("provider q is not configured"),
            ),
            (
                r#"{"defaultModel":"off/m"}"#,
                Some("provider off is disabled"),
            ),
        ];
        for (config, expected) in cases {
            let findings = check(&store(dir.path(), config, &[("p", &ok), ("off", &off)]));
            match expected {
                None => assert_eq!(findings, Findings::default(), "{config}"),
                Some(text) => {
                    assert_eq!(findings.errors.len(), 1, "{config}: {findings:?}");
                    assert!(findings.errors[0].contains(text), "{config}: {findings:?}");
                }
            }
        }
    }

    #[test]
    fn an_unresolved_safety_model_is_an_error_in_every_mode() {
        // Session startup validates the configured safety model whatever the mode.
        let dir = TempDir::new("safety");
        let ok = provider("PATH", true);
        for mode in ["manual", "auto", "yolo"] {
            let config = format!(
                r#"{{"defaultModel":"p/m","permissions":{{"mode":"{mode}","autoModel":"p/x"}}}}"#
            );
            let findings = check(&store(dir.path(), &config, &[("p", &ok)]));
            assert_eq!(findings.errors.len(), 1, "{mode}: {findings:?}");
            assert!(findings.errors[0].starts_with("permissions.autoModel p/x"));
        }
    }

    #[test]
    fn roles_need_text_capabilities() {
        let dir = TempDir::new("roles");
        let models = r#"{
            "image":{"maxOutputTokens":8192,"capabilities":{"input":["text"],"output":["image"],"tools":false}},
            "vision":{"maxOutputTokens":8192,"capabilities":{"input":["image"],"output":["text"],"tools":true}}
        }"#;
        let p = format!(
            r#"{{"type":"openai-chat","baseUrl":"https://example.invalid/v1","models":{models}}}"#
        );
        let only_image = format!(
            r#"{{"type":"openai-chat","baseUrl":"https://example.invalid/v1","models":{{"image":{}}}}}"#,
            r#"{"maxOutputTokens":8192,"capabilities":{"input":["text"],"output":["image"],"tools":false}}"#
        );
        let findings = check(&store(
            dir.path(),
            r#"{"defaultModel":"p/image","permissions":{"autoModel":"p/vision"}}"#,
            &[("p", &p)],
        ));
        assert_eq!(findings.errors.len(), 2, "{findings:?}");
        assert!(findings.errors[0].contains("chat requires a text-output model"));
        assert!(findings.errors[1].contains("needs text input and text output"));
        let findings = check(&store(
            dir.path(),
            r#"{"defaultModel":"p/vision"}"#,
            &[("p", &p)],
        ));
        assert_eq!(findings, Findings::default());
        let findings = check(&store(dir.path(), r#"{}"#, &[("p", &only_image)]));
        assert!(
            findings.errors[0].starts_with("no usable model"),
            "{findings:?}"
        );
    }

    #[test]
    fn chat_models_are_checked_like_the_engine_builds_them() {
        let dir = TempDir::new("engine");
        let zero = r#"{"type":"openai-chat","baseUrl":"https://example.invalid/v1","models":{"m":{"maxOutputTokens":0,"capabilities":{"input":["text"],"output":["text"],"tools":true}}}}"#;
        let ftp = r#"{"type":"openai-chat","baseUrl":"ftp://example.invalid/v1","models":{"m":{"capabilities":{"input":["text"],"output":["text"],"tools":true}}}}"#;
        for (body, expected) in [
            (zero, "maxOutputTokens must be positive"),
            (ftp, "baseUrl must be HTTP(S)"),
        ] {
            let findings = check(&store(
                dir.path(),
                r#"{"defaultModel":"p/m"}"#,
                &[("p", body)],
            ));
            assert_eq!(findings.errors.len(), 2, "{expected}: {findings:?}");
            assert!(
                findings.errors[0].starts_with("no usable model"),
                "{findings:?}"
            );
            assert!(findings.errors[1].contains(expected), "{findings:?}");
        }
    }

    #[test]
    fn a_disabled_safety_provider_matters_only_when_the_mode_uses_it() {
        let dir = TempDir::new("disabled-safety");
        let ok = provider("PATH", true);
        let off = provider("PATH", false);
        let manual = check(&store(
            dir.path(),
            r#"{"defaultModel":"p/m","permissions":{"autoModel":"off/m"}}"#,
            &[("p", &ok), ("off", &off)],
        ));
        assert!(manual.errors.is_empty(), "{manual:?}");
        assert!(manual.warnings[0].contains("provider off is disabled"));
        let auto = check(&store(
            dir.path(),
            r#"{"defaultModel":"p/m","permissions":{"mode":"unattended","autoModel":"off/m"}}"#,
            &[("p", &ok), ("off", &off)],
        ));
        assert_eq!(auto.errors.len(), 1, "{auto:?}");
        // Startup still rejects a safety model without text input, disabled or not.
        let image_only = r#"{"type":"openai-chat","baseUrl":"https://example.invalid/v1","enabled":false,"models":{"m":{"capabilities":{"input":["image"],"output":["text"],"tools":false}}}}"#;
        let manual = check(&store(
            dir.path(),
            r#"{"defaultModel":"p/m","permissions":{"autoModel":"off/m"}}"#,
            &[("p", &ok), ("off", image_only)],
        ));
        assert_eq!(manual.errors.len(), 1, "{manual:?}");
        assert!(manual.errors[0].starts_with("permissions.autoModel off/m"));
    }

    #[test]
    fn a_chat_model_still_to_be_discovered_is_a_warning_a_safety_model_is_not() {
        // Sessions refresh discovery after they validate the safety model.
        let dir = TempDir::new("discovery");
        let command = r#"{"type":"command","command":{"argv":["adapter"],"discover":{"argv":["adapter","models"]}}}"#;
        let findings = check(&store(
            dir.path(),
            r#"{"defaultModel":"c/m","permissions":{"autoModel":"c/s"}}"#,
            &[("c", command)],
        ));
        assert_eq!(findings.warnings.len(), 1, "{findings:?}");
        assert!(findings.warnings[0].contains("cockpit-code discover c"));
        assert_eq!(findings.errors.len(), 1, "{findings:?}");
        assert!(findings.errors[0].starts_with("permissions.autoModel c/s"));
    }
}
