use aria_core::workspace::cloud_sync_folders;

#[test]
fn returns_existing_sync_folders() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join("Dropbox")).unwrap();
    std::fs::create_dir(home.path().join("Google Drive")).unwrap();

    let got: std::collections::HashSet<_> = cloud_sync_folders(home.path())
        .into_iter()
        .collect();
    let want: std::collections::HashSet<_> = [
        home.path().join("Dropbox"),
        home.path().join("Google Drive"),
    ]
    .into_iter()
    .collect();
    assert_eq!(got, want);
}

#[test]
fn empty_when_no_sync_folders() {
    let home = tempfile::tempdir().unwrap();
    assert!(cloud_sync_folders(home.path()).is_empty());
}

#[test]
fn excludes_files_with_sync_names() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("OneDrive"), "not a folder").unwrap();
    std::fs::create_dir(home.path().join("Dropbox")).unwrap();

    let got = cloud_sync_folders(home.path());
    assert_eq!(got, vec![home.path().join("Dropbox")]);
}
