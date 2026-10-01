use serde_json::{Map, Value};

use crate::project_config::{
    DISABLED_SERVERS_KEY, ENABLE_ALL_KEY, ENABLED_SERVERS_KEY, InvalidProjectMcpChoices,
    ProjectMcpAction, ProjectMcpChoices,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectMcpSettingsChange {
    pub changed: bool,
    pub authority_reduced: bool,
}

pub fn apply_project_mcp_action(
    workspace: &mut Map<String, Value>,
    action: &ProjectMcpAction,
) -> Result<ProjectMcpSettingsChange, InvalidProjectMcpChoices> {
    let current =
        ProjectMcpChoices::parse(Some(&Value::Object(workspace.clone())), &mut Vec::new())?;
    let transition = current.apply(action);
    let mut changed =
        put_string_array(workspace, ENABLED_SERVERS_KEY, &transition.choices.approved);
    changed |= put_string_array(
        workspace,
        DISABLED_SERVERS_KEY,
        &transition.choices.rejected,
    );
    changed |= if transition.choices.enable_all {
        put_bool(workspace, ENABLE_ALL_KEY, true)
    } else {
        workspace.shift_remove(ENABLE_ALL_KEY).is_some()
    };
    Ok(ProjectMcpSettingsChange {
        changed,
        authority_reduced: transition.authority_reduced,
    })
}

fn put_string_array(object: &mut Map<String, Value>, key: &str, values: &[String]) -> bool {
    if values.is_empty() {
        return object.shift_remove(key).is_some();
    }
    let unchanged = object
        .get(key)
        .and_then(Value::as_array)
        .is_some_and(|existing| {
            existing.len() == values.len()
                && existing
                    .iter()
                    .zip(values)
                    .all(|(field, value)| field.as_str() == Some(value.as_str()))
        });
    if unchanged {
        return false;
    }
    object.insert(
        key.to_owned(),
        Value::Array(values.iter().cloned().map(Value::String).collect()),
    );
    true
}

fn put_bool(object: &mut Map<String, Value>, key: &str, value: bool) -> bool {
    if object.get(key).and_then(Value::as_bool) == Some(value) {
        return false;
    }
    object.insert(key.to_owned(), Value::Bool(value));
    true
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(object) => object,
            _ => unreachable!(),
        }
    }

    #[test]
    fn actions_write_only_the_trust_keys_and_report_reduced_authority() {
        let mut workspace = object(json!({"model": "m"}));
        let change =
            apply_project_mcp_action(&mut workspace, &ProjectMcpAction::Approve("a".to_owned()))
                .unwrap();
        assert!(change.changed);
        assert!(!change.authority_reduced);
        assert_eq!(
            Value::Object(workspace.clone()),
            json!({"model": "m", "enabledMcpjsonServers": ["a"]})
        );

        let change =
            apply_project_mcp_action(&mut workspace, &ProjectMcpAction::ApproveAll).unwrap();
        assert!(change.changed);
        assert_eq!(
            Value::Object(workspace.clone()),
            json!({"model": "m", "enabledMcpjsonServers": ["a"], "enableAllProjectMcpServers": true})
        );

        let change = apply_project_mcp_action(&mut workspace, &ProjectMcpAction::Reset).unwrap();
        assert!(change.changed);
        assert!(change.authority_reduced);
        assert_eq!(Value::Object(workspace.clone()), json!({"model": "m"}));

        let unchanged = apply_project_mcp_action(&mut workspace, &ProjectMcpAction::Reset).unwrap();
        assert_eq!(unchanged, ProjectMcpSettingsChange::default());
    }

    #[test]
    fn rejecting_moves_a_server_between_lists() {
        let mut workspace = object(json!({"enabledMcpjsonServers": ["a", "b"]}));
        let change =
            apply_project_mcp_action(&mut workspace, &ProjectMcpAction::Reject("a".to_owned()))
                .unwrap();
        assert!(change.authority_reduced);
        assert_eq!(
            Value::Object(workspace),
            json!({"enabledMcpjsonServers": ["b"], "disabledMcpjsonServers": ["a"]})
        );
    }

    #[test]
    fn malformed_choices_are_refused_without_a_write() {
        let mut workspace = object(json!({"enabledMcpjsonServers": "a"}));
        assert_eq!(
            apply_project_mcp_action(&mut workspace, &ProjectMcpAction::ApproveAll),
            Err(InvalidProjectMcpChoices)
        );
        assert_eq!(
            Value::Object(workspace),
            json!({"enabledMcpjsonServers": "a"})
        );
    }
}
