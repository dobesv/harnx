//! OpenAI-compatible Chat Completions request bodies.
use super::*;
use harnx_core::text::strip_think_tag;

pub fn openai_build_chat_completions_body(data: ChatCompletionsData, model: &Model) -> Value {
    let mut body = json!({
        "model": model.real_name(),
        "messages": messages(data.messages, model),
    });
    set_output_limit(&mut body, model);
    if let Some(value) = data.temperature {
        body["temperature"] = value.into();
    }
    if let Some(value) = data.top_p {
        body["top_p"] = value.into();
    }
    if data.stream {
        body["stream"] = true.into();
        body["stream_options"] = json!({"include_usage": true});
    }
    if let Some(functions) = data.functions {
        body["tools"] = functions
            .iter()
            .map(|function| {
                json!({
                    "type": "function", "function": function,
                })
            })
            .collect();
    }
    body
}

fn messages(messages: Vec<Message>, model: &Model) -> Vec<Value> {
    let count = messages.len();
    messages
        .into_iter()
        .enumerate()
        .flat_map(|(index, message)| {
            let Message { role, content, .. } = message;
            match content {
                MessageContent::ToolCalls(calls) => tool_replay::messages(calls, model),
                MessageContent::Text(text) if role.is_assistant() && index != count - 1 => {
                    vec![json!({"role": role, "content": strip_think_tag(&text)})]
                }
                _ => vec![json!({"role": role, "content": content})],
            }
        })
        .collect()
}

fn set_output_limit(body: &mut Value, model: &Model) {
    let Some(limit) = model.max_tokens_param() else {
        return;
    };
    // Catalog patches setting max_tokens to null select max_completion_tokens.
    let completion_limit = model.patches().is_some_and(|patches| {
        patches
            .iter()
            .any(|patch| patch.contains(".max_tokens") && patch.contains("null"))
    });
    let key = if completion_limit {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    body[key] = limit.into();
}
