use aria_core::provider::{
    ChatMessage, OpenAiProvider, Provider, ProviderEvent, ToolSpec, TurnResult,
};
use tokio::sync::mpsc;

/// Serve `body` on 127.0.0.1 and return the provider pointed at it.
fn serve(body: &'static str) -> (OpenAiProvider, u16) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.flush();
        }
    });
    let settings = aria_core::config::Settings {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        model: "test-model".into(),
        ..Default::default()
    };
    let provider = OpenAiProvider::new(settings, "sk-test").unwrap();
    (provider, port)
}

async fn run_fixture(fixture: &'static str) -> (TurnResult, Vec<ProviderEvent>, &'static str) {
    let raw: &'static str = match fixture {
        "text" => concat!(
            include_str!("fixtures/text.sse"),
        ),
        "tools" => include_str!("fixtures/tools.sse"),
        "truncated" => include_str!("fixtures/truncated.sse"),
        "no_usage" => include_str!("fixtures/no_usage_empty_args.sse"),
        _ => unreachable!(),
    };
    let (provider, _port) = serve(raw);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let result = provider
        .stream_chat(&[ChatMessage::user("hi")], &[], tx)
        .await;
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    (result.unwrap(), events, fixture)
}

#[tokio::test]
async fn plain_text_with_usage() {
    let (result, events, _) = run_fixture("text").await;
    assert_eq!(result.text, "Hello there");
    assert!(result.tool_calls.is_empty());
    assert_eq!(result.prompt_tokens, Some(11));
    assert_eq!(
        events,
        vec![
            ProviderEvent::TextDelta("Hello".into()),
            ProviderEvent::TextDelta(" there".into()),
            ProviderEvent::Usage { prompt_tokens: 11 },
        ]
    );
}

#[tokio::test]
async fn two_tool_calls_accumulate_fragments() {
    let (result, events, _) = run_fixture("tools").await;
    assert_eq!(result.text, "Let me look.");
    assert_eq!(result.tool_calls.len(), 2);
    assert_eq!(result.tool_calls[0].id, "call_abc");
    assert_eq!(result.tool_calls[0].name(), "read_file");
    assert_eq!(result.tool_calls[0].arguments(), "{\"path\":\"a.txt\"}");
    assert_eq!(result.tool_calls[1].id, "call_def");
    assert_eq!(result.tool_calls[1].name(), "list_dir");
    assert_eq!(result.prompt_tokens, Some(42));
    let calls: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, ProviderEvent::ToolCall(_)))
        .collect();
    assert_eq!(calls.len(), 2);
}

#[tokio::test]
async fn truncated_stream_is_an_error_and_yields_no_tool_calls() {
    let raw: &'static str = include_str!("fixtures/truncated.sse");
    let (provider, _port) = serve(raw);
    let (tx, _rx) = mpsc::unbounded_channel();
    let err = provider
        .stream_chat(&[ChatMessage::user("go")], &[], tx)
        .await
        .unwrap_err();
    assert!(matches!(err, aria_core::provider::ProviderError::Truncated));
}

#[tokio::test]
async fn empty_tool_arguments_become_an_empty_object() {
    let (result, _events, _) = run_fixture("no_usage").await;
    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(result.tool_calls[0].arguments(), "{}");
    assert_eq!(result.prompt_tokens, None);
}

#[tokio::test]
async fn http_error_status_is_reported() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let body = "{\"error\":\"bad key\"}";
            let _ = write!(s, "HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}", body.len(), body);
        }
    });
    let settings = aria_core::config::Settings {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        model: "m".into(),
        ..Default::default()
    };
    let provider = OpenAiProvider::new(settings, "bad").unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let err = provider
        .stream_chat(&[ChatMessage::user("x")], &[ToolSpec { name: "t".into(), description: "d".into(), parameters: serde_json::json!({"type":"object"}) }], tx)
        .await
        .unwrap_err();
    match err {
        aria_core::provider::ProviderError::Status { status, body } => {
            assert_eq!(status, 401);
            assert!(body.contains("bad key"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn missing_model_is_rejected_before_any_request() {
    let settings = aria_core::config::Settings::default();
    assert!(matches!(
        OpenAiProvider::new(settings, "k"),
        Err(aria_core::provider::ProviderError::MissingModel)
    ));
}

#[test]
fn messages_serialise_in_openai_shape() {
    use aria_core::provider::ToolCallRequest;
    let msgs = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("hi"),
        ChatMessage::assistant(
            "calling",
            vec![ToolCallRequest::new("c1", "read_file", "{}")],
        ),
        ChatMessage::tool_result("c1", "contents"),
    ];
    let json = serde_json::to_value(&msgs).unwrap();
    assert_eq!(json[0]["role"], "system");
    // Tool calls must use the nested wire shape a real endpoint expects.
    assert_eq!(json[2]["tool_calls"][0]["id"], "c1");
    assert_eq!(json[2]["tool_calls"][0]["type"], "function");
    assert_eq!(json[2]["tool_calls"][0]["function"]["name"], "read_file");
    assert_eq!(json[2]["tool_calls"][0]["function"]["arguments"], "{}");
    assert!(json[2]["tool_calls"][0].get("name").is_none());
    assert_eq!(json[3]["role"], "tool");
    assert_eq!(json[3]["tool_call_id"], "c1");
}

#[test]
fn tool_call_requests_round_trip_through_the_wire_shape() {
    use aria_core::provider::ToolCallRequest;
    let call = ToolCallRequest::new("c1", "read_file", "{\"path\":\"a\"}");
    let text = serde_json::to_string(&call).unwrap();
    let back: ToolCallRequest = serde_json::from_str(&text).unwrap();
    assert_eq!(back, call);
    assert_eq!(back.name(), "read_file");
}

#[test]
fn embedding_endpoint_and_model_fall_back_to_chat_settings() {
    let settings = aria_core::config::Settings {
        base_url: "https://api.openai.com/v1/".into(),
        model: "m".into(),
        embedding_base_url: None,
        embedding_model: None,
        ..Default::default()
    };
    let p = OpenAiProvider::new(settings, "k").unwrap();
    assert_eq!(p.embedding_endpoint(), "https://api.openai.com/v1/embeddings");
    assert_eq!(p.embedding_model(), "text-embedding-3-small");
}

#[test]
fn embedding_settings_can_be_overridden() {
    let settings = aria_core::config::Settings {
        base_url: "https://api.openai.com/v1".into(),
        model: "m".into(),
        embedding_base_url: Some("https://emb.example/".into()),
        embedding_model: Some("  custom-model  ".into()),
        ..Default::default()
    };
    let p = OpenAiProvider::new(settings, "k").unwrap();
    assert_eq!(p.embedding_endpoint(), "https://emb.example/embeddings");
    // Spec trims only for the emptiness check, not the returned value.
    assert_eq!(p.embedding_model(), "  custom-model  ");
}

/// Serve an embeddings response, returning the provider plus the captured request.
fn serve_embed(response: &'static str) -> (OpenAiProvider, std::sync::mpsc::Receiver<String>) {
    use std::io::{Read, Write};
    let (tx, rx) = std::sync::mpsc::channel();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                response.len(),
                response
            );
        }
    });
    let settings = aria_core::config::Settings {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        model: "m".into(),
        ..Default::default()
    };
    let provider = OpenAiProvider::new(settings, "sk-test").unwrap();
    (provider, rx)
}

#[tokio::test]
async fn embed_returns_first_embedding_vector() {
    let (provider, rx) = serve_embed(
        r#"{"object":"list","data":[{"object":"embedding","embedding":[0.1,0.2,0.3],"index":0}]}"#,
    );
    let embedding = provider.embed("hello world").await.unwrap();
    assert_eq!(embedding, vec![0.1, 0.2, 0.3]);

    let request = rx.recv().unwrap();
    let body = request.split("\r\n\r\n").nth(1).unwrap_or("");
    let sent: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(sent["input"], "hello world");
    assert_eq!(sent["model"], "text-embedding-3-small");
    assert!(request.to_lowercase().contains("authorization: bearer sk-test"));
}

#[tokio::test]
async fn embed_reports_non_success_status() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let body = "{\"error\":\"bad key\"}";
            let _ = write!(s, "HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}", body.len(), body);
        }
    });
    let settings = aria_core::config::Settings {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        model: "m".into(),
        ..Default::default()
    };
    let provider = OpenAiProvider::new(settings, "bad").unwrap();
    let err = provider.embed("x").await.unwrap_err();
    match err {
        aria_core::provider::ProviderError::Status { status, body } => {
            assert_eq!(status, 401);
            assert!(body.contains("bad key"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}
