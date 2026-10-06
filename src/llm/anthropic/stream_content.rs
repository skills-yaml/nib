//! Ephemeral native assistant blocks for Anthropic tool-result replay.
//! This state deliberately has no Debug or public projection.
use crate::llm::ToolCallRequest;
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct NativeStreamContent {
    blocks: BTreeMap<usize, NativeBlock>,
    missing_start: bool,
    terminal_seen: bool,
}

struct NativeBlock {
    value: Value,
    arguments: String,
    closed: bool,
}

fn index(data: &Value) -> Result<usize, String> {
    data.get("index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| "Anthropic native content is missing a valid index".to_string())
}

impl NativeStreamContent {
    pub(super) fn observe(&mut self, kind: &str, data: &Value) -> Result<(), String> {
        if kind.starts_with("content_block_") && self.terminal_seen {
            return Err(
                "Anthropic stream changed native content after its terminal reason".to_string(),
            );
        }
        if kind == "message_delta"
            && data
                .get("delta")
                .and_then(|delta| delta.get("stop_reason"))
                .is_some()
        {
            self.terminal_seen = true;
        }
        match kind {
            "content_block_start" => {
                let index = index(data)?;
                if self.blocks.contains_key(&index) {
                    return Err("Anthropic stream restarted a native content block".to_string());
                }
                let value = data
                    .get("content_block")
                    .filter(|value| value.is_object())
                    .ok_or_else(|| "Anthropic native content block is missing".to_string())?
                    .clone();
                if value.get("type").and_then(Value::as_str).is_none() {
                    return Err("Anthropic native content block is missing its type".to_string());
                }
                crate::llm::ensure_response_item_count(
                    self.blocks.len() + 1,
                    "Anthropic native content blocks",
                )?;
                self.blocks.insert(
                    index,
                    NativeBlock {
                        value,
                        arguments: String::new(),
                        closed: false,
                    },
                );
            }
            "content_block_delta" => {
                let index = index(data)?;
                let delta = data
                    .get("delta")
                    .filter(|value| value.is_object())
                    .ok_or_else(|| "Anthropic native content delta is missing".to_string())?;
                let Some(block) = self.blocks.get_mut(&index) else {
                    // Retain the pre-existing text-only delta projection. A tool
                    // turn cannot authorize replay of a missing native block.
                    if delta.get("text").is_some() {
                        self.missing_start = true;
                        return Ok(());
                    }
                    return Err("Anthropic native content delta has no matching block".to_string());
                };
                block.append(delta)?;
            }
            "content_block_stop" => {
                let index = index(data)?;
                let block = self.blocks.get_mut(&index).ok_or_else(|| {
                    "Anthropic native content stop has no matching block".to_string()
                })?;
                if block.closed {
                    return Err("Anthropic native content block stopped twice".to_string());
                }
                block.closed = true;
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn finish(self, calls: &[ToolCallRequest]) -> Result<Vec<Value>, String> {
        if self.missing_start {
            return Err("Anthropic tool turn is missing native content starts".to_string());
        }
        let mut content = Vec::with_capacity(self.blocks.len());
        let mut tool_count = 0;
        for (expected, (index, block)) in self.blocks.into_iter().enumerate() {
            if index != expected || !block.closed {
                return Err("Anthropic tool turn has incomplete native content blocks".to_string());
            }
            if block.value["type"] == "tool_use" {
                tool_count += 1;
            }
            content.push(block.finish(calls)?);
        }
        if tool_count != calls.len() {
            return Err("Anthropic native tool content disagrees with validated calls".to_string());
        }
        Ok(content)
    }
}

impl NativeBlock {
    fn append(&mut self, delta: &Value) -> Result<(), String> {
        if self.closed {
            return Err("Anthropic stream changed a stopped native content block".to_string());
        }
        let fields = ["text", "thinking", "signature", "partial_json"];
        let present = fields
            .into_iter()
            .filter(|field| delta.get(*field).is_some())
            .collect::<Vec<_>>();
        if present.len() != 1 {
            return Err("Anthropic native content delta is unsupported or ambiguous".to_string());
        }
        let field = present[0];
        let fragment = delta[field]
            .as_str()
            .ok_or_else(|| "Anthropic native content fragment must be a string".to_string())?;
        let (block_type, delta_type) = match field {
            "text" => ("text", "text_delta"),
            "thinking" => ("thinking", "thinking_delta"),
            "signature" => ("thinking", "signature_delta"),
            "partial_json" => ("tool_use", "input_json_delta"),
            _ => unreachable!(),
        };
        if self.value["type"] != block_type
            || delta.get("type").is_some_and(|kind| kind != delta_type)
        {
            return Err("Anthropic native content delta has the wrong block type".to_string());
        }
        if field == "partial_json" {
            self.arguments.push_str(fragment);
        } else {
            if field == "signature" && self.value.get(field).is_none() {
                self.value[field] = json!("");
            }
            let Some(Value::String(target)) = self.value.get_mut(field) else {
                return Err("Anthropic native content block field must be a string".to_string());
            };
            target.push_str(fragment);
        }
        Ok(())
    }

    fn finish(mut self, calls: &[ToolCallRequest]) -> Result<Value, String> {
        match self.value["type"].as_str() {
            Some("thinking") => {
                if self.value.get("thinking").and_then(Value::as_str).is_none()
                    || self
                        .value
                        .get("signature")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
                {
                    return Err(
                        "Anthropic native thinking block is missing its text or signature"
                            .to_string(),
                    );
                }
            }
            Some("redacted_thinking") => {
                if self
                    .value
                    .get("data")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return Err(
                        "Anthropic native redacted thinking block is missing data".to_string()
                    );
                }
            }
            Some("text") => {
                if self.value.get("text").and_then(Value::as_str).is_none() {
                    return Err("Anthropic native text block is missing text".to_string());
                }
            }
            Some("tool_use") => self.finish_tool(calls)?,
            _ => {}
        }
        Ok(self.value)
    }

    fn finish_tool(&mut self, calls: &[ToolCallRequest]) -> Result<(), String> {
        let call = calls
            .iter()
            .find(|call| {
                call.call_id
                    .as_ref()
                    .is_some_and(|id| self.value["id"] == id.as_str())
            })
            .ok_or_else(|| "Anthropic native tool content has no validated call".to_string())?;
        let initial = self
            .value
            .get("input")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let arguments = if self.arguments.is_empty() {
            initial
        } else {
            if initial != json!({}) {
                return Err("Anthropic native tool input conflicts with its deltas".to_string());
            }
            serde_json::from_str::<Value>(&self.arguments)
                .map_err(|_| "Anthropic native tool input contains malformed JSON".to_string())?
        };
        if !arguments.is_object() || arguments != call.arguments || self.value["name"] != call.name
        {
            return Err(
                "Anthropic native tool input disagrees with its validated call".to_string(),
            );
        }
        self.value["input"] = arguments;
        Ok(())
    }
}
