use crate::{
    auth::{Principal, Profile},
    operation::{DiscoverArgs, Operation, RunArgs},
};
use rmcp::model::Tool;
use schemars::JsonSchema;
use serde_json::{Value, json};
use std::sync::LazyLock;

pub fn typed_tool<T: JsonSchema>(
    name: &str,
    description: &str,
    read_only: bool,
    open_world: bool,
) -> Tool {
    let mut schema = serde_json::to_value(schemars::schema_for!(T)).expect("generated schema");
    schema.as_object_mut().unwrap().remove("$schema");
    schema.as_object_mut().unwrap().remove("title");
    serde_json::from_value(json!({"name":name,"description":description,"inputSchema":schema,
        "annotations":{"readOnlyHint":read_only,"destructiveHint":!read_only,"openWorldHint":open_world}})).expect("generated tool")
}
static TOOLS: LazyLock<Vec<Tool>> = LazyLock::new(Operation::definitions);
pub fn tools() -> Vec<Tool> {
    TOOLS.clone()
}
pub fn compact() -> Vec<Tool> {
    vec![
        typed_tool::<RunArgs>(
            "Run",
            "Execute an operation from the initial guide; inspect tool and arguments for approval.",
            false,
            true,
        ),
        typed_tool::<DiscoverArgs>(
            "Discover",
            "Get one exact operation schema, or a brief catalog. Initial instructions contain the signatures.",
            true,
            false,
        ),
    ]
}
pub fn exposed(p: &Principal) -> Vec<Tool> {
    if p.profile == Profile::Compact {
        compact()
    } else {
        tools().into_iter().filter(|t| p.allows(&t.name)).collect()
    }
}
pub fn validate(name: &str, args: &Value) -> anyhow::Result<()> {
    match name {
        "Run" => {
            serde_json::from_value::<RunArgs>(args.clone())?;
        }
        "Discover" => {
            serde_json::from_value::<DiscoverArgs>(args.clone())?;
        }
        _ => {
            Operation::decode(name, args.clone())?;
        }
    }
    Ok(())
}
pub fn guide(p: &Principal, cwd: &std::path::Path) -> String {
    let mut out = format!(
        "Reach {}: user's local {}, default cwd={}, identity={}.\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        cwd.display(),
        p.id
    );
    out.push_str("The client owns approval. Accepted calls execute directly. Files enforce workspace roots; commands and terminals have the OS account's execution rights. cwd is per call. Shell is a fresh process with separate stdout/stderr; persistent terminals are different resources. HTTP connections carry no session state.\n");
    if p.profile == Profile::Compact {
        out.push_str("Call Run({tool:NAME,arguments:{...}}). Discover({tool:NAME}) gives exact schema when needed.\n");
    }
    for t in TOOLS.iter().filter(|t| p.allows(&t.name)) {
        let required = t.input_schema.get("required").and_then(Value::as_array);
        let fields = t
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(name, schema)| {
                let optional = !required.is_some_and(|r| r.iter().any(|v| v == name));
                let kind = schema
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        if let Some(values) = schema.get("enum") {
                            values.to_string()
                        } else {
                            "value".into()
                        }
                    });
                format!("{name}{}:{kind}", if optional { "?" } else { "" })
            })
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&format!(
            "{}({fields}): {}\n",
            t.name,
            t.description.as_deref().unwrap_or_default()
        ));
    }
    out.push_str("Read before editing: replace, append, edit and patch of existing files require SHA-256 versions. Keep returned task_id to recover results. A caller-generated operation_id makes submission retries safe; without one, a lost submission response is ambiguous: inspect Tasks before resubmitting. Unfinished tasks become interrupted after restart and are never replayed. Ownership is principal identity, not HTTP connection.\n");
    out
}
