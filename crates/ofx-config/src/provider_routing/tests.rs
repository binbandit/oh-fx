use std::fs;
use std::path::PathBuf;

use serde_json::json;

use super::*;
use crate::{ProfilePaths, Settings};

fn layer(value: &Value) -> Result<RoutingLayer, LayerError> {
    let Value::Object(object) = value else {
        panic!("a layer is an object");
    };
    RoutingLayer::parse(object)
}

fn slugs(order: &[&str]) -> Vec<String> {
    order.iter().map(|slug| (*slug).to_owned()).collect()
}

fn no_environment(_: &str) -> Option<String> {
    None
}

#[test]
fn a_layer_reads_the_order_and_strict_switch() {
    assert_eq!(
        layer(&json!({"provider_order": ["azure", "bedrock"], "provider_strict": true})),
        Ok(RoutingLayer {
            order: Some(slugs(&["azure", "bedrock"])),
            strict: Some(true),
        })
    );
    assert_eq!(
        layer(&json!({"provider_order": []})),
        Ok(RoutingLayer {
            order: Some(Vec::new()),
            strict: None,
        })
    );
    assert_eq!(layer(&json!({})), Ok(RoutingLayer::default()));
}

#[test]
fn a_layer_rejects_malformed_routing_with_upstream_s_errors() {
    let long = "a".repeat(65);
    for (value, error) in [
        (
            json!({"provider_order": "azure"}),
            LayerError::InvalidProviderOrderType,
        ),
        (
            json!({"provider_order": ["a", "b", "c", "d", "e", "f", "g", "h", "i"]}),
            LayerError::InvalidProviderOrderValue,
        ),
        (
            json!({"provider_order": ["Bad Slug"]}),
            LayerError::InvalidProviderOrderValue,
        ),
        (
            json!({"provider_order": ["-azure"]}),
            LayerError::InvalidProviderOrderValue,
        ),
        (
            json!({"provider_order": [long]}),
            LayerError::InvalidProviderOrderValue,
        ),
        (
            json!({"provider_order": ["azure", "azure"]}),
            LayerError::InvalidProviderOrderValue,
        ),
        (
            json!({"provider_order": [1]}),
            LayerError::InvalidProviderOrderValue,
        ),
        (
            json!({"provider_strict": "yes"}),
            LayerError::InvalidProviderStrictType,
        ),
    ] {
        assert_eq!(layer(&value), Err(error), "{value}");
    }
}

#[test]
fn order_lists_trim_skip_blanks_and_reject_bad_slugs() {
    assert_eq!(
        parse_provider_order_list(" azure, bedrock ,,vertex"),
        Ok(Some(slugs(&["azure", "bedrock", "vertex"])))
    );
    assert_eq!(parse_provider_order_list(" , "), Ok(None));
    for invalid in ["azure,azure", "Bad Slug", "a,b,c,d,e,f,g,h,i"] {
        assert_eq!(parse_provider_order_list(invalid), Err(InvalidOrder));
    }
}

#[test]
fn the_workspace_outranks_the_profile_which_outranks_the_project_field_by_field() {
    let workspace = layer(&json!({"provider_order": ["vertex"]})).unwrap();
    let global =
        layer(&json!({"provider_order": ["bedrock", "azure"], "provider_strict": true})).unwrap();
    let project =
        layer(&json!({"provider_order": ["azure", "openai"], "provider_strict": false})).unwrap();
    assert_eq!(
        resolve([&workspace, &global, &project], &no_environment),
        ProviderRouting {
            order: slugs(&["vertex"]),
            strict: true,
        }
    );
    let silent = RoutingLayer::default();
    assert_eq!(
        resolve([&silent, &silent, &project], &no_environment),
        ProviderRouting {
            order: slugs(&["azure", "openai"]),
            strict: false,
        }
    );
    let cleared = layer(&json!({"provider_order": []})).unwrap();
    assert_eq!(
        resolve([&cleared, &global, &project], &no_environment),
        ProviderRouting {
            order: Vec::new(),
            strict: true,
        }
    );
    assert_eq!(
        resolve([&silent, &silent, &silent], &no_environment),
        ProviderRouting::default()
    );
}

#[test]
fn the_environment_overrides_settings_and_invalid_values_are_ignored() {
    let global = layer(&json!({"provider_order": ["bedrock"], "provider_strict": true})).unwrap();
    let silent = RoutingLayer::default();
    let with = |order: &'static str, strict: &'static str| {
        move |name: &str| match name {
            ORDER_VARIABLE => Some(order.to_owned()),
            STRICT_VARIABLE => Some(strict.to_owned()),
            _ => None,
        }
    };
    for (order, strict, expected_order, expected_strict) in [
        (" azure, vertex ", " OFF ", vec!["azure", "vertex"], false),
        ("azure,azure", "maybe", vec!["bedrock"], true),
        (" , ", "", vec!["bedrock"], true),
        ("openai", "No", vec!["openai"], false),
        ("openai", "0", vec!["openai"], false),
        ("openai", "false", vec!["openai"], false),
    ] {
        assert_eq!(
            resolve([&silent, &global, &silent], &with(order, strict)),
            ProviderRouting {
                order: slugs(&expected_order),
                strict: expected_strict,
            },
            "{order:?} {strict:?}"
        );
    }
    for on in ["1", "true", "On", "YES"] {
        assert!(
            resolve([&silent, &silent, &silent], &with("", on)).strict,
            "{on}"
        );
    }
}

struct Profile {
    _directory: tempfile::TempDir,
    paths: ProfilePaths,
    workspace: PathBuf,
}

fn profile(settings: &Value, project: Option<&Value>) -> Profile {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config/oh-fx");
    let workspace = directory.path().join("workspace");
    fs::create_dir_all(&config).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let settings = settings
        .to_string()
        .replace("WORKSPACE", &workspace.to_string_lossy());
    fs::write(config.join("settings.json"), settings).unwrap();
    if let Some(project) = project {
        fs::write(workspace.join(".oh-fx.json"), project.to_string()).unwrap();
    }
    Profile {
        paths: ProfilePaths {
            config,
            data: directory.path().join("data"),
            state: directory.path().join("state"),
            cache: directory.path().join("cache"),
        },
        workspace,
        _directory: directory,
    }
}

fn routing(profile: &Profile) -> (ProviderRouting, Vec<String>) {
    let settings = Settings::load(&profile.paths, &profile.workspace).unwrap();
    let diagnostics = settings
        .diagnostics()
        .iter()
        .map(ToString::to_string)
        .collect();
    (settings.provider_routing(&no_environment), diagnostics)
}

#[test]
fn settings_read_routing_from_the_profile_its_workspace_and_the_project_file() {
    let layered = profile(
        &json!({
            "provider_order": ["bedrock", "azure"],
            "provider_strict": true,
            "workspaces": {"WORKSPACE": {"provider_order": ["vertex"], "provider_strict": false}},
        }),
        Some(&json!({"provider_order": ["azure", "openai"]})),
    );
    assert_eq!(
        routing(&layered),
        (
            ProviderRouting {
                order: slugs(&["vertex"]),
                strict: false,
            },
            Vec::new()
        )
    );
    let project_only = profile(
        &json!({}),
        Some(&json!({"provider_order": ["azure", "openai"], "provider_strict": true})),
    );
    assert_eq!(
        routing(&project_only).0,
        ProviderRouting {
            order: slugs(&["azure", "openai"]),
            strict: true,
        }
    );
}

#[test]
fn a_malformed_project_routing_is_diagnosed_and_ignored() {
    let malformed = profile(
        &json!({}),
        Some(&json!({"provider_order": "azure", "max_agent_steps": 3})),
    );
    let (routed, diagnostics) = routing(&malformed);
    assert_eq!(routed, ProviderRouting::default());
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert!(diagnostics[0].contains("project"), "{diagnostics:?}");
}
