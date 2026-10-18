//! Width-independent display content retained after an MCP call completes.
//!
//! The wire result can contain multi-megabyte image, audio, or resource bodies. Validate each
//! block once with the MCP model, then retain only what history rendering actually displays.

use crate::text_formatting::format_json_compact;
use base64::Engine;
use codex_protocol::mcp::CallToolResult;
use image::DynamicImage;
use image::ImageReader;
use rmcp::model::ContentBlock;
use rmcp::model::ResourceContents;
use serde::Deserialize;
use std::borrow::Cow;
use std::io::Cursor;
use tracing::error;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum McpResultKind {
    Standard,
    NodeRepl,
}

#[derive(Debug)]
pub(super) struct McpToolResult {
    pub(super) content: Vec<McpContentBlock>,
    pub(super) is_error: bool,
    pub(super) has_image: bool,
}

#[derive(Debug)]
pub(super) struct McpContentBlock {
    display: McpContentDisplay,
    pub(super) is_image: bool,
    /// Code mode uses a top-level `text` field even on malformed or non-text blocks.
    original_text: Option<crate::retained_text::RetainedText>,
}

#[derive(Debug)]
enum McpContentDisplay {
    Text(crate::retained_text::RetainedText),
    Summary(Cow<'static, str>),
    Json(crate::retained_text::RetainedText),
}

impl McpToolResult {
    /// Consumes a wire result, dropping bodies represented only by media or resource summaries.
    ///
    /// Canonical MCP deserialization preserves full text and the exact JSON fallback for malformed
    /// or unknown blocks. Every block is projected, but image decoding stops at the first fully
    /// valid image.
    pub(super) fn new(result: CallToolResult, kind: McpResultKind) -> Self {
        let mut has_image = false;
        let content = result
            .content
            .into_iter()
            .map(|block| {
                // Deserialize by reference so malformed blocks remain available for the exact
                // JSON fallback. Successful blocks no longer retain their wire representation.
                let parsed = ContentBlock::deserialize(&block);
                let is_image = matches!(&parsed, Ok(ContentBlock::Image(_)));
                let original_text = match (&parsed, kind) {
                    (Ok(ContentBlock::Text(_)), _) | (_, McpResultKind::Standard) => None,
                    (_, McpResultKind::NodeRepl) => block
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .map(|text| crate::retained_text::RetainedText::new(text.to_owned())),
                };
                let display = match parsed {
                    Ok(ContentBlock::Text(text)) => {
                        McpContentDisplay::Text(crate::retained_text::RetainedText::new(text.text))
                    }
                    Ok(ContentBlock::Image(image)) => {
                        // Computer activity previews require a valid image. Stop decoding once
                        // one has been found; the result text only retains a readable marker.
                        if !has_image {
                            has_image = decode_mcp_image(&image.data).is_some();
                        }
                        McpContentDisplay::Summary("Returned image".into())
                    }
                    Ok(ContentBlock::Audio(_)) => {
                        McpContentDisplay::Summary("<audio content>".into())
                    }
                    Ok(ContentBlock::Resource(resource)) => {
                        let summary = match resource.resource {
                            ResourceContents::TextResourceContents { uri, .. }
                            | ResourceContents::BlobResourceContents { uri, .. } => {
                                format!("embedded resource: {uri}").into()
                            }
                            _ => "<unknown embedded resource>".into(),
                        };
                        McpContentDisplay::Summary(summary)
                    }
                    Ok(ContentBlock::ResourceLink(link)) => {
                        McpContentDisplay::Summary(format!("link: {}", link.uri).into())
                    }
                    Ok(_) | Err(_) => McpContentDisplay::Json(
                        crate::retained_text::RetainedText::new(block.to_string()),
                    ),
                };
                McpContentBlock {
                    display,
                    is_image,
                    original_text,
                }
            })
            .collect();

        Self {
            content,
            is_error: result.is_error.unwrap_or(false),
            has_image,
        }
    }
}

impl McpContentBlock {
    /// Returns all retained text, with honest markers for media and embedded resource bodies.
    /// A bounded view for compact summaries; expanded transcripts use `render_full`.
    pub(super) fn preview_text(&self) -> &str {
        if let Some(text) = &self.original_text {
            return text.preview();
        }
        match &self.display {
            McpContentDisplay::Text(text) | McpContentDisplay::Json(text) => text.preview(),
            McpContentDisplay::Summary(summary) => summary,
        }
    }

    pub(super) fn render_full(&self) -> Cow<'_, str> {
        if let Some(text) = &self.original_text {
            return text.read();
        }
        match &self.display {
            McpContentDisplay::Text(text) | McpContentDisplay::Json(text) => text.read(),
            McpContentDisplay::Summary(summary) => summary.clone(),
        }
    }

    pub(super) fn text(&self) -> Option<Cow<'_, str>> {
        match &self.display {
            McpContentDisplay::Text(text) => Some(Cow::Borrowed(text.preview())),
            McpContentDisplay::Summary(_) | McpContentDisplay::Json(_) => self
                .original_text
                .as_ref()
                .map(|text| Cow::Borrowed(text.preview())),
        }
    }

    pub(super) fn render(&self) -> Cow<'_, str> {
        match &self.display {
            McpContentDisplay::Text(text) | McpContentDisplay::Json(text) => {
                let text = text.preview();
                format_json_compact(text)
                    .map(Cow::Owned)
                    .unwrap_or(Cow::Borrowed(text))
            }
            McpContentDisplay::Summary(summary) => summary.clone(),
        }
    }
}

/// Fully decodes an MCP image before exposing an image preview in computer activity.
///
/// A header-only check would accept images whose decoder rejects their pixel data. Preserve the
/// existing behavior for invalid base64, unknown formats, corrupt images, and data URLs.
fn decode_mcp_image(data: &str) -> Option<DynamicImage> {
    let base64_data = if let Some(data_url) = data.strip_prefix("data:") {
        data_url.split_once(',')?.1
    } else {
        data
    };
    let raw_data = base64::engine::general_purpose::STANDARD
        .decode(base64_data)
        .map_err(|e| {
            error!("Failed to decode image data: {e}");
            e
        })
        .ok()?;
    let reader = ImageReader::new(Cursor::new(raw_data))
        .with_guessed_format()
        .map_err(|e| {
            error!("Failed to guess image format: {e}");
            e
        })
        .ok()?;

    reader
        .decode()
        .map_err(|e| {
            error!("Image decoding failed: {e}");
            e
        })
        .ok()
}
