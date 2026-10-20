use crate::{
    Config,
    audit::Audit,
    auth::{Profile, Store},
    engine::Hand,
    jobs::Jobs,
};
use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

struct Fixture {
    root: tempfile::TempDir,
    app: Router,
    a: String,
    b: String,
    read: String,
    auth: std::path::PathBuf,
}
fn fixture(rate: u32) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let auth = root.path().join("credentials.json");
    let mut store = Store::default();
    let (_, a) = store
        .issue("a", "A", vec!["*".into()], Profile::Compact, Some(600))
        .unwrap();
    let (_, b) = store
        .issue("b", "B", vec!["*".into()], Profile::Native, Some(600))
        .unwrap();
    let (_, read) = store
        .issue(
            "reader",
            "read",
            vec!["read".into()],
            Profile::Compact,
            Some(600),
        )
        .unwrap();
    store.save(&auth).unwrap();
    let config = Config {
        listen: "127.0.0.1:1".parse().unwrap(),
        auth_file: auth.clone(),
        default_cwd: root.path().to_path_buf(),
        state_dir: root.path().join("state"),
        ssh: None,
        requests_per_minute: rate,
        admin: None,
        protected_paths: vec![],
    };
    let hand = Hand {
        audit: Arc::new(Audit::open(&config.state_dir).unwrap()),
        jobs: Arc::new(Jobs::open(&config.state_dir).unwrap()),
        config: Arc::new(config),
    };
    Fixture {
        app: crate::http::router(hand, CancellationToken::new()),
        root,
        a,
        b,
        read,
        auth,
    }
}
async fn rpc(
    f: &Fixture,
    token: Option<&str>,
    method: &str,
    params: Value,
) -> (StatusCode, Value, HeaderMap) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "127.0.0.1")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-11-25");
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let response = f
        .app
        .clone()
        .oneshot(
            req.body(Body::from(
                json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string(),
            ))
            .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body, headers)
}
async fn tool(f: &Fixture, token: &str, name: &str, args: Value) -> Value {
    let (status, body, _) = rpc(
        f,
        Some(token),
        "tools/call",
        json!({"name":name,"arguments":args}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body
}

#[tokio::test]
async fn stateless_discovery_compact_dispatch_and_scope_enforcement() {
    let f = fixture(240);
    let (status, _, headers) = rpc(&f, None, "tools/list", json!({})).await;
    assert_eq!(status, 401);
    assert!(headers.contains_key("www-authenticate"));
    let(status,body,headers)=rpc(&f,Some(&f.a),"initialize",json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}})).await;
    assert_eq!(status, 200);
    assert!(!headers.contains_key("Mcp-Session-Id"));
    assert!(
        body["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("ApplyPatch")
    );
    let (_, compact, _) = rpc(&f, Some(&f.a), "tools/list", json!({})).await;
    assert_eq!(compact["result"]["tools"].as_array().unwrap().len(), 2);
    let compact_bytes = serde_json::to_vec(&compact["result"]["tools"])
        .unwrap()
        .len();
    assert!(compact_bytes < 1400);
    let (_, native, _) = rpc(&f, Some(&f.b), "tools/list", json!({})).await;
    assert_eq!(native["result"]["tools"].as_array().unwrap().len(), 13);
    let info = tool(&f, &f.a, "Run", json!({"tool":"Info","arguments":{}})).await;
    assert_eq!(info["result"]["structuredContent"]["principal"], "a");
    let denied = tool(
        &f,
        &f.read,
        "Run",
        json!({"tool":"Shell","arguments":{"command":"throw 'must never run'"}}),
    )
    .await;
    assert_eq!(denied["result"]["isError"], true);
    let hidden = tool(&f, &f.read, "Discover", json!({"tool":"Shell"})).await;
    assert_eq!(hidden["result"]["isError"], true);
    let strict = tool(
        &f,
        &f.a,
        "Run",
        json!({"tool":"Read","arguments":{"paths":[],"unexpected":true}}),
    )
    .await;
    assert_eq!(strict["result"]["isError"], true);
    let bypass = tool(&f, &f.read, "Shell", json!({"command":"throw 'bypass'"})).await;
    assert!(bypass["error"].is_object());
    assert!(
        !std::fs::read_to_string(f.root.path().join("state/audit.jsonl"))
            .unwrap()
            .contains("must never run")
    );
}

#[tokio::test]
async fn ownership_retry_rotation_and_revocation_use_authenticated_identity() {
    let f = fixture(240);
    let created = tool(
        &f,
        &f.a,
        "Run",
        json!({"tool":"Write","arguments":{"path":"sample","content":"hello"}}),
    )
    .await;
    let id = created["result"]["structuredContent"]["task_id"].clone();
    assert!(id.is_string());
    for name in ["Tasks", "Cancel"] {
        let response = tool(&f, &f.b, name, json!({"task_id":id})).await;
        assert_eq!(response["result"]["isError"], true);
    }
    let retry = tool(
        &f,
        &f.b,
        "Write",
        json!({"path":"sample","content":"hello","operation_id":id}),
    )
    .await;
    assert_eq!(retry["result"]["isError"], true);
    let list = tool(&f, &f.b, "Tasks", json!({})).await;
    assert_eq!(list["result"]["structuredContent"]["total"], 0);
    let mut store = Store::load(&f.auth).unwrap();
    let (key, new) = store
        .issue(
            "a",
            "rotated",
            vec!["tasks".into()],
            Profile::Native,
            Some(600),
        )
        .unwrap();
    let old = store
        .credentials
        .iter()
        .find(|(_, c)| c.principal == "a" && c.profile == Profile::Compact)
        .unwrap()
        .0
        .clone();
    store.credentials.get_mut(&old).unwrap().revoked = true;
    store.save(&f.auth).unwrap();
    let (status, _, _) = rpc(&f, Some(&f.a), "tools/list", json!({})).await;
    assert_eq!(status, 401);
    let owned = tool(&f, &new, "Tasks", json!({"task_id":id})).await;
    assert_eq!(owned["result"]["structuredContent"]["status"], "completed");
    store.credentials.get_mut(&key).unwrap().expires_at = Some(crate::auth::now());
    store.save(&f.auth).unwrap();
    let (status, _, _) = rpc(&f, Some(&new), "tools/list", json!({})).await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn origin_host_rate_and_native_image_contract() {
    use base64::Engine;
    let f = fixture(240);
    for (host, origin) in [
        ("evil.example", None),
        ("127.0.0.1", Some("https://evil.example")),
    ] {
        let mut req = Request::builder()
            .uri("/health")
            .header("host", host)
            .header("authorization", format!("Bearer {}", f.a));
        if let Some(o) = origin {
            req = req.header("origin", o);
        }
        assert_eq!(
            f.app
                .clone()
                .oneshot(req.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            403
        );
    }
    let png=base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jK0YAAAAASUVORK5CYII=").unwrap();
    std::fs::write(f.root.path().join("pixel.png"), png).unwrap();
    let image = tool(
        &f,
        &f.a,
        "Run",
        json!({"tool":"Image","arguments":{"path":"pixel.png"}}),
    )
    .await;
    assert_eq!(image["result"]["content"][1]["type"], "image");
    assert_eq!(image["result"]["content"][1]["mimeType"], "image/png");
    assert!(
        image["result"]["structuredContent"]
            .get("image_base64")
            .is_none()
    );
    let f = fixture(1);
    assert_eq!(rpc(&f, Some(&f.a), "tools/list", json!({})).await.0, 200);
    assert_eq!(rpc(&f, Some(&f.a), "tools/list", json!({})).await.0, 429);
}
