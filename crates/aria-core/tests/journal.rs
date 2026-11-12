use std::sync::Arc;

use aria_core::db::{self, Db};
use aria_core::journal::Blobs;
use aria_core::workspace::WorkspaceRoot;

struct Fixture {
    _ws_dir: tempfile::TempDir,
    _blob_dir: tempfile::TempDir,
    db: Arc<Db>,
    root: WorkspaceRoot,
    blobs: Blobs,
    ws_id: String,
}

fn fixture() -> Fixture {
    let ws_dir = tempfile::tempdir().unwrap();
    let blob_dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open_in_memory().unwrap());
    let ws_id = db
        .call(|c| db::insert_workspace(c, &ws_dir.path().display().to_string()))
        .unwrap()
        .id;
    let root = WorkspaceRoot::new(ws_dir.path()).unwrap();
    let blobs = Blobs::new(blob_dir.path(), Arc::clone(&db));
    Fixture {
        _ws_dir: ws_dir,
        _blob_dir: blob_dir,
        db,
        root,
        blobs,
        ws_id,
    }
}

#[test]
fn three_edits_undo_three_times() {
    let f = fixture();
    std::fs::write(f.root.root().join("a.txt"), "v0").unwrap();
    f.blobs.write_file(&f.ws_id, &f.root, "a.txt", "v1").unwrap();
    f.blobs.write_file(&f.ws_id, &f.root, "a.txt", "v2").unwrap();
    f.blobs.write_file(&f.ws_id, &f.root, "a.txt", "v3").unwrap();
    assert_eq!(std::fs::read_to_string(f.root.root().join("a.txt")).unwrap(), "v3");

    f.blobs.undo(&f.ws_id, &f.root, "a.txt").unwrap();
    assert_eq!(std::fs::read_to_string(f.root.root().join("a.txt")).unwrap(), "v2");
    f.blobs.undo(&f.ws_id, &f.root, "a.txt").unwrap();
    assert_eq!(std::fs::read_to_string(f.root.root().join("a.txt")).unwrap(), "v1");
    f.blobs.undo(&f.ws_id, &f.root, "a.txt").unwrap();
    assert_eq!(std::fs::read_to_string(f.root.root().join("a.txt")).unwrap(), "v0");
    assert!(!f.blobs.can_undo(&f.ws_id, "a.txt").unwrap());
    assert!(f.blobs.undo(&f.ws_id, &f.root, "a.txt").is_err());
}

#[test]
fn delete_then_undo_restores_bytes_and_path() {
    let f = fixture();
    let bytes = vec![0x00, 0xff, 0x10, b'x'];
    std::fs::create_dir_all(f.root.root().join("sub")).unwrap();
    std::fs::write(f.root.root().join("sub/bin.dat"), &bytes).unwrap();
    f.blobs.delete_file(&f.ws_id, &f.root, "sub/bin.dat").unwrap();
    assert!(!f.root.root().join("sub/bin.dat").exists());
    f.blobs.undo(&f.ws_id, &f.root, "sub/bin.dat").unwrap();
    assert_eq!(std::fs::read(f.root.root().join("sub/bin.dat")).unwrap(), bytes);
}

#[test]
fn undo_of_created_file_removes_it() {
    let f = fixture();
    f.blobs.write_file(&f.ws_id, &f.root, "new.txt", "hi").unwrap();
    assert!(f.root.root().join("new.txt").exists());
    f.blobs.undo(&f.ws_id, &f.root, "new.txt").unwrap();
    assert!(!f.root.root().join("new.txt").exists());
}

#[test]
fn move_then_undo_restores_source_and_destination() {
    let f = fixture();
    std::fs::write(f.root.root().join("src.txt"), "data").unwrap();
    f.blobs
        .move_file(&f.ws_id, &f.root, "src.txt", "dst.txt")
        .unwrap();
    assert!(!f.root.root().join("src.txt").exists());
    assert_eq!(std::fs::read_to_string(f.root.root().join("dst.txt")).unwrap(), "data");

    f.blobs.undo(&f.ws_id, &f.root, "dst.txt").unwrap();
    assert!(!f.root.root().join("dst.txt").exists());
    f.blobs.undo(&f.ws_id, &f.root, "src.txt").unwrap();
    assert_eq!(std::fs::read_to_string(f.root.root().join("src.txt")).unwrap(), "data");
}

#[test]
fn deduplicates_identical_content() {
    let f = fixture();
    f.blobs.put(b"same").unwrap();
    f.blobs.put(b"same").unwrap();
    assert_eq!(std::fs::read_dir(f.blobs.dir()).unwrap().count(), 1);
}

#[test]
fn prune_removes_only_unreferenced_blobs() {
    let f = fixture();
    let referenced = f.blobs.put(b"keep-me").unwrap();
    f.db.call(|c| db::insert_journal_entry(c, &f.ws_id, "a.txt", &referenced, true))
        .unwrap();
    let _stored = std::fs::read_dir(f.blobs.dir())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    std::fs::write(f.blobs.dir().join("deadbeef"), "orphan").unwrap();
    std::fs::write(f.blobs.dir().join("half.tmp"), "partial").unwrap();
    let removed = f.blobs.prune().unwrap();
    assert_eq!(removed, 2);
    assert!(f.blobs.dir().join(&referenced).exists());
    assert!(!f.blobs.dir().join("deadbeef").exists());
    assert!(!f.blobs.dir().join("half.tmp").exists());
}

#[test]
fn changed_paths_are_most_recent_first() {
    let f = fixture();
    f.blobs.write_file(&f.ws_id, &f.root, "a.txt", "1").unwrap();
    f.blobs.write_file(&f.ws_id, &f.root, "b.txt", "2").unwrap();
    f.blobs.write_file(&f.ws_id, &f.root, "a.txt", "3").unwrap();
    assert_eq!(f.blobs.changed_paths(&f.ws_id).unwrap(), vec!["a.txt", "b.txt"]);
    let _ = &f.db;
}

#[test]
fn undo_is_scoped_to_workspace() {
    let f = fixture();
    let other_ws = f
        .db
        .call(|c| db::insert_workspace(c, "/elsewhere"))
        .unwrap()
        .id;
    f.blobs.write_file(&f.ws_id, &f.root, "a.txt", "x").unwrap();
    assert!(f.blobs.undo(&other_ws, &f.root, "a.txt").is_err());
    assert_eq!(std::fs::read_to_string(f.root.root().join("a.txt")).unwrap(), "x");
}

#[test]
fn non_utf8_write_is_refused_by_read_text() {
    let f = fixture();
    std::fs::write(f.root.root().join("bin"), [0xff, 0xfe]).unwrap();
    assert!(Blobs::read_text(&f.root, "bin").is_err());
}


#[test]
fn every_write_journals_the_prior_version() {
    let f = fixture();
    f.blobs.write_file(&f.ws_id, &f.root, "n1.txt", "one").unwrap();
    f.blobs.write_file(&f.ws_id, &f.root, "n2.txt", "two").unwrap();
    std::fs::write(f.root.root().join("n2.txt"), "changed").unwrap();
    f.blobs.write_file(&f.ws_id, &f.root, "n2.txt", "three").unwrap();

    // Rows for files that did not exist carry an empty hash by design.
    let rows = f.db.call(db::blob_usage).unwrap();
    let created_rows = rows.get("").copied().unwrap_or(0);
    assert_eq!(created_rows, 2, "two files were created from nothing");

    // The third write snapshotted "changed" as a blob.
    f.blobs.undo(&f.ws_id, &f.root, "n2.txt").unwrap();
    assert_eq!(
        std::fs::read_to_string(f.root.root().join("n2.txt")).unwrap(),
        "changed"
    );
    let hashes = f.db.call(db::referenced_blobs).unwrap();
    assert_eq!(hashes.len(), 1, "the prior content is now the only real blob");
}
