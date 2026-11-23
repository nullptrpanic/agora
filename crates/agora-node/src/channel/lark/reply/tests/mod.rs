use super::*;
use crate::channel::test_http::{HttpMockServer, MockResponse, RecordedRequest};
use crate::config::LarkChannelConfig;
use serde_json::{Value, json};

mod api;
mod content;
mod delivery;
mod images;

fn response(request: &RecordedRequest) -> MockResponse {
    MockResponse::json(if request.endpoint() == "internal" {
        r#"{"code":0,"msg":"ok","tenant_access_token":"token","expire":7200}"#
    } else if request.path.contains("message_cot?") {
        r#"{"code":0,"msg":"ok","data":{"cot_id":"cot","message_id":"process"}}"#
    } else {
        r#"{"code":0,"msg":"ok","data":{"message_id":"answer"}}"#
    })
}

fn api(server: &HttpMockServer) -> LarkApi {
    LarkApi::with_base_url(
        LarkChannelConfig {
            name: "test".into(),
            app_id: "app".into(),
            secret: "secret".into(),
            permission: Default::default(),
            proxy: None,
        },
        server.base_url(),
    )
    .unwrap()
}

fn reply(server: &HttpMockServer) -> LarkAgentReply {
    LarkAgentReply::new(
        LarkReplyTarget {
            message_id: "source".into(),
            mention_user_id: Some("ou_sender".into()),
        },
        "chat".into(),
        "agent".into(),
        api(server),
    )
}

fn body(request: &RecordedRequest) -> Value {
    serde_json::from_str(&request.body).unwrap()
}

fn post_text(request: &RecordedRequest) -> String {
    let post: Value = serde_json::from_str(body(request)["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        post["zh_cn"]["content"][0][0],
        json!({"tag":"at", "user_id":"ou_sender"})
    );
    post["zh_cn"]["content"][1][0]["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn events(requests: &[RecordedRequest]) -> Vec<Value> {
    requests
        .iter()
        .filter(|r| r.method == "PUT" && r.path.ends_with("message_cot"))
        .flat_map(|r| body(r)["events"].as_array().unwrap().clone())
        .collect()
}

fn event_content(event: &Value) -> Value {
    serde_json::from_str(event["content"].as_str().unwrap()).unwrap()
}

fn answer(text: &str) -> RunEvent {
    RunEvent::Output(OutputEvent::Answer { text: text.into() })
}

fn commentary(id: &str, text: &str) -> RunEvent {
    RunEvent::Output(OutputEvent::Commentary {
        id: id.into(),
        text: text.into(),
    })
}
