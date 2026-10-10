use serde_json::{Map, Value};

use crate::feature_operations::{PromptSummary, ResourceSummary};
use crate::features::common::{ResourceContent, ResourceData};
use crate::features::completion::CompletionResult;
use crate::features::prompts::PromptGetResult;
use crate::tool_result::{UNSENT_BINARY_RESOURCE, project_media_for_text};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeatureAction {
    ResourceList,
    ResourceTemplates,
    ResourceRead,
    PromptList,
    PromptGet,
    PromptComplete,
    ResourceComplete,
}

impl FeatureAction {
    const ALL: [Self; 7] = [
        Self::ResourceList,
        Self::ResourceTemplates,
        Self::ResourceRead,
        Self::PromptList,
        Self::PromptGet,
        Self::PromptComplete,
        Self::ResourceComplete,
    ];

    pub(crate) fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.as_str() == text)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ResourceList => "resource_list",
            Self::ResourceTemplates => "resource_templates",
            Self::ResourceRead => "resource_read",
            Self::PromptList => "prompt_list",
            Self::PromptGet => "prompt_get",
            Self::PromptComplete => "prompt_complete",
            Self::ResourceComplete => "resource_complete",
        }
    }

    fn feature(self) -> &'static str {
        match self {
            Self::ResourceList
            | Self::ResourceRead
            | Self::ResourceTemplates
            | Self::ResourceComplete => "resources",
            Self::PromptList | Self::PromptGet | Self::PromptComplete => "prompts",
        }
    }
}

pub(crate) fn resource_catalog(
    action: FeatureAction,
    server: &str,
    items: &[ResourceSummary],
    template: bool,
) -> String {
    let items = items
        .iter()
        .map(|item| {
            let mut entry = Map::new();
            entry.insert("server".to_owned(), Value::from(server));
            entry.insert("identity".to_owned(), Value::from(item.identity.as_str()));
            entry.insert("name".to_owned(), Value::from(item.name.as_str()));
            insert_optional(&mut entry, "title", item.title.as_deref());
            insert_optional(&mut entry, "description", item.description.as_deref());
            insert_optional(&mut entry, "mimeType", item.mime_type.as_deref());
            entry.insert("template".to_owned(), Value::Bool(template));
            Value::Object(entry)
        })
        .collect();
    let mut envelope = envelope(action, server);
    envelope.insert("items".to_owned(), Value::Array(items));
    Value::Object(envelope).to_string()
}

pub(crate) fn resource_read(server: &str, uri: &str, contents: &[ResourceContent]) -> String {
    let contents = contents
        .iter()
        .map(|content| {
            let mut entry = Map::new();
            entry.insert("uri".to_owned(), Value::from(content.uri.as_str()));
            insert_optional(&mut entry, "mimeType", content.mime_type.as_deref());
            insert_json(
                &mut entry,
                "annotations",
                content.annotations_json.as_deref(),
            );
            insert_json(&mut entry, "_meta", content.metadata_json.as_deref());
            match &content.data {
                ResourceData::Text(text) => {
                    entry.insert("type".to_owned(), Value::from("text"));
                    entry.insert("text".to_owned(), Value::from(text.as_str()));
                }
                ResourceData::Blob(_) => {
                    entry.insert("type".to_owned(), Value::from("blob"));
                    entry.insert("delivery".to_owned(), Value::from(UNSENT_BINARY_RESOURCE));
                }
            }
            Value::Object(entry)
        })
        .collect();
    let mut envelope = envelope(FeatureAction::ResourceRead, server);
    envelope.insert("identity".to_owned(), Value::from(uri));
    envelope.insert("contents".to_owned(), Value::Array(contents));
    Value::Object(envelope).to_string()
}

pub(crate) fn prompt_catalog(server: &str, items: &[PromptSummary]) -> String {
    let items = items
        .iter()
        .map(|item| {
            let mut entry = Map::new();
            entry.insert("server".to_owned(), Value::from(server));
            entry.insert("identity".to_owned(), Value::from(item.name.as_str()));
            insert_optional(&mut entry, "title", item.title.as_deref());
            insert_optional(&mut entry, "description", item.description.as_deref());
            let arguments = item
                .arguments
                .iter()
                .map(|argument| {
                    let mut described = Map::new();
                    described.insert("name".to_owned(), Value::from(argument.name.as_str()));
                    described.insert("required".to_owned(), Value::Bool(argument.required));
                    insert_optional(
                        &mut described,
                        "description",
                        argument.description.as_deref(),
                    );
                    Value::Object(described)
                })
                .collect();
            entry.insert("arguments".to_owned(), Value::Array(arguments));
            Value::Object(entry)
        })
        .collect();
    let mut envelope = envelope(FeatureAction::PromptList, server);
    envelope.insert("items".to_owned(), Value::Array(items));
    Value::Object(envelope).to_string()
}

pub(crate) fn prompt_get(server: &str, name: &str, result: &PromptGetResult) -> String {
    let messages = result
        .messages
        .iter()
        .map(|message| {
            let mut content = serde_json::from_str(&message.content_json).unwrap_or(Value::Null);
            project_media_for_text(&mut content);
            let mut entry = Map::new();
            entry.insert("role".to_owned(), Value::from(message.role.as_str()));
            entry.insert(
                "contentKind".to_owned(),
                Value::from(message.content_kind.as_str()),
            );
            entry.insert("content".to_owned(), content);
            Value::Object(entry)
        })
        .collect();
    let mut envelope = envelope(FeatureAction::PromptGet, server);
    envelope.insert("identity".to_owned(), Value::from(name));
    insert_optional(&mut envelope, "description", result.description.as_deref());
    envelope.insert("messages".to_owned(), Value::Array(messages));
    Value::Object(envelope).to_string()
}

pub(crate) fn completion(
    action: FeatureAction,
    server: &str,
    identity: &str,
    argument: &str,
    result: &CompletionResult,
) -> String {
    let mut envelope = envelope(action, server);
    envelope.insert("identity".to_owned(), Value::from(identity));
    envelope.insert("argument".to_owned(), Value::from(argument));
    envelope.insert(
        "values".to_owned(),
        result.values.iter().map(String::as_str).collect(),
    );
    if let Some(total) = result.total {
        envelope.insert("total".to_owned(), Value::from(total));
    }
    if let Some(has_more) = result.has_more {
        envelope.insert("hasMore".to_owned(), Value::Bool(has_more));
    }
    Value::Object(envelope).to_string()
}

pub(crate) fn unsupported(action: FeatureAction, server: &str) -> String {
    let mut envelope = envelope(action, server);
    envelope.insert("unsupported".to_owned(), Value::Bool(true));
    envelope.insert(
        "message".to_owned(),
        Value::from(format!(
            "{server} did not advertise a {} capability, so this feature is unavailable on that server. Use its tools or pick another server.",
            action.feature()
        )),
    );
    Value::Object(envelope).to_string()
}

fn envelope(action: FeatureAction, server: &str) -> Map<String, Value> {
    let mut envelope = Map::new();
    envelope.insert("trust".to_owned(), Value::from("untrusted_external"));
    envelope.insert("authority".to_owned(), Value::from("none"));
    envelope.insert("action".to_owned(), Value::from(action.as_str()));
    envelope.insert("server".to_owned(), Value::from(server));
    envelope
}

fn insert_optional(object: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        object.insert(key.to_owned(), Value::from(value));
    }
}

fn insert_json(object: &mut Map<String, Value>, key: &str, json: Option<&str>) {
    if let Some(value) = json.and_then(|json| serde_json::from_str(json).ok()) {
        object.insert(key.to_owned(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::prompts::{PromptArgument, PromptContentKind, PromptMessage, PromptRole};

    const ENVELOPE: &str = r#"{"trust":"untrusted_external","authority":"none""#;

    fn resource(identity: &str, details: [Option<&str>; 3]) -> ResourceSummary {
        let [title, description, mime_type] = details.map(|value| value.map(str::to_owned));
        ResourceSummary {
            identity: identity.to_owned(),
            name: "plan".to_owned(),
            title,
            description,
            mime_type,
        }
    }

    #[test]
    fn every_action_keeps_its_upstream_name() {
        for action in FeatureAction::ALL {
            assert_eq!(FeatureAction::parse(action.as_str()), Some(action));
        }
        assert_eq!(FeatureAction::parse("resource_subscribe"), None);
        assert_eq!(FeatureAction::parse("Resource_list"), None);
    }

    #[test]
    fn catalogs_list_each_item_with_its_server_and_identity() {
        assert_eq!(
            resource_catalog(
                FeatureAction::ResourceList,
                "docs",
                &[
                    resource(
                        "memory://plan",
                        [Some("Plan"), Some("The \"plan\""), Some("text/markdown")]
                    ),
                    resource("memory://notes", [None; 3]),
                ],
                false,
            ),
            format!(
                r#"{ENVELOPE},"action":"resource_list","server":"docs","items":[{{"server":"docs","identity":"memory://plan","name":"plan","title":"Plan","description":"The \"plan\"","mimeType":"text/markdown","template":false}},{{"server":"docs","identity":"memory://notes","name":"plan","template":false}}]}}"#
            )
        );
        assert_eq!(
            resource_catalog(
                FeatureAction::ResourceTemplates,
                "docs",
                &[resource("memory://{id}", [None, None, Some("text/plain")])],
                true,
            ),
            format!(
                r#"{ENVELOPE},"action":"resource_templates","server":"docs","items":[{{"server":"docs","identity":"memory://{{id}}","name":"plan","mimeType":"text/plain","template":true}}]}}"#
            )
        );
        assert_eq!(
            resource_catalog(FeatureAction::ResourceList, "docs", &[], false),
            format!(r#"{ENVELOPE},"action":"resource_list","server":"docs","items":[]}}"#)
        );
        let prompts = [
            PromptSummary {
                name: "review".to_owned(),
                title: Some("Review".to_owned()),
                description: Some("Review code".to_owned()),
                arguments: vec![
                    PromptArgument {
                        name: "focus".to_owned(),
                        description: Some("What to check".to_owned()),
                        required: true,
                    },
                    PromptArgument {
                        name: "depth".to_owned(),
                        description: None,
                        required: false,
                    },
                ],
            },
            PromptSummary {
                name: "plain".to_owned(),
                title: None,
                description: None,
                arguments: Vec::new(),
            },
        ];
        assert_eq!(
            prompt_catalog("docs", &prompts),
            format!(
                r#"{ENVELOPE},"action":"prompt_list","server":"docs","items":[{{"server":"docs","identity":"review","title":"Review","description":"Review code","arguments":[{{"name":"focus","required":true,"description":"What to check"}},{{"name":"depth","required":false}}]}},{{"server":"docs","identity":"plain","arguments":[]}}]}}"#
            )
        );
    }

    #[test]
    fn reads_carry_text_and_describe_binary_contents_as_unsent() {
        let contents = [
            ResourceContent {
                uri: "memory://plan".to_owned(),
                mime_type: Some("text/markdown".to_owned()),
                annotations_json: Some(r#"{"priority":0.5,"audience":["user"]}"#.to_owned()),
                metadata_json: Some(r#"{"z":1,"a":true}"#.to_owned()),
                data: ResourceData::Text("line\none".to_owned()),
            },
            ResourceContent {
                uri: "memory://logo".to_owned(),
                mime_type: Some("image/png".to_owned()),
                annotations_json: None,
                metadata_json: None,
                data: ResourceData::Blob("aGk=".to_owned()),
            },
        ];
        assert_eq!(
            resource_read("docs", "memory://plan", &contents),
            format!(
                r#"{ENVELOPE},"action":"resource_read","server":"docs","identity":"memory://plan","contents":[{{"uri":"memory://plan","mimeType":"text/markdown","annotations":{{"priority":0.5,"audience":["user"]}},"_meta":{{"z":1,"a":true}},"type":"text","text":"line\none"}},{{"uri":"memory://logo","mimeType":"image/png","type":"blob","delivery":"binary resource content was not sent to the model"}}]}}"#
            )
        );
    }

    #[test]
    fn prompt_messages_keep_their_content_and_describe_media_as_unsent() {
        let message = |role, content_kind, content_json: &str| PromptMessage {
            role,
            content_kind,
            content_json: content_json.to_owned(),
        };
        let result = PromptGetResult {
            description: Some("Review the change".to_owned()),
            messages: vec![
                message(
                    PromptRole::User,
                    PromptContentKind::Text,
                    r#"{"type":"text","text":"PROMPT_TEXT","_meta":{"n":1000.0}}"#,
                ),
                message(
                    PromptRole::Assistant,
                    PromptContentKind::Image,
                    r#"{"type":"image","data":"aGk=","mimeType":"image/png","annotations":{"priority":1}}"#,
                ),
                message(
                    PromptRole::User,
                    PromptContentKind::Resource,
                    r#"{"type":"resource","resource":{"uri":"file:///a","blob":"aGk=","mimeType":"application/pdf"}}"#,
                ),
            ],
        };
        assert_eq!(
            prompt_get("docs", "review", &result),
            format!(
                r#"{ENVELOPE},"action":"prompt_get","server":"docs","identity":"review","description":"Review the change","messages":[{{"role":"user","contentKind":"text","content":{{"type":"text","text":"PROMPT_TEXT","_meta":{{"n":1000.0}}}}}},{{"role":"assistant","contentKind":"image","content":{{"type":"image","annotations":{{"priority":1}},"mimeType":"image/png","delivery":"unsupported media; content was not sent to the model"}}}},{{"role":"user","contentKind":"resource","content":{{"type":"resource","resource":{{"uri":"file:///a","mimeType":"application/pdf","delivery":"binary resource content was not sent to the model"}}}}}}]}}"#
            )
        );
        let bare = PromptGetResult {
            description: None,
            messages: Vec::new(),
        };
        assert_eq!(
            prompt_get("docs", "plain", &bare),
            format!(
                r#"{ENVELOPE},"action":"prompt_get","server":"docs","identity":"plain","messages":[]}}"#
            )
        );
    }

    #[test]
    fn completions_name_their_reference_and_argument() {
        let mut result = CompletionResult {
            values: vec!["balpha".to_owned(), "beta".to_owned()],
            total: Some(5),
            has_more: Some(false),
        };
        assert_eq!(
            completion(
                FeatureAction::PromptComplete,
                "docs",
                "review",
                "tone",
                &result
            ),
            format!(
                r#"{ENVELOPE},"action":"prompt_complete","server":"docs","identity":"review","argument":"tone","values":["balpha","beta"],"total":5,"hasMore":false}}"#
            )
        );
        result.total = None;
        result.has_more = None;
        assert_eq!(
            completion(
                FeatureAction::ResourceComplete,
                "docs",
                "custom://project/{path}",
                "path",
                &result
            ),
            format!(
                r#"{ENVELOPE},"action":"resource_complete","server":"docs","identity":"custom://project/{{path}}","argument":"path","values":["balpha","beta"]}}"#
            )
        );
    }

    #[test]
    fn unsupported_features_name_the_missing_capability() {
        for (action, feature) in [
            (FeatureAction::ResourceList, "resources"),
            (FeatureAction::ResourceTemplates, "resources"),
            (FeatureAction::ResourceRead, "resources"),
            (FeatureAction::ResourceComplete, "resources"),
            (FeatureAction::PromptList, "prompts"),
            (FeatureAction::PromptGet, "prompts"),
            (FeatureAction::PromptComplete, "prompts"),
        ] {
            assert_eq!(
                unsupported(action, "tools"),
                format!(
                    r#"{ENVELOPE},"action":"{}","server":"tools","unsupported":true,"message":"tools did not advertise a {feature} capability, so this feature is unavailable on that server. Use its tools or pick another server."}}"#,
                    action.as_str()
                )
            );
        }
    }
}
