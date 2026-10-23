use crate::{
    error::{Failure, Result},
    operation::ImageArgs,
    policy::Policy,
};
use base64::Engine;
use serde::Serialize;
use std::{
    io::Read,
    path::{Path, PathBuf},
};
#[derive(Debug, Serialize)]
pub struct Image {
    pub path: PathBuf,
    pub mime_type: &'static str,
    pub bytes: usize,
    pub sha256: String,
    #[serde(skip)]
    pub data: String,
}
pub fn load(cwd: &Path, args: &ImageArgs, policy: &Policy) -> Result<Image> {
    let path = crate::files::resolve(cwd, &args.path);
    policy.check(&path)?;
    let mut bytes = Vec::new();
    std::fs::File::open(&path)?
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(Failure::invalid("Image exceeds 8 MiB"));
    }
    let mime_type = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        return Err(Failure::invalid(
            "Unsupported image signature; use PNG, JPEG, GIF or WebP",
        ));
    };
    Ok(Image {
        path,
        mime_type,
        bytes: bytes.len(),
        sha256: crate::files::hash(&bytes),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}
