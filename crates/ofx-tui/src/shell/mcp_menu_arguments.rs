use ofx_contract::{
    McpMenuArgument, McpMenuCompletionTarget, McpMenuPreviewTarget, McpMenuRequest, UiCommand,
};

use super::Shell;
use super::mcp_menu::{Effect, Event};
use super::mcp_menu_template::{expand_template, template_fields};
use crate::footer::mcp_menu_presentation::{Item, McpMenuArgumentField};
use crate::input::{Action as InputAction, InputEvent};

const INCOMPLETE: &str = "Complete every required MCP argument.";
const CTRL_Z: u8 = 26;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ArgumentForm {
    target: ArgumentTarget,
    pub(super) fields: Vec<McpMenuArgumentField>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ArgumentTarget {
    Prompt {
        server: String,
        name: String,
    },
    Template {
        server: String,
        uri_template: String,
    },
}

impl Shell<'_> {
    pub(super) fn open_mcp_menu_item(&mut self) {
        let Some(menu) = &self.mcp_menu else {
            return;
        };
        let state = &menu.state;
        let server = menu.selected_server_name();
        let opened = match menu.catalogs.item(
            state.section,
            &state.query,
            state.selected_index,
            server.as_deref(),
        ) {
            Some(Item::Tool(name)) => Opened::Preview(McpMenuPreviewTarget::Tool(name.to_owned())),
            Some(Item::Resource(resource)) if !resource.template => match server {
                Some(server) => Opened::Preview(McpMenuPreviewTarget::Resource {
                    server,
                    uri: resource.uri.clone(),
                }),
                None => return,
            },
            Some(Item::Resource(resource)) => match (server, template_fields(&resource.uri)) {
                (Some(server), Ok(fields)) => Opened::Form(
                    ArgumentTarget::Template {
                        server,
                        uri_template: resource.uri.clone(),
                    },
                    fields.into_iter().map(|name| (name, true)).collect(),
                ),
                (Some(_), Err(error)) => Opened::Refused(error),
                (None, _) => return,
            },
            Some(Item::Prompt(prompt)) => match server {
                Some(server) => Opened::Form(
                    ArgumentTarget::Prompt {
                        server,
                        name: prompt.name.clone(),
                    },
                    prompt
                        .arguments
                        .iter()
                        .map(|argument| (argument.name.clone(), argument.required))
                        .collect(),
                ),
                None => return,
            },
            None => return,
        };
        match opened {
            Opened::Preview(target) => self.begin_mcp_menu_preview(target),
            Opened::Form(target, fields) => self.show_mcp_menu_arguments(target, fields),
            Opened::Refused(error) => self.refuse_mcp_menu_preview(error),
        }
    }

    pub(super) fn route_mcp_argument_key(&mut self, event: &InputEvent) -> bool {
        let pending = self
            .mcp_menu
            .as_ref()
            .is_some_and(|menu| menu.state.pending_generation.is_some());
        match event {
            InputEvent::Action(decoded) => match decoded.action {
                InputAction::Escape => {
                    self.composer.clear();
                    self.mcp_menu_back();
                }
                InputAction::CursorLeft => {
                    self.apply_mcp_menu_event(Event::CycleSection(-1));
                }
                InputAction::CursorRight => {
                    self.apply_mcp_menu_event(Event::CycleSection(1));
                }
                InputAction::CursorUp | InputAction::CursorDown => {}
                _ => return false,
            },
            InputEvent::Raw(raw) if raw.byte == CTRL_Z => return false,
            InputEvent::Paste(_)
            | InputEvent::TextDropped(_)
            | InputEvent::NativeClearProbe
            | InputEvent::NativeClearDetected => return false,
            _ if pending => {}
            InputEvent::Raw(raw) => match raw.byte {
                b'\t' => {
                    self.commit_mcp_argument_draft();
                    self.composer.clear();
                    self.complete_mcp_menu_argument();
                }
                b'\r' => {
                    self.commit_mcp_argument_draft();
                    self.composer.clear();
                    self.advance_mcp_menu_arguments();
                }
                _ => return self.route_mcp_form_byte(*raw),
            },
            InputEvent::Text(_) => return false,
        }
        true
    }

    pub(super) fn mcp_menu_completed(&mut self, generation: u64, value: String) {
        let Some(menu) = self.mcp_menu_awaiting(generation) else {
            return;
        };
        if let Some(field) = menu
            .completing
            .take()
            .zip(menu.argument_form.as_mut())
            .and_then(|(index, form)| form.fields.get_mut(index))
        {
            field.value = value;
        }
        menu.feedback = None;
        menu.state.apply(Event::CompletionLoaded(generation));
        self.invalidate();
    }

    fn show_mcp_menu_arguments(&mut self, target: ArgumentTarget, names: Vec<(String, bool)>) {
        let mut fields: Vec<McpMenuArgumentField> = Vec::new();
        for (name, required) in names {
            if !fields.iter().any(|field| field.name == name) {
                fields.push(McpMenuArgumentField {
                    name,
                    required,
                    value: String::new(),
                });
            }
        }
        if fields.is_empty() {
            let preview = match target {
                ArgumentTarget::Prompt { server, name } => McpMenuPreviewTarget::Prompt {
                    server,
                    name,
                    arguments: Vec::new(),
                },
                ArgumentTarget::Template {
                    server,
                    uri_template,
                } => match expand_template(&uri_template, |_| None) {
                    Ok(uri) => McpMenuPreviewTarget::Resource { server, uri },
                    Err(error) => {
                        self.refuse_mcp_menu_preview(error);
                        return;
                    }
                },
            };
            self.begin_mcp_menu_preview(preview);
            return;
        }
        let count = fields.len();
        if let Some(menu) = &mut self.mcp_menu {
            menu.argument_form = Some(ArgumentForm { target, fields });
        }
        self.composer.clear();
        self.apply_mcp_menu_event(Event::ShowArguments(count));
    }

    fn commit_mcp_argument_draft(&mut self) {
        let draft = self.composer.text().to_owned();
        let Some(menu) = &mut self.mcp_menu else {
            return;
        };
        let index = menu.state.argument_index;
        let Some(field) = menu
            .argument_form
            .as_mut()
            .and_then(|form| form.fields.get_mut(index))
        else {
            return;
        };
        if !draft.is_empty() || field.value.is_empty() {
            field.value = draft;
        }
    }

    fn complete_mcp_menu_argument(&mut self) {
        let Some(Effect::Complete { generation }) =
            self.apply_mcp_menu_event(Event::RequestCompletion)
        else {
            return;
        };
        let Some(menu) = &mut self.mcp_menu else {
            return;
        };
        let index = menu.state.argument_index;
        let Some(form) = &menu.argument_form else {
            return;
        };
        let target = match &form.target {
            ArgumentTarget::Prompt { server, name } => McpMenuCompletionTarget::Prompt {
                server: server.clone(),
                name: name.clone(),
            },
            ArgumentTarget::Template {
                server,
                uri_template,
            } => McpMenuCompletionTarget::Resource {
                server: server.clone(),
                uri_template: uri_template.clone(),
            },
        };
        let mut context = arguments(&form.fields);
        if index >= context.len() {
            return;
        }
        let argument = context.remove(index);
        menu.completing = Some(index);
        menu.feedback = None;
        self.send(UiCommand::McpMenu {
            request: McpMenuRequest::Complete {
                generation,
                target,
                argument,
                context,
            },
        });
    }

    fn advance_mcp_menu_arguments(&mut self) {
        let Some(menu) = &mut self.mcp_menu else {
            return;
        };
        let Some(form) = &menu.argument_form else {
            return;
        };
        let field_count = form.fields.len();
        if menu.state.argument_index + 1 < field_count {
            menu.state.apply(Event::MoveArgument {
                delta: 1,
                field_count,
            });
            return;
        }
        if form
            .fields
            .iter()
            .any(|field| field.required && field.value.is_empty())
        {
            menu.feedback = Some(INCOMPLETE.to_owned());
            return;
        }
        let fields = &form.fields;
        let preview = match &form.target {
            ArgumentTarget::Prompt { server, name } => Ok(McpMenuPreviewTarget::Prompt {
                server: server.clone(),
                name: name.clone(),
                arguments: arguments(fields),
            }),
            ArgumentTarget::Template {
                server,
                uri_template,
            } => expand_template(uri_template, |name| {
                fields
                    .iter()
                    .find(|field| field.name == name)
                    .map(|field| field.value.as_str())
            })
            .map(|uri| McpMenuPreviewTarget::Resource {
                server: server.clone(),
                uri,
            }),
        };
        match preview {
            Ok(target) => self.begin_mcp_menu_preview(target),
            Err(error) => self.refuse_mcp_menu_preview(error),
        }
    }

    fn begin_mcp_menu_preview(&mut self, target: McpMenuPreviewTarget) {
        if let Some(Effect::LoadPreview { generation }) =
            self.apply_mcp_menu_event(Event::BeginPreview)
        {
            if let Some(menu) = &mut self.mcp_menu {
                menu.feedback = None;
            }
            self.send(UiCommand::McpMenu {
                request: McpMenuRequest::LoadPreview { generation, target },
            });
        }
    }

    fn refuse_mcp_menu_preview(&mut self, error: &str) {
        if let Some(Effect::LoadPreview { generation }) =
            self.apply_mcp_menu_event(Event::BeginPreview)
            && let Some(menu) = &mut self.mcp_menu
        {
            menu.feedback = Some(format!("MCP preview failed: {error}"));
            menu.state.apply(Event::EffectFailed(generation));
        }
    }
}

enum Opened {
    Preview(McpMenuPreviewTarget),
    Form(ArgumentTarget, Vec<(String, bool)>),
    Refused(&'static str),
}

fn arguments(fields: &[McpMenuArgumentField]) -> Vec<McpMenuArgument> {
    fields
        .iter()
        .map(|field| McpMenuArgument {
            name: field.name.clone(),
            value: field.value.clone(),
        })
        .collect()
}
