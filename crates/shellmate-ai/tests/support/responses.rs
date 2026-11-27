use rig_core::test_utils::MockHttpResponse;
use serde_json::{Value, json};

pub fn message(id: &str, text: &str, phase: &str) -> Value {
    json!({"type":"message","id":id,"status":"completed","role":"assistant","phase":phase,
        "content":[{"type":"output_text","text":text,"annotations":[]}]})
}
pub fn reasoning(id: &str) -> Value {
    json!({"type":"reasoning","id":id,"encrypted_content":format!("CIPHER_{id}"),
        "summary":[{"type":"summary_text","text":"PRIVATE_REASONING"}]})
}
pub fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"type":"function_call","id":format!("fc_{id}"),"call_id":id,"name":name,
        "arguments":args.to_string(),"status":"completed"})
}
pub fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}
pub fn sse(items: Vec<Value>, status: &str) -> String {
    let mut text = event(
        json!({"type":"response.created","response":{"id":"resp_fixture","status":"in_progress","output":[]}}),
    );
    for (index, item) in items.iter().enumerate() {
        let mut added = item.clone();
        if item["type"] == "function_call" {
            added["arguments"] = "".into();
        }
        if item["type"] == "message" {
            added["content"] = json!([]);
        }
        text +=
            &event(json!({"type":"response.output_item.added","output_index":index,"item":added}));
        if item["type"] == "message" {
            text += &event(
                json!({"type":"response.output_text.delta","output_index":index,"item_id":item["id"],"content_index":0,"delta":item["content"][0]["text"]}),
            );
        }
        if item["type"] == "function_call" {
            let args = item["arguments"].as_str().unwrap();
            // Deliberately split JSON across frames, including UTF-8 in the body.
            let midpoint = args
                .char_indices()
                .nth(args.chars().count() / 2)
                .map_or(args.len(), |(i, _)| i);
            for chunk in [&args[..midpoint], &args[midpoint..]] {
                text += &event(
                    json!({"type":"response.function_call_arguments.delta","output_index":index,"item_id":item["id"],"delta":chunk}),
                );
            }
            text += &event(
                json!({"type":"response.function_call_arguments.done","output_index":index,"item_id":item["id"],"arguments":args}),
            );
        }
        text +=
            &event(json!({"type":"response.output_item.done","output_index":index,"item":item}));
    }
    text += &event(
        json!({"type":format!("response.{status}"),"response":{"id":"resp_fixture","model":"gpt-5.4","status":status,"output":items,
        "incomplete_details":if status=="incomplete" {json!({"reason":"max_output_tokens"})} else {Value::Null},
        "usage":{"input_tokens":32,"output_tokens":16,"total_tokens":48}}}),
    );
    text
}
pub fn reply(items: Vec<Value>) -> MockHttpResponse {
    MockHttpResponse::success_typed(sse(items, "completed"), "text/event-stream")
}
