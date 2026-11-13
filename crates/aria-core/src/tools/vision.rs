//! `view_image`: send an image to the OpenAI-compatible vision model.
//!
//! No local OCR: the file bytes are base64-encoded into a data URL and the
//! model answers about them. Config arrives on [`ToolContext::vision`]; absent
//! config means the tool refuses, mirroring `search`.

use std::time::Duration;

use base64::Engine;
use serde::Deserialize;

use super::{Result, ToolContext, ToolError};

#[derive(Debug, Clone)]
pub struct VisionConfig {
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ViewImageArgs {
    pub path: String,
    #[serde(default)]
    pub question: Option<String>,
}

const DEFAULT_QUESTION: &str = "Describe this image in detail, including all \
                                visible text, UI elements, charts, diagrams, and layout.";

pub async fn view_image(ctx: &ToolContext, args: ViewImageArgs) -> Result<String> {
    let cfg = ctx
        .vision
        .as_ref()
        .ok_or_else(|| ToolError::Refused("vision is not configured".into()))?;
    let resolved = ctx.scope.resolve_read(&args.path)?;
    if resolved.is_dir() {
        return Err(ToolError::Args(format!("{} is a directory", args.path)));
    }
    let bytes = std::fs::read(&resolved)?;
    let mime = detect_mime(&args.path, &bytes).ok_or_else(|| {
        ToolError::Args(format!(
            "{} is not a supported image (png, jpeg, webp, gif)",
            args.path
        ))
    })?;
    let data_url = format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    );
    let body = serde_json::json!({
        "model": cfg.model,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": args.question.as_deref().unwrap_or(DEFAULT_QUESTION) },
                { "type": "image_url", "image_url": { "url": data_url } }
            ]
        }],
        "max_tokens": 1000
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| ToolError::Refused(e.to_string()))?;
    let mut req = client
        .post(format!(
            "{}/chat/completions",
            cfg.base_url.trim_end_matches('/')
        ))
        .json(&body);
    if let Some(key) = cfg.api_key.as_deref().filter(|k| !k.is_empty()) {
        req = req.bearer_auth(key);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| ToolError::Refused(format!("vision request failed: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(ToolError::Refused(format!("vision API error {status}: {body}")));
    }
    let value: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| ToolError::Refused(format!("bad vision response: {e}")))?;
    extract_text(&value)
        .ok_or_else(|| ToolError::Refused("vision response had no output text".into()))
}

/// Magic bytes first (an extension can lie), then the extension.
fn detect_mime(path: &str, bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

/// `choices[0].message.content` as a string, or joined text parts (some
/// OpenAI-compatible servers return an array of content parts).
fn extract_text(value: &serde_json::Value) -> Option<String> {
    let content = value
        .get("choices")?
        .as_array()?
        .first()?
        .get("message")?
        .get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let mut out = String::new();
    for part in content.as_array()? {
        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
            out.push_str(t);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::detect_mime;

    #[test]
    fn mime_from_magic_bytes() {
        assert_eq!(
            detect_mime("x.bin", &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0]),
            Some("image/png")
        );
        assert_eq!(detect_mime("x.bin", &[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(detect_mime("x.bin", b"GIF89a.."), Some("image/gif"));
        let mut webp = b"RIFF\x00\x00\x00\x00WEBPVP8 ".to_vec();
        webp.truncate(16);
        assert_eq!(detect_mime("x.bin", &webp), Some("image/webp"));
    }

    #[test]
    fn mime_falls_back_to_extension_and_rejects_others() {
        assert_eq!(detect_mime("shot.PNG", b"not a real png"), Some("image/png"));
        assert_eq!(detect_mime("doc.pdf", b"%PDF-1.4"), None);
        assert_eq!(detect_mime("noext", b"data"), None);
    }
}
