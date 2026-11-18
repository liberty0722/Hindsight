//! 聊天功能的真实 HTTP 往返集成测试:起一个本地 mock OpenAI-compatible
//! server,验证请求体/AUTH 头/响应解析/错误处理全链路(不依赖外网与 GUI)。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use hindsight_core::chat::{self, ChatMessage, ChatRole};
use hindsight_core::models::AgentId;
use serde_json::{json, Value};

/// 在随机端口起一个一次性的 HTTP server,返回 (端口, 收到的请求信息)。
/// `handler` 收到请求体,返回 (status, 响应体)。
fn spawn_mock(handler: impl FnOnce(&str) -> (u16, String) + Send + 'static) -> (u16, thread::JoinHandle<(String, String)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap();
        let raw = String::from_utf8_lossy(&buf[..n]).to_string();
        // 分离 header 与 body
        let split = raw.find("\r\n\r\n").unwrap_or(raw.len());
        let headers = raw[..split].to_string();
        let body = if split + 4 <= raw.len() {
            raw[split + 4..].to_string()
        } else {
            String::new()
        };
        let (status, resp) = handler(&body);
        let reason = if status == 200 { "OK" } else { "Bad Request" };
        let reply = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp}",
            resp.as_bytes().len()
        );
        stream.write_all(reply.as_bytes()).unwrap();
        stream.flush().unwrap();
        (headers, body)
    });
    (port, handle)
}

#[test]
fn full_round_trip_request_and_response_are_correct() {
    let (port, handle) = spawn_mock(|body| {
        // 校验请求体:model、system 提示、消息历史顺序
        let v: Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["model"], "gpt-4o");
        assert_eq!(v["stream"], false);
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3); // 1 条 system + 2 条历史
        assert_eq!(msgs[0]["role"], "system");
        assert!(msgs[0]["content"].as_str().unwrap().contains("Codex"));
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "what is 2+2?");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "thinking");
        let ok = json!({
            "choices": [{"message": {"role": "assistant", "content": "It is 4."}}]
        });
        (200, ok.to_string())
    });

    let history = vec![
        ChatMessage {
            role: ChatRole::User,
            content: "what is 2+2?".into(),
            timestamp: 1,
            kind: chat::ChatMessageKind::Normal,
        },
        ChatMessage {
            role: ChatRole::Assistant,
            content: "thinking".into(),
            timestamp: 2,
            kind: chat::ChatMessageKind::Normal,
        },
    ];
    let body = chat::build_request_body("gpt-4o", AgentId::Codex, &history).to_string();
    let url = chat::completions_url(&format!("http://127.0.0.1:{port}/v1")).unwrap();
    let reply = chat::send_once(&url, "test-key", &body).unwrap();
    assert_eq!(reply, "It is 4.");

    let (headers, _body) = handle.join().unwrap();
    // 校验 POST 路径、Content-Type、Authorization(header 名大小写不敏感)
    assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1"), "{headers}");
    let lower = headers.to_ascii_lowercase();
    assert!(lower.contains("content-type: application/json"), "{headers}");
    assert!(lower.contains("authorization: bearer test-key"), "{headers}");
}

#[test]
fn non_2xx_uses_api_error_message() {
    let (port, handle) = spawn_mock(|_| {
        let err = json!({"error": {"message": "invalid api key"}});
        (401, err.to_string())
    });
    let body = chat::build_request_body("m", AgentId::With, &[]).to_string();
    let url = chat::completions_url(&format!("http://127.0.0.1:{port}/v1")).unwrap();
    let err = chat::send_once(&url, "bad", &body).unwrap_err();
    assert_eq!(err, "invalid api key");
    handle.join().unwrap();
}

#[test]
fn array_content_response_is_parsed_end_to_end() {
    let (port, handle) = spawn_mock(|_| {
        let ok = json!({
            "choices": [{"message": {"content": [
                {"type": "text", "text": "Hello "},
                {"type": "text", "text": "world."}
            ]}}]
        });
        (200, ok.to_string())
    });
    let body = chat::build_request_body("m", AgentId::ClaudeCode, &[]).to_string();
    let url = chat::completions_url(&format!("http://127.0.0.1:{port}/v1")).unwrap();
    let reply = chat::send_once(&url, "", &body).unwrap();
    assert_eq!(reply, "Hello world.");
    let (headers, _) = handle.join().unwrap();
    // 无 key 时不应带 Authorization
    assert!(!headers.contains("Authorization"), "{headers}");
}
