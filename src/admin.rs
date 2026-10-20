//! Optional local-only management. Browser login tickets are issued by the local
//! OS user through the CLI; MCP credentials never authorize this listener.
use crate::{
    auth::{self, Profile, Store},
    config::AdminConfig,
    engine::Hand,
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Default, Serialize, Deserialize)]
struct Tickets {
    tickets: Vec<Ticket>,
}
#[derive(Serialize, Deserialize)]
struct Ticket {
    sha256: String,
    expires: u64,
}
fn ticket_lock(path: &std::path::Path) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))?;
    fs2::FileExt::try_lock_exclusive(&file).context("Login ticket store is busy")?;
    Ok(file)
}
fn read_tickets(path: &std::path::Path) -> Result<Tickets> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Tickets::default()),
        Err(e) => Err(e.into()),
    }
}
pub fn prepare(path: &std::path::Path) -> Result<()> {
    save_tickets(path, &Tickets::default())
}
fn save_tickets(path: &std::path::Path, tickets: &Tickets) -> Result<()> {
    let mut temp =
        tempfile::NamedTempFile::new_in(path.parent().context("Ticket parent missing")?)?;
    temp.write_all(&serde_json::to_vec(tickets)?)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
pub fn login_url(admin: &AdminConfig) -> Result<String> {
    let _lock = ticket_lock(&admin.tickets_file)?;
    let mut tickets = read_tickets(&admin.tickets_file)?;
    tickets.tickets.retain(|t| t.expires > auth::now());
    ensure!(tickets.tickets.len() < 32, "Too many unused login tickets");
    let secret = auth::secret()?;
    tickets.tickets.push(Ticket {
        sha256: crate::files::hash(secret.as_bytes()),
        expires: auth::now() + 180,
    });
    save_tickets(&admin.tickets_file, &tickets)?;
    Ok(format!("http://{}/#ticket={secret}", admin.listen))
}
struct Session {
    csrf: String,
    expires: u64,
    last: u64,
}
#[derive(Clone)]
struct Admin {
    hand: Hand,
    config: AdminConfig,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
}
type ApiResult = std::result::Result<Json<Value>, (StatusCode, Json<Value>)>;
fn fail(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error":message})))
}
fn internal(error: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    eprintln!("Management operation failed: {error}");
    fail(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Local operation failed; inspect executor logs",
    )
}
fn cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|p| p.trim().strip_prefix("reach_admin="))
}
async fn guard(State(admin): State<Admin>, mut request: Request, next: Next) -> Response {
    let expected = admin.config.listen.to_string();
    if request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        != Some(expected.as_str())
    {
        return fail(StatusCode::FORBIDDEN, "Unexpected management host").into_response();
    }
    if let Some(origin) = request.headers().get(header::ORIGIN)
        && origin.to_str().ok() != Some(format!("http://{expected}").as_str())
    {
        return fail(
            StatusCode::FORBIDDEN,
            "Cross-origin management is forbidden",
        )
        .into_response();
    }
    if request.uri().path().starts_with("/api/") {
        let write = request.method() != axum::http::Method::GET;
        if write
            && request
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok())
                != Some("application/json")
        {
            return fail(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Use application/json")
                .into_response();
        }
        if request.uri().path() != "/api/login" {
            let key = cookie(request.headers()).map(|c| crate::files::hash(c.as_bytes()));
            let mut sessions = admin.sessions.lock().expect("admin sessions");
            let now = auth::now();
            sessions.retain(|_, s| s.expires > now && s.last + 1800 > now);
            let Some(session) = key.as_ref().and_then(|key| sessions.get_mut(key)) else {
                return fail(
                    StatusCode::UNAUTHORIZED,
                    "Run reach ui on this machine to sign in",
                )
                .into_response();
            };
            if write
                && request
                    .headers()
                    .get("x-reach-csrf")
                    .and_then(|h| h.to_str().ok())
                    != Some(session.csrf.as_str())
            {
                return fail(StatusCode::FORBIDDEN, "Invalid CSRF token").into_response();
            }
            session.last = now;
            request.extensions_mut().insert(session.csrf.clone());
        }
    }
    let mut response = next.run(request).await;
    for (name, value) in [
        (
            "content-security-policy",
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
        ("x-frame-options", "DENY"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("cache-control", "no-store"),
    ] {
        response.headers_mut().insert(
            header::HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    response
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Login {
    ticket: String,
}
async fn login(
    State(admin): State<Admin>,
    Json(input): Json<Login>,
) -> std::result::Result<Response, (StatusCode, Json<Value>)> {
    if input.ticket.len() != 64 {
        return Err(fail(
            StatusCode::UNAUTHORIZED,
            "Login ticket expired or invalid",
        ));
    }
    let _lock = ticket_lock(&admin.config.tickets_file).map_err(internal)?;
    let mut tickets = read_tickets(&admin.config.tickets_file).map_err(internal)?;
    let digest = crate::files::hash(input.ticket.as_bytes());
    let now = auth::now();
    let before = tickets.tickets.len();
    let valid = tickets
        .tickets
        .iter()
        .any(|t| t.expires > now && t.sha256 == digest);
    tickets
        .tickets
        .retain(|t| t.expires > now && t.sha256 != digest);
    if before != tickets.tickets.len() {
        save_tickets(&admin.config.tickets_file, &tickets).map_err(internal)?;
    }
    if !valid {
        return Err(fail(
            StatusCode::UNAUTHORIZED,
            "Login ticket expired or already used",
        ));
    }
    let token = auth::secret().map_err(internal)?;
    let csrf = auth::secret().map_err(internal)?;
    {
        let mut sessions = admin.sessions.lock().expect("admin sessions");
        sessions.retain(|_, s| s.expires > now && s.last + 1800 > now);
        if sessions.len() >= 32 {
            return Err(fail(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many management sessions",
            ));
        }
        sessions.insert(
            crate::files::hash(token.as_bytes()),
            Session {
                csrf: csrf.clone(),
                expires: now + 8 * 3600,
                last: now,
            },
        );
    }
    let mut response = Json(json!({"csrf":csrf})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "reach_admin={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=28800"
        ))
        .map_err(internal)?,
    );
    Ok(response)
}
async fn state(State(admin): State<Admin>, request: Request) -> ApiResult {
    let store = Store::load(&admin.hand.config.auth_file).map_err(internal)?;
    let clients:Vec<_>=store.credentials.iter().map(|(id,c)|json!({"id":id,"principal":c.principal,"label":c.label,"scopes":c.scopes,"roots":c.roots,"profile":c.profile,"created_at":c.created_at,"expires_at":c.expires_at,"revoked":c.revoked})).collect();
    Ok(Json(
        json!({"name":"Reach","version":env!("CARGO_PKG_VERSION"),"os":std::env::consts::OS,"default_cwd":admin.hand.config.default_cwd,"local_mcp":format!("http://{}/mcp",admin.hand.config.listen),"ssh_mcp":admin.hand.config.ssh.as_ref().map(|s|format!("http://127.0.0.1:{}/mcp",s.remote_port)),"active_tasks":admin.hand.jobs.active_count(),"clients":clients,"tasks":admin.hand.jobs.summaries(12),"csrf":request.extensions().get::<String>()}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Issue {
    label: String,
    scopes: Vec<String>,
    profile: Profile,
    #[serde(default)]
    roots: Vec<PathBuf>,
    ttl_hours: u64,
    #[serde(default)]
    remote: bool,
}
fn endpoint(admin: &Admin, remote: bool) -> String {
    if remote {
        admin
            .hand
            .config
            .ssh
            .as_ref()
            .map(|s| format!("http://127.0.0.1:{}/mcp", s.remote_port))
            .unwrap_or_else(|| format!("http://{}/mcp", admin.hand.config.listen))
    } else {
        format!("http://{}/mcp", admin.hand.config.listen)
    }
}
fn issued(id: String, token: String, url: String) -> Json<Value> {
    Json(
        json!({"id":id,"token":token,"config":{"mcpServers":{"reach":{"type":"http","url":url,"headers":{"Authorization":format!("Bearer {token}")}}}}}),
    )
}
async fn issue(State(admin): State<Admin>, Json(input): Json<Issue>) -> ApiResult {
    if input.label.trim().is_empty()
        || input.label.len() > 100
        || input.ttl_hours == 0
        || input.ttl_hours > 87600
    {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "Use a name and a lifetime between 1 hour and 10 years",
        ));
    }
    let roots = if input.roots.is_empty() {
        vec![admin.hand.config.default_cwd.clone()]
    } else {
        input.roots
    };
    for root in &roots {
        if !root.is_absolute() || !root.is_dir() {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "Workspace roots must be existing absolute directories",
            ));
        }
    }
    let _lock = ticket_lock(&admin.hand.config.auth_file).map_err(internal)?;
    let mut store = Store::load(&admin.hand.config.auth_file).map_err(internal)?;
    let principal = format!("client-{}", uuid::Uuid::new_v4().simple());
    let (id, token) = store
        .issue(
            &principal,
            input.label.trim(),
            input.scopes,
            input.profile,
            Some(input.ttl_hours * 3600),
        )
        .map_err(|_| fail(StatusCode::BAD_REQUEST, "Invalid credential permissions"))?;
    store
        .credentials
        .get_mut(&id)
        .expect("issued credential")
        .roots = roots;
    store.save(&admin.hand.config.auth_file).map_err(internal)?;
    audit(&admin, "credential.issue", &id);
    Ok(issued(id, token, endpoint(&admin, input.remote)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rotate {
    #[serde(default)]
    remote: bool,
}
async fn rotate(
    State(admin): State<Admin>,
    Path(id): Path<String>,
    Json(input): Json<Rotate>,
) -> ApiResult {
    let _lock = ticket_lock(&admin.hand.config.auth_file).map_err(internal)?;
    let mut store = Store::load(&admin.hand.config.auth_file).map_err(internal)?;
    let old = store
        .credentials
        .get(&id)
        .cloned()
        .filter(|c| !c.revoked)
        .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                "Credential not found or already revoked",
            )
        })?;
    let ttl = old
        .expires_at
        .and_then(|t| t.checked_sub(auth::now()))
        .filter(|t| *t > 0)
        .unwrap_or(30 * 86400);
    let (new, token) = store
        .issue(
            &old.principal,
            &old.label,
            old.scopes,
            old.profile,
            Some(ttl),
        )
        .map_err(internal)?;
    store
        .credentials
        .get_mut(&new)
        .expect("new credential")
        .roots = old.roots;
    store
        .credentials
        .get_mut(&id)
        .expect("old credential")
        .revoked = true;
    store.save(&admin.hand.config.auth_file).map_err(internal)?;
    audit(&admin, "credential.rotate", &id);
    Ok(issued(new, token, endpoint(&admin, input.remote)))
}
async fn revoke(State(admin): State<Admin>, Path(id): Path<String>) -> ApiResult {
    let _lock = ticket_lock(&admin.hand.config.auth_file).map_err(internal)?;
    let mut store = Store::load(&admin.hand.config.auth_file).map_err(internal)?;
    store
        .credentials
        .get_mut(&id)
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "Credential not found"))?
        .revoked = true;
    store.save(&admin.hand.config.auth_file).map_err(internal)?;
    audit(&admin, "credential.revoke", &id);
    Ok(Json(json!({"ok":true})))
}
async fn cancel(State(admin): State<Admin>, Path(id): Path<String>) -> ApiResult {
    let id =
        uuid::Uuid::parse_str(&id).map_err(|_| fail(StatusCode::BAD_REQUEST, "Invalid task ID"))?;
    let task = admin
        .hand
        .jobs
        .get(id)
        .map_err(|_| fail(StatusCode::NOT_FOUND, "Task not found"))?;
    admin.hand.jobs.cancel(&task).map_err(|_| {
        fail(
            StatusCode::BAD_REQUEST,
            "Only process tasks can be cancelled",
        )
    })?;
    audit(&admin, "task.cancel", &id.to_string());
    Ok(Json(admin.hand.jobs.report(&task, 0).await))
}
async fn logout(State(admin): State<Admin>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie(&headers) {
        admin
            .sessions
            .lock()
            .expect("admin sessions")
            .remove(&crate::files::hash(token.as_bytes()));
    }
    let mut response = Json(json!({"ok":true})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("reach_admin=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
    );
    response
}
pub fn router(hand: Hand, config: AdminConfig) -> Router {
    let admin = Admin {
        hand,
        config,
        sessions: Arc::new(Mutex::new(HashMap::new())),
    };
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../web/index.html")) }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/app.js"),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../web/style.css"),
                )
            }),
        )
        .route("/api/login", post(login))
        .route("/api/state", get(state))
        .route("/api/credentials", post(issue))
        .route("/api/credentials/{id}/rotate", post(rotate))
        .route("/api/credentials/{id}/revoke", post(revoke))
        .route("/api/tasks/{id}/cancel", post(cancel))
        .route("/api/logout", post(logout))
        .layer(DefaultBodyLimit::max(8192))
        .layer(middleware::from_fn_with_state(admin.clone(), guard))
        .with_state(admin)
}

fn audit(admin: &Admin, action: &str, target: &str) {
    if let Err(e) = admin.hand.audit.event(action, target) {
        eprintln!("Management audit failed: {e}");
    }
}
