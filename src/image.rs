use anyhow::{Result, bail, ensure};
use base64::Engine;
use serde_json::{Value, json};
use std::path::Path;
pub fn load(cwd: &Path, args: &Value) -> Result<Value> {
    let path = crate::files::resolve(cwd, crate::files::text_arg(args, "path")?);
    ensure!(
        std::fs::metadata(&path)?.len() <= 8 * 1024 * 1024,
        "Image exceeds 8 MiB"
    );
    let bytes = std::fs::read(&path)?;
    ensure!(bytes.len() <= 8 * 1024 * 1024, "Image grew beyond 8 MiB");
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        bail!("Unsupported image signature; use PNG, JPEG, GIF or WebP")
    };
    Ok(
        json!({"path":path,"mime_type":mime,"bytes":bytes.len(),"sha256":crate::files::hash(&bytes),"image_base64":base64::engine::general_purpose::STANDARD.encode(&bytes)}),
    )
}
