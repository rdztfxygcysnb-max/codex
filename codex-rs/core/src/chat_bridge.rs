//! Bridges codex core types into the chat wire adapter (`codex_api::chat_wire`).
//!
//! Used by the `wire_api = "chat"` transport: converts the prompt history into
//! chat messages and codex tool specs into chat function specs.

use crate::client_common::Prompt;
use codex_api::chat_wire::HistoryItem;
use codex_api::chat_wire::ToolSpec as ChatToolSpec;
use codex_api::chat_wire::apply_patch_function_spec;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_tools::ToolSpec;

/// Converts the prompt history into chat wire items: a merged leading system
/// message, user/assistant messages, tool calls and their outputs.
///
/// Freeform tool calls (`apply_patch`) travel as a single `input` string and
/// are wrapped as `{"input": "..."}`, matching the function variant built by
/// [`chat_tools_from_prompt`].
pub fn chat_history_from_prompt(prompt: &Prompt) -> Vec<HistoryItem> {
    let mut items = Vec::new();
    let instructions = prompt.base_instructions.text.trim();
    if !instructions.is_empty() {
        items.push(HistoryItem::System(instructions.to_string()));
    }
    for item in &prompt.input {
        match item {
            ResponseItem::Message { role, content, .. } => {
                let text = content_text(content);
                if text.is_empty() {
                    continue;
                }
                match role.as_str() {
                    "user" => items.push(HistoryItem::User(text)),
                    "assistant" => items.push(HistoryItem::Assistant(text)),
                    "system" | "developer" => items.push(HistoryItem::System(text)),
                    _ => {}
                }
            }
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                items.push(HistoryItem::ToolCall {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                });
            }
            ResponseItem::FunctionCallOutput { call_id, output, .. } => {
                if let Some(call_id) = call_id {
                    items.push(HistoryItem::ToolOutput {
                        call_id: call_id.clone(),
                        output: output_text(output),
                    });
                }
            }
            ResponseItem::CustomToolCall {
                call_id,
                name,
                input,
                ..
            } => {
                let arguments = serde_json::json!({ "input": input }).to_string();
                items.push(HistoryItem::ToolCall {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments,
                });
            }
            ResponseItem::CustomToolCallOutput { call_id, output, .. } => {
                items.push(HistoryItem::ToolOutput {
                    call_id: call_id.clone(),
                    output: output_text(output),
                });
            }
            _ => {}
        }
    }
    items
}

fn content_text(content: &[ContentItem]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for item in content {
        if let ContentItem::InputText { text } = item {
            parts.push(text.as_str());
        }
    }
    parts.join("\n")
}

fn output_text(output: &FunctionCallOutputPayload) -> String {
    match &output.body {
        FunctionCallOutputBody::Text(text) => text.clone(),
        FunctionCallOutputBody::ContentItems(items) => {
            let mut parts: Vec<String> = Vec::new();
            for item in items {
                if let FunctionCallOutputContentItem::InputText { text } = item {
                    parts.push(text.clone());
                }
            }
            parts.join("\n")
        }
    }
}

/// Converts codex tool specs into chat function specs.
///
/// Freeform tools are converted to their function variant (`apply_patch` uses
/// the function schema from `chat_wire`); namespaced, tool-search and
/// web-search tools have no chat representation and are skipped.
pub fn chat_tools_from_prompt(tools: &[ToolSpec]) -> Vec<ChatToolSpec> {
    let mut out = Vec::new();
    for tool in tools {
        match tool {
            ToolSpec::Function(function) => {
                let parameters = serde_json::to_value(&function.parameters).unwrap_or_else(
                    |_| serde_json::json!({ "type": "object", "properties": {} }),
                );
                out.push(ChatToolSpec {
                    name: function.name.clone(),
                    description: function.description.clone(),
                    parameters,
                });
            }
            ToolSpec::Freeform(freeform) => {
                if freeform.name == "apply_patch" {
                    out.push(apply_patch_function_spec());
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_tools::FreeformTool;
    use codex_tools::FreeformToolFormat;
    use codex_tools::ResponsesApiTool;

    fn message(role: &str, text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: role.to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    #[test]
    fn history_maps_roles_and_tools() {
        let prompt = Prompt {
            input: vec![
                message("user", "hello"),
                message("assistant", "hi"),
                ResponseItem::FunctionCall {
                    id: None,
                    name: "shell".to_string(),
                    namespace: None,
                    arguments: "{\"cmd\":\"ls\"}".to_string(),
                    encrypted_function_args: None,
                    call_id: "c1".to_string(),
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: Some("c1".to_string()),
                    name: None,
                    namespace: None,
                    output: FunctionCallOutputPayload {
                        body: FunctionCallOutputBody::Text("ok".to_string()),
                        success: Some(true),
                    },
                    internal_chat_message_metadata_passthrough: None,
                },
            ],
            ..Default::default()
        };
        let items = chat_history_from_prompt(&prompt);
        assert_eq!(items.len(), 4);
        assert!(matches!(&items[0], HistoryItem::User(t) if t == "hello"));
        assert!(matches!(&items[1], HistoryItem::Assistant(t) if t == "hi"));
        match &items[2] {
            HistoryItem::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                assert_eq!(call_id, "c1");
                assert_eq!(name, "shell");
                assert_eq!(arguments, "{\"cmd\":\"ls\"}");
            }
            other => panic!("unexpected item: {other:?}"),
        }
        assert!(matches!(&items[3], HistoryItem::ToolOutput { call_id, output } if call_id == "c1" && output == "ok"));
    }

    #[test]
    fn custom_tool_call_wraps_input_as_json() {
        let prompt = Prompt {
            input: vec![ResponseItem::CustomToolCall {
                id: None,
                status: None,
                call_id: "c9".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                input: "*** Begin Patch\n*** End Patch".to_string(),
                internal_chat_message_metadata_passthrough: None,
            }],
            ..Default::default()
        };
        let items = chat_history_from_prompt(&prompt);
        match &items[0] {
            HistoryItem::ToolCall {
                name, arguments, ..
            } => {
                assert_eq!(name, "apply_patch");
                let parsed: serde_json::Value = serde_json::from_str(arguments).expect("json");
                assert_eq!(
                    parsed.get("input").and_then(|v| v.as_str()),
                    Some("*** Begin Patch\n*** End Patch")
                );
            }
            other => panic!("unexpected item: {other:?}"),
        }
    }

    #[test]
    fn tools_convert_function_and_freeform_apply_patch() {
        let tools = vec![
            ToolSpec::Function(ResponsesApiTool {
                name: "shell".to_string(),
                description: "run a command".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::parse_tool_input_schema(&serde_json::json!({
                    "type": "object",
                    "properties": { "cmd": { "type": "string" } }
                }))
                .expect("schema"),
                output_schema: None,
            }),
            ToolSpec::Freeform(FreeformTool {
                name: "apply_patch".to_string(),
                description: "edit files".to_string(),
                defer_loading: None,
                format: FreeformToolFormat {
                    r#type: "grammar".to_string(),
                    syntax: "lark".to_string(),
                    definition: "start: /.*/".to_string(),
                },
            }),
        ];
        let converted = chat_tools_from_prompt(&tools);
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].name, "shell");
        assert_eq!(converted[1].name, "apply_patch");
        assert_eq!(
            converted[1].parameters.get("type").and_then(|v| v.as_str()),
            Some("object")
        );
    }
}
