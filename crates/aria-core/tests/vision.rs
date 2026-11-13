use std::sync::Arc;

use aria_core::db::{self, Db};
use aria_core::journal::Blobs;
use aria_core::tools::vision::{view_image, ViewImageArgs, VisionConfig};
use aria_core::tools::{ToolContext, ToolError, ToolRegistry};
use aria_core::workspace::{Scope, WorkspaceRoot};

const PNG_1X1: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D',
    b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
    0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, b'I', b'D', b'A', b'T', 0x78, 0x9C, 0x63, 0x00,
    0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, b'I',
    b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82,
];

struct Fx {
    _ws: tempfile::TempDir,
    ws_path: std::path::PathBuf,
    ctx: ToolContext,
}

fn fx(vision: Option<VisionConfig>) -> Fx {
    let ws = tempfile::tempdir().unwrap();
    let blob_dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open_in_memory().unwrap());
    let ws_id = db
        .call(|c| db::insert_workspace(c, &ws.path().display().to_string()))
        .unwrap()
        .id;
    let conv = db.call(|c| db::insert_conversation(c, Some(&ws_id), "")).unwrap();
    let run = db.call(|c| db::insert_run(c, &conv.id)).unwrap();
    let ws_root = WorkspaceRoot::new(ws.path()).unwrap();
    let ctx = ToolContext {
        scope: Scope::Workspace(ws_root.clone()),
        root: Some(ws_root),
        workspace_id: Some(ws_id),
        run_id: run.id,
        db: Arc::clone(&db),
        blobs: Arc::new(Blobs::new(blob_dir.path(), Arc::clone(&db))),
        shell_timeout_secs: 10,
        embed: None,
        search: None,
        vision,
        background: None,
    };
    Fx {
        ws_path: ws.path().to_path_buf(),
        _ws: ws,
        ctx,
    }
}

#[tokio::test]
async fn view_image_refuses_without_vision_config() {
    let f = fx(None);
    let err = view_image(
        &f.ctx,
        ViewImageArgs {
            path: "test.png".into(),
            question: None,
        },
    )
    .await
    .unwrap_err();

    assert!(
        matches!(err, ToolError::Refused(_)),
        "expected ToolError::Refused, got {err:?}"
    );
    assert!(err.to_string().contains("vision is not configured"));
}

#[tokio::test]
async fn view_image_refuses_nonexistent_file() {
    let f = fx(Some(VisionConfig {
        model: "test-vision".into(),
        base_url: "http://127.0.0.1:9".into(),
        api_key: None,
    }));
    let err = view_image(
        &f.ctx,
        ViewImageArgs {
            path: "nonexistent.png".into(),
            question: None,
        },
    )
    .await
    .unwrap_err();

    assert!(
        matches!(err, ToolError::Path(_) | ToolError::Io(_)),
        "expected Path or Io error for nonexistent file, got {err:?}"
    );

    let outcome = ToolRegistry::execute(
        "view_image",
        &serde_json::json!({ "path": "nonexistent.png" }).to_string(),
        &f.ctx,
    )
    .await;
    assert!(outcome.is_error);
}

#[tokio::test]
async fn view_image_refuses_non_image_extension_or_file() {
    let f = fx(Some(VisionConfig {
        model: "test-vision".into(),
        base_url: "http://127.0.0.1:9".into(),
        api_key: None,
    }));
    std::fs::write(f.ws_path.join("not_image.txt"), b"plain text content").unwrap();

    let err = view_image(
        &f.ctx,
        ViewImageArgs {
            path: "not_image.txt".into(),
            question: None,
        },
    )
    .await
    .unwrap_err();

    assert!(
        matches!(err, ToolError::Args(_)),
        "expected ToolError::Args, got {err:?}"
    );
    assert!(err.to_string().contains("not a supported image"));
}

#[tokio::test]
async fn view_image_reads_valid_image_and_attempts_request() {
    let f = fx(Some(VisionConfig {
        model: "test-vision".into(),
        base_url: "http://127.0.0.1:9".into(),
        api_key: Some("dummy-key".into()),
    }));
    std::fs::write(f.ws_path.join("dummy.png"), PNG_1X1).unwrap();

    let err = view_image(
        &f.ctx,
        ViewImageArgs {
            path: "dummy.png".into(),
            question: Some("Describe this 1x1 PNG".into()),
        },
    )
    .await
    .unwrap_err();

    assert!(
        matches!(err, ToolError::Refused(_)),
        "expected ToolError::Refused, got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("vision request failed"),
        "expected network request attempt error, got: {msg}"
    );
}

#[tokio::test]
async fn read_file_suggests_view_image_on_binary_image() {
    let f = fx(None);
    // Write invalid UTF-8 bytes to test.png
    std::fs::write(f.ws_path.join("test.png"), [0x89, b'P', b'N', b'G', 0xff, 0xfe]).unwrap();

    let outcome = ToolRegistry::execute(
        "read_file",
        &serde_json::json!({ "path": "test.png" }).to_string(),
        &f.ctx,
    )
    .await;

    assert!(outcome.is_error);
    assert!(
        outcome.text.contains("view_image"),
        "expected error to mention 'view_image', got: {}",
        outcome.text
    );
    assert!(
        outcome.text.contains("binary image file"),
        "expected error to mention 'binary image file', got: {}",
        outcome.text
    );
}
