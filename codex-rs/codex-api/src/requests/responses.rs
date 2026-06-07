use codex_protocol::models::ResponseItem;
use serde_json::Value;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Compression {
    #[default]
    None,
    Zstd,
}

/// Reattaches the item IDs historically sent with stored/stateful Responses requests.
///
/// `ResponseItem` IDs are skipped by serde so request modes can choose their attachment policy
/// after serializing the common request body. Stateful requests use the legacy narrower policy:
/// attach IDs for prompt items the stateful Responses path may treat as existing stored items,
/// but do not opt every Codex-owned output item into the newer stateless stable-ID behavior.
pub(crate) fn attach_stateful_response_item_ids(
    payload_json: &mut Value,
    original_items: &[ResponseItem],
) {
    let Some(input_value) = payload_json.get_mut("input") else {
        return;
    };
    let Value::Array(items) = input_value else {
        return;
    };

    for (value, item) in items.iter_mut().zip(original_items.iter()) {
        if let ResponseItem::Reasoning { id, .. }
        | ResponseItem::Message { id: Some(id), .. }
        | ResponseItem::WebSearchCall { id: Some(id), .. }
        | ResponseItem::FunctionCall { id: Some(id), .. }
        | ResponseItem::ToolSearchCall { id: Some(id), .. }
        | ResponseItem::LocalShellCall { id: Some(id), .. }
        | ResponseItem::CustomToolCall { id: Some(id), .. } = item
        {
            if id.is_empty() {
                continue;
            }

            if let Some(obj) = value.as_object_mut() {
                obj.insert("id".to_string(), Value::String(id.clone()));
            }
        }
    }
}

/// Reattaches every non-empty stable item ID before sending a stateless Responses request.
///
/// Stateless requests resend the full prompt history by value, so the server only sees stable
/// item identity when Codex explicitly puts each known ID back into the serialized `input` array.
/// This includes Codex-generated IDs for new local prompt items and IDs preserved from server
/// output, compaction, and rollout replay.
pub(crate) fn attach_stateless_response_item_ids(
    payload_json: &mut Value,
    original_items: &[ResponseItem],
) {
    let Some(Value::Array(items)) = payload_json.get_mut("input") else {
        return;
    };

    for (value, item) in items.iter_mut().zip(original_items.iter()) {
        item.attach_id_to_json(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::AgentMessageInputContent;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::LocalShellAction;
    use codex_protocol::models::LocalShellExecAction;
    use codex_protocol::models::LocalShellStatus;
    use codex_protocol::models::WebSearchAction;
    use pretty_assertions::assert_eq;

    #[test]
    fn attaches_stateless_response_item_ids_to_input_json() {
        let items = vec![
            ResponseItem::Message {
                id: Some("msg_1".to_string()),
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "hello".to_string(),
                }],
                phase: None,
            },
            ResponseItem::AgentMessage {
                id: Some("amsg_1".to_string()),
                author: "/root".to_string(),
                recipient: "/root/worker".to_string(),
                content: vec![AgentMessageInputContent::EncryptedContent {
                    encrypted_content: "opaque".to_string(),
                }],
            },
            ResponseItem::Reasoning {
                id: "rs_1".to_string(),
                summary: Vec::new(),
                content: None,
                encrypted_content: None,
            },
            ResponseItem::LocalShellCall {
                id: Some("lsh_1".to_string()),
                call_id: Some("call_shell".to_string()),
                status: LocalShellStatus::Completed,
                action: LocalShellAction::Exec(LocalShellExecAction {
                    command: vec!["echo".to_string(), "ok".to_string()],
                    timeout_ms: None,
                    working_directory: None,
                    env: None,
                    user: None,
                }),
            },
            ResponseItem::FunctionCall {
                id: Some("fc_1".to_string()),
                name: "shell".to_string(),
                namespace: None,
                arguments: "{}".to_string(),
                call_id: "call_function".to_string(),
            },
            ResponseItem::ToolSearchCall {
                id: Some("tsc_1".to_string()),
                call_id: Some("call_search".to_string()),
                status: Some("completed".to_string()),
                execution: "client".to_string(),
                arguments: serde_json::json!({}),
            },
            ResponseItem::FunctionCallOutput {
                id: Some("fco_1".to_string()),
                call_id: "call_1".to_string(),
                output: FunctionCallOutputPayload::from_text("ok".to_string()),
            },
            ResponseItem::CustomToolCall {
                id: Some("ctc_1".to_string()),
                status: Some("completed".to_string()),
                call_id: "call_custom".to_string(),
                name: "apply_patch".to_string(),
                input: "{}".to_string(),
            },
            ResponseItem::CustomToolCallOutput {
                id: Some("ctco_1".to_string()),
                call_id: "call_custom".to_string(),
                name: Some("apply_patch".to_string()),
                output: FunctionCallOutputPayload::from_text("ok".to_string()),
            },
            ResponseItem::ToolSearchOutput {
                id: Some("tso_1".to_string()),
                call_id: Some("call_search".to_string()),
                status: "completed".to_string(),
                execution: "client".to_string(),
                tools: Vec::new(),
            },
            ResponseItem::WebSearchCall {
                id: Some("ws_1".to_string()),
                status: Some("completed".to_string()),
                action: Some(WebSearchAction::Search {
                    query: Some("weather".to_string()),
                    queries: None,
                }),
            },
            ResponseItem::ImageGenerationCall {
                id: "ig_1".to_string(),
                status: "completed".to_string(),
                revised_prompt: None,
                result: "image".to_string(),
            },
            ResponseItem::Compaction {
                id: Some("cmp_1".to_string()),
                encrypted_content: "opaque".to_string(),
            },
        ];
        let mut payload = serde_json::json!({
            "input": serde_json::to_value(&items).expect("serialize input"),
        });

        attach_stateless_response_item_ids(&mut payload, &items);

        let ids = payload["input"]
            .as_array()
            .expect("input array")
            .iter()
            .map(|item| item["id"].as_str().expect("item id"))
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                "msg_1", "amsg_1", "rs_1", "lsh_1", "fc_1", "tsc_1", "fco_1", "ctc_1", "ctco_1",
                "tso_1", "ws_1", "ig_1", "cmp_1",
            ]
        );
    }

    #[test]
    fn attach_stateless_response_item_ids_leaves_missing_ids_unchanged() {
        let items = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: Vec::new(),
            phase: None,
        }];
        let mut payload = serde_json::json!({
            "input": serde_json::to_value(&items).expect("serialize input"),
        });

        attach_stateless_response_item_ids(&mut payload, &items);

        assert_eq!(payload["input"][0].get("id"), None);
    }
}
