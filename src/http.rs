use crate::{
    auth::{Principal, Profile, Store},
    catalog,
    engine::Hand,
};
use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::*,
    service::RequestContext,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::{Value, json};
use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

fn principal(ctx: &RequestContext<RoleServer>) -> Result<Principal, ErrorData> {
    ctx.extensions
        .get::<axum::http::request::Parts>()
        .and_then(|p| p.extensions.get::<Principal>())
        .cloned()
        .ok_or_else(|| ErrorData::internal_error("Missing authenticated request identity", None))
}
impl ServerHandler for Hand {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("reach", env!("CARGO_PKG_VERSION")));
        info.protocol_version = ProtocolVersion::V_2025_11_25;
        info
    }
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[
            ProtocolVersion::V_2025_03_26,
            ProtocolVersion::V_2025_06_18,
            ProtocolVersion::V_2025_11_25,
        ])
    }
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        let p = principal(&ctx)?;
        ctx.peer.set_peer_info(request.clone());
        let mut info = self.negotiate_initialize(&request)?;
        info.instructions = Some(catalog::guide(
            &p,
            p.roots.first().unwrap_or(&self.config.default_cwd),
        ));
        Ok(info)
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: catalog::exposed(&principal(&ctx)?),
            ..Default::default()
        })
    }
    fn get_tool(&self, name: &str) -> Option<Tool> {
        catalog::tools()
            .into_iter()
            .chain(catalog::compact())
            .find(|t| t.name == name)
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let p = principal(&ctx)?;
        let started = Instant::now();
        let name = request.name.to_string();
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        if !catalog::exposed(&p).iter().any(|t| t.name == name) {
            return Err(ErrorData::invalid_params(
                "Tool is not exposed for this credential",
                None,
            ));
        }
        let mut audit_operation = name.clone();
        let outcome: crate::error::Result<Value> = async {
            if p.profile==Profile::Compact && name=="Discover" {
                let args:crate::operation::DiscoverArgs=serde_json::from_value(arguments.clone()).map_err(|e|crate::error::Failure::invalid(e.to_string()))?;
                let allowed:Vec<Tool>=catalog::tools().into_iter().filter(|t|p.allows(&t.name)).collect();
                return if let Some(name)=args.tool {
                    allowed.into_iter().find(|t|t.name==name).map(|t|serde_json::to_value(t).expect("tool"))
                        .ok_or_else(||crate::error::Failure::invalid("Unknown or unavailable operation"))
                } else {Ok(json!({"operations":allowed.iter().map(|t|json!({"name":t.name,"description":t.description})).collect::<Vec<_>>()}))};
            }
            let (operation,args)=if p.profile==Profile::Compact {
                let args:crate::operation::RunArgs=serde_json::from_value(arguments).map_err(|e|crate::error::Failure::invalid(e.to_string()))?;
                (args.tool,args.arguments)
            } else {(name.clone(),arguments)};
            audit_operation=operation.clone();
            self.execute_for(&p,&operation,args).await
        }.await;
        let (mut value, error) = match outcome {
            Ok(value) => {
                let failed = !matches!(audit_operation.as_str(), "Tasks" | "Poll" | "Cancel")
                    && value
                        .get("status")
                        .and_then(Value::as_str)
                        .is_some_and(|s| {
                            matches!(
                                s,
                                "failed"
                                    | "timed_out"
                                    | "cancelled"
                                    | "interrupted"
                                    | "unknown"
                                    | "expired"
                            )
                        });
                (value, failed)
            }
            Err(error) => (json!({"kind":"rejected","error":error}), true),
        };
        let operation = audit_operation;
        if let Err(e) = self.audit.record(&p, &operation, started, !error) {
            eprintln!("Audit write failed: {e}");
        }
        let mut blocks = Vec::new();
        if let Some(data) = value.as_object_mut().and_then(|v| v.remove("image_base64")) {
            blocks.push(
                serde_json::from_value(
                    json!({"type":"image","data":data,"mimeType":value["mime_type"]}),
                )
                .map_err(|_| ErrorData::internal_error("Image encoding failed", None))?,
            );
        }
        blocks.insert(0, ContentBlock::text(value.to_string()));
        let mut result = CallToolResult::success(blocks);
        result.structured_content = Some(value);
        result.is_error = Some(error);
        Ok(result.into())
    }
}
#[derive(Clone)]
struct Guard {
    config: Arc<crate::Config>,
    limits: Arc<Mutex<HashMap<String, (Instant, u32)>>>,
}
fn reject(status: StatusCode, message: &str) -> Response {
    let mut response = (status, axum::Json(json!({"error":message}))).into_response();
    if status == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer realm=\"reach\""),
        );
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, header::HeaderValue::from_static("60"));
    }
    response
}
async fn authenticate(State(guard): State<Guard>, mut request: Request, next: Next) -> Response {
    if request.headers().contains_key(header::ORIGIN) {
        return reject(StatusCode::FORBIDDEN, "Browser origins are not allowed");
    }
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<axum::http::uri::Authority>().ok());
    if !host.as_ref().is_some_and(|h| {
        ["localhost", "127.0.0.1", "::1"].contains(&h.host().trim_matches(['[', ']']))
    }) {
        return reject(StatusCode::FORBIDDEN, "Invalid local host");
    }
    let store = match Store::load(&guard.config.auth_file) {
        Ok(s) => s,
        Err(_) => {
            return reject(
                StatusCode::SERVICE_UNAVAILABLE,
                "Credential store unavailable",
            );
        }
    };
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let p = match token.and_then(|t| store.authenticate(t)) {
        Some(p) => p,
        None => return reject(StatusCode::UNAUTHORIZED, "Unauthorized"),
    };
    {
        let mut limits = guard.limits.lock().expect("rate lock");
        limits.retain(|_, (t, _)| t.elapsed().as_secs() < 60);
        let entry = limits
            .entry(p.credential_id.clone())
            .or_insert((Instant::now(), 0));
        if entry.1 >= guard.config.requests_per_minute {
            return reject(StatusCode::TOO_MANY_REQUESTS, "Request rate exceeded");
        }
        entry.1 += 1;
    }
    request.extensions_mut().insert(p);
    next.run(request).await
}
pub fn router(hand: Hand, stop: CancellationToken) -> Router {
    let guard = Guard {
        config: hand.config.clone(),
        limits: Arc::new(Mutex::new(HashMap::new())),
    };
    let service: StreamableHttpService<Hand, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(hand.clone()),
        Default::default(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_sse_keep_alive(None)
            .with_max_request_body_bytes(16 * 1024 * 1024)
            .with_cancellation_token(stop)
            .with_allowed_hosts(["localhost", "127.0.0.1", "::1"]),
    );
    Router::new().nest_service("/mcp",service).route("/health",get(||async {axum::Json(json!({"status":"ok","server":"reach","version":env!("CARGO_PKG_VERSION"),"stateless":true}))})).layer(middleware::from_fn_with_state(guard,authenticate))
}
