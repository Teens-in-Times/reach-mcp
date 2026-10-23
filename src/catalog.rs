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
        let schema = Value::Object((*t.input_schema).clone());
        let fields = fields(&schema, &schema, 0);
        out.push_str(&format!(
            "{}({fields}): {}\n",
            t.name,
            t.description.as_deref().unwrap_or_default()
        ));
    }
    out.push_str("Poll waits for new bytes or task completion, whichever comes first; Tasks waits for a terminal task state. stdout_next_offset/stderr_next_offset continue after command previews. Recover inspects ended file tasks; restoration requires explicit current versions and creates a new task. Payloads may expire while retry identities remain. Read before editing: replace, append, edit and patch of existing files require SHA-256 versions. Keep returned task_id to recover results. A caller-generated operation_id makes submission retries safe; without one, a lost submission response is ambiguous: inspect Tasks before resubmitting. Unfinished tasks become interrupted after restart and are never replayed. Ownership is principal identity, not HTTP connection.\n");
    out
}

fn fields(schema: &Value, root: &Value, depth: usize) -> String {
    let required = schema.get("required").and_then(Value::as_array);
    schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(name, s)| {
            let optional = !required.is_some_and(|r| r.iter().any(|v| v == name));
            format!(
                "{name}{}:{}",
                if optional { "?" } else { "" },
                syntax(s, root, depth + 1)
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}
fn syntax(s: &Value, root: &Value, depth: usize) -> String {
    if depth > 8 {
        return "value".into();
    }
    if let Some(reference) = s
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix('#'))
        .and_then(|r| root.pointer(r))
    {
        return syntax(reference, root, depth + 1);
    }
    if let Some(v) = s.get("const") {
        return v.to_string();
    }
    if let Some(values) = s.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("|");
    }
    if let Some(variants) = s
        .get("oneOf")
        .or_else(|| s.get("anyOf"))
        .and_then(Value::as_array)
    {
        return variants
            .iter()
            .filter(|v| v.get("type").and_then(Value::as_str) != Some("null"))
            .map(|v| syntax(v, root, depth + 1))
            .collect::<Vec<_>>()
            .join("|");
    }
    let kind = s
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| {
            s.get("type")
                .and_then(Value::as_array)
                .and_then(|v| v.iter().filter_map(Value::as_str).find(|s| *s != "null"))
        })
        .unwrap_or("value");
    match kind {
        "object" => {
            let fields = fields(s, root, depth);
            if fields.is_empty()
                && let Some(props) = s.get("additionalProperties")
                && props != false
            {
                return format!("{{key:{}}}", syntax(props, root, depth + 1));
            }
            format!("{{{fields}}}")
        }
        "array" => format!(
            "{}[]",
            s.get("items")
                .map(|v| syntax(v, root, depth + 1))
                .unwrap_or_else(|| "value".into())
        ),
        "string" if s.get("format").and_then(Value::as_str) == Some("uuid") => "uuid".into(),
        "integer" if s.get("minimum").is_some() && s.get("maximum").is_some() => {
            format!("integer[{}..{}]", s["minimum"], s["maximum"])
        }
        _ => kind.into(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_guide_resolves_nested_sum_types_from_the_actual_schema() {
        let p = Principal::full("test");
        let guide = guide(&p, std::path::Path::new("."));
        assert!(guide.contains("mode:\"create\""));
        assert!(guide.contains("expected_sha256:string"));
        assert!(guide.contains("wait_seconds?:integer[0..30]"));
        assert!(guide.contains("key:\"enter\""));
        let missing = crate::operation::Operation::decode(
            "Write",
            json!({"path":"a","update":{"mode":"replace","content":"x"}}),
        );
        assert!(missing.is_err());
    }
}
