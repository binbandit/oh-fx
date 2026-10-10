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
