use crate::{
    Config,
    audit::Audit,
    auth::{Profile, Store},
    config::AdminConfig,
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
use tower::ServiceExt;

struct Fixture {
    _root: tempfile::TempDir,
    config: Config,
    admin: AdminConfig,
    hand: Hand,
    app: Router,
    mcp_token: String,
}
fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let private = project.join(".private");
    std::fs::create_dir_all(&private).unwrap();
    let auth = private.join("credentials.json");
    let mut store = Store::default();
    let (_, token) = store
        .issue(
            "agent",
            "Agent",
            vec!["read".into()],
            Profile::Compact,
            Some(600),
        )
        .unwrap();
    store.save(&auth).unwrap();
    std::fs::write(private.join("secret.txt"), "NEVER_LEAK_PRIVATE_MARKER").unwrap();
    std::fs::write(project.join("hello.txt"), "hello").unwrap();
    let admin = AdminConfig {
        listen: "127.0.0.1:42374".parse().unwrap(),
        tickets_file: private.join("login-tickets.json"),
    };
    let config = Config {
        listen: "127.0.0.1:42373".parse().unwrap(),
        auth_file: auth,
        default_cwd: project,
        state_dir: private.join("state"),
        ssh: None,
        requests_per_minute: 240,
        admin: Some(admin.clone()),
        protected_paths: vec![private],
    };
    let hand = Hand {
        config: Arc::new(config.clone()),
        jobs: Arc::new(Jobs::open(&config.state_dir).unwrap()),
        audit: Arc::new(Audit::open(&config.state_dir).unwrap()),
    };
    let app = crate::admin::router(hand.clone(), admin.clone());
    Fixture {
        _root: root,
        config,
        admin,
        hand,
        app,
        mcp_token: token,
    }
}
async fn request(
    f: &Fixture,
    path: &str,
    body: Option<Value>,
    cookie: Option<&str>,
    csrf: Option<&str>,
    origin: Option<&str>,
) -> (StatusCode, Value, HeaderMap) {
    let mut req = Request::builder()
        .uri(path)
        .header("host", f.admin.listen.to_string());
    if let Some(c) = cookie {
        req = req.header("cookie", c);
    }
    if let Some(c) = csrf {
        req = req.header("x-reach-csrf", c);
    }
    if let Some(o) = origin {
        req = req.header("origin", o);
    }
    let body = if let Some(body) = body {
        req = req
            .method("POST")
            .header("content-type", "application/json");
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    let response = f
        .app
        .clone()
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or_default(),
        headers,
    )
}
async fn login(f: &Fixture) -> (String, String) {
    let url = crate::admin::login_url(&f.admin).unwrap();
    let ticket = url.split("#ticket=").nth(1).unwrap();
    let (s, b, h) = request(
        f,
        "/api/login",
        Some(json!({"ticket":ticket})),
        None,
        None,
        Some("http://127.0.0.1:42374"),
    )
    .await;
    assert_eq!(s, 200);
    let set = h.get("set-cookie").unwrap().to_str().unwrap();
    assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict"));
    (
        set.split(';').next().unwrap().to_string(),
        b["csrf"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn local_ticket_is_one_use_and_agent_key_is_not_admin() {
    let f = fixture();
    assert_eq!(
        request(&f, "/api/state", None, None, None, None).await.0,
        401
    );
    assert_eq!(
        request(
            &f,
            "/api/state",
            None,
            Some(&format!("reach_admin={}", f.mcp_token)),
            None,
            None
        )
        .await
        .0,
        401
    );
    let url = crate::admin::login_url(&f.admin).unwrap();
    let ticket = url.split("#ticket=").nth(1).unwrap();
    let body = Some(json!({"ticket":ticket}));
    assert_eq!(
        request(
            &f,
            "/api/login",
            body.clone(),
            None,
            None,
            Some("https://evil.example")
        )
        .await
        .0,
        403
    );
    assert_eq!(
        request(&f, "/api/login", body.clone(), None, None, None)
            .await
            .0,
        200
    );
    assert_eq!(
        request(&f, "/api/login", body, None, None, None).await.0,
        401
    );
    let (cookie, csrf) = login(&f).await;
    let (_, state, headers) = request(&f, "/api/state", None, Some(&cookie), None, None).await;
    assert_eq!(state["name"], "Reach");
    assert!(!state.to_string().contains("sha256"));
    assert!(!state.to_string().contains(&f.mcp_token));
    assert!(headers.contains_key("content-security-policy"));
    assert_eq!(
        request(
            &f,
            "/api/credentials",
            Some(json!({})),
            Some(&cookie),
            None,
            None
        )
        .await
        .0,
        403
    );
    assert_eq!(
        request(
            &f,
            "/api/logout",
            Some(json!({})),
            Some(&cookie),
            Some(&csrf),
            None
        )
        .await
        .0,
        200
    );
    assert_eq!(
        request(&f, "/api/state", None, Some(&cookie), None, None)
            .await
            .0,
        401
    );
}

#[tokio::test]
async fn management_issues_rotates_and_revokes_without_restarting() {
    let f = fixture();
    let (cookie, csrf) = login(&f).await;
    let(s,created,_)=request(&f,"/api/credentials",Some(json!({"label":"New client","scopes":["read","tasks"],"profile":"compact","roots":[f.config.default_cwd],"ttl_hours":24,"remote":false})),Some(&cookie),Some(&csrf),None).await;
    assert_eq!(s, 200);
    let token = created["token"].as_str().unwrap();
    let id = created["id"].as_str().unwrap();
    let before = Store::load(&f.config.auth_file)
        .unwrap()
        .authenticate(token)
        .unwrap();
    assert!(!before.allows("Shell"));
    assert_eq!(before.roots, vec![f.config.default_cwd.clone()]);
    assert_eq!(
        created["config"]["mcpServers"]["reach"]["headers"]["Authorization"],
        format!("Bearer {token}")
    );
    let (s, rotated, _) = request(
        &f,
        &format!("/api/credentials/{id}/rotate"),
        Some(json!({})),
        Some(&cookie),
        Some(&csrf),
        None,
    )
    .await;
    assert_eq!(s, 200);
    let store = Store::load(&f.config.auth_file).unwrap();
    assert!(store.authenticate(token).is_none());
    let after = store
        .authenticate(rotated["token"].as_str().unwrap())
        .unwrap();
    assert_eq!(before.id, after.id);
    assert_eq!(before.roots, after.roots);
    assert_eq!(
        request(
            &f,
            &format!(
                "/api/credentials/{}/revoke",
                rotated["id"].as_str().unwrap()
            ),
            Some(json!({})),
            Some(&cookie),
            Some(&csrf),
            None
        )
        .await
        .0,
        200
    );
    assert!(
        Store::load(&f.config.auth_file)
            .unwrap()
            .authenticate(rotated["token"].as_str().unwrap())
            .is_none()
    );
}

#[tokio::test]
async fn file_capabilities_cannot_read_credentials_or_escape_workspace() {
    let f = fixture();
    let p = Store::load(&f.config.auth_file)
        .unwrap()
        .authenticate(&f.mcp_token)
        .unwrap();
    let read = f
        .hand
        .execute_for(
            &p,
            "Read",
            json!({"paths":["hello.txt",f.config.auth_file]}),
        )
        .await
        .unwrap();
    assert!(read["files"][0]["text"].as_str().unwrap().contains("hello"));
    assert!(read["files"][1]["error"].is_string());
    let found = f
        .hand
        .execute_for(&p, "Grep", json!({"pattern":"NEVER_LEAK_PRIVATE_MARKER"}))
        .await
        .unwrap();
    assert_eq!(found["matches"].as_array().unwrap().len(), 0);
    let listing = f
        .hand
        .execute_for(&p, "Read", json!({"paths":["."]}))
        .await
        .unwrap();
    assert!(!listing.to_string().contains(".private"));
    assert!(
        f.hand
            .execute_for(&p, "Write", json!({"path":"bad","content":"bad"}))
            .await
            .is_err()
    );
    let mut p = p;
    p.scopes.push("write".into());
    assert!(
        f.hand
            .execute_for(
                &p,
                "Write",
                json!({"path":f.config.auth_file,"content":"bad"})
            )
            .await
            .is_err()
    );
    assert!(
        f.hand
            .execute_for(
                &p,
                "ApplyPatch",
                json!({"patch":"*** Begin Patch\n*** Add File: .private/new\n+bad\n*** End Patch"})
            )
            .await
            .unwrap()["status"]
            == "failed"
    );
    assert!(!f.config.auth_file.parent().unwrap().join("new").exists());
}
