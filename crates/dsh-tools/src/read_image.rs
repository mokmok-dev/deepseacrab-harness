//! Read an image file as an inline model input.

use crate::input::parse;
use async_trait::async_trait;
use base64::Engine as _;
use dsh_core::{ContentBlock, Tool, ToolContext, ToolOutcome, ToolSchema};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;

/// Read a workspace image and return it as an image content block.
pub struct ReadImageTool;

const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Deserialize)]
struct ImageArgs {
    path: String,
}

#[async_trait]
impl Tool for ReadImageTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "read_image".to_string(),
            description: "Read a PNG, JPEG, WebP, or GIF image and attach it to the conversation."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: Value) -> ToolOutcome {
        let args: ImageArgs = match parse(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        let path = match context.readable_path(&args.path) {
            Ok(path) => path,
            Err(error) => return ToolOutcome::error(error.to_string()),
        };
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                return ToolOutcome::error(format!("cannot read {}: {error}", path.display()));
            }
        };
        if !metadata.is_file() {
            return ToolOutcome::error(format!("{} is not a file", path.display()));
        }
        if metadata.len() > MAX_IMAGE_BYTES {
            return ToolOutcome::error(format!(
                "image {} is larger than {} bytes",
                path.display(),
                MAX_IMAGE_BYTES
            ));
        }
        let Some(media_type) = media_type(&path) else {
            return ToolOutcome::error(format!(
                "unsupported image extension for {}",
                path.display()
            ));
        };
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return ToolOutcome::error(format!("cannot read {}: {error}", path.display()));
            }
        };
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let caption = format!(
            "image: {} ({} bytes, {media_type})",
            path.display(),
            bytes.len()
        );
        ToolOutcome {
            content: vec![
                ContentBlock::image(media_type, data),
                ContentBlock::text(caption),
            ],
            is_error: false,
            meta: None,
        }
    }
}

fn media_type(path: &std::path::Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::context;
    use dsh_core::PermissionMode;

    fn fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            directory.path().join("pixel.png"),
            [0x89, b'P', b'N', b'G', 0x0D, 0x0A],
        )
        .expect("png");
        directory
    }

    #[tokio::test]
    async fn images_become_inline_content() {
        let directory = fixture();
        let context = context(directory.path(), PermissionMode::ReadOnly);
        let outcome = ReadImageTool
            .execute(&context, json!({"path": "pixel.png"}))
            .await;
        assert!(!outcome.is_error);
        assert!(matches!(
            &outcome.content[0],
            ContentBlock::Image { media_type, data }
                if media_type == "image/png"
                    && base64::engine::general_purpose::STANDARD.decode(data).expect("decode")
                        == [0x89, b'P', b'N', b'G', 0x0D, 0x0A]
        ));
        assert!(outcome.text_content().contains("image/png"));
    }

    #[tokio::test]
    async fn supported_extensions_map_to_media_types() {
        for (extension, expected) in [
            ("jpg", "image/jpeg"),
            ("jpeg", "image/jpeg"),
            ("webp", "image/webp"),
            ("gif", "image/gif"),
        ] {
            let directory = tempfile::tempdir().expect("tempdir");
            let name = format!("image.{extension}");
            std::fs::write(directory.path().join(&name), b"bytes").expect("file");
            let context = context(directory.path(), PermissionMode::ReadOnly);
            let outcome = ReadImageTool.execute(&context, json!({"path": name})).await;
            assert!(!outcome.is_error);
            assert!(matches!(
                &outcome.content[0],
                ContentBlock::Image { media_type, .. } if media_type == expected
            ));
        }
    }

    #[tokio::test]
    async fn unsupported_files_and_missing_paths_are_errors() {
        let directory = fixture();
        std::fs::write(directory.path().join("notes.txt"), b"text").expect("txt");
        let context = context(directory.path(), PermissionMode::ReadOnly);
        assert!(
            ReadImageTool
                .execute(&context, json!({"path": "notes.txt"}))
                .await
                .is_error
        );
        assert!(
            ReadImageTool
                .execute(&context, json!({"path": "missing.png"}))
                .await
                .is_error
        );
        assert!(
            ReadImageTool
                .execute(&context, json!({"path": "/etc/passwd"}))
                .await
                .is_error
        );
    }

    #[tokio::test]
    async fn image_size_limit_is_inclusive() {
        assert_eq!(MAX_IMAGE_BYTES, 10 * 1024 * 1024);
        let directory = tempfile::tempdir().expect("tempdir");
        let exact = directory.path().join("exact.png");
        std::fs::File::create(&exact)
            .expect("file")
            .set_len(10 * 1024 * 1024)
            .expect("length");
        let context = context(directory.path(), PermissionMode::ReadOnly);
        let outcome = ReadImageTool
            .execute(&context, json!({"path": "exact.png"}))
            .await;
        assert!(!outcome.is_error);

        let over = directory.path().join("over.png");
        std::fs::File::create(&over)
            .expect("file")
            .set_len(10 * 1024 * 1024 + 1)
            .expect("length");
        let outcome = ReadImageTool
            .execute(&context, json!({"path": "over.png"}))
            .await;
        assert!(outcome.is_error);
    }

    #[tokio::test]
    async fn oversized_images_are_rejected_before_reading() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("large.png");
        let file = std::fs::File::create(&path).expect("file");
        file.set_len(MAX_IMAGE_BYTES + 1).expect("length");
        let context = context(directory.path(), PermissionMode::ReadOnly);
        let outcome = ReadImageTool
            .execute(&context, json!({"path": "large.png"}))
            .await;
        assert!(outcome.is_error);
        assert!(outcome.text_content().contains("larger than"));
    }

    #[test]
    fn media_types_cover_the_supported_extensions() {
        assert_eq!(media_type(std::path::Path::new("a.PNG")), Some("image/png"));
        assert_eq!(media_type(std::path::Path::new("a.txt")), None);
        assert_eq!(media_type(std::path::Path::new("noext")), None);
    }
}
