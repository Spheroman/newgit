use camino::Utf8PathBuf;
use newgit_core::BranchInstance;
use newgit_core::branch::branch_slug;
use newgit_core::materializer::{Materializer, RealDirMaterializer};
use newgit_core::store::MetadataStore;
use newgit_core::templates::starter_template;
use tempfile::tempdir;

fn utf8_tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let tempdir = tempdir().expect("tempdir should be created");
    let path = Utf8PathBuf::from_path_buf(tempdir.path().to_path_buf())
        .expect("tempdir path should be UTF-8");
    (tempdir, path)
}

#[test]
fn init_creates_metadata_layout_and_template_files() {
    let (_tempdir, root) = utf8_tempdir();
    let store = MetadataStore::init(&root, "example").expect("metadata init should succeed");

    assert!(store.paths().config.is_file());
    assert!(store.paths().branches.is_dir());
    assert!(store.paths().trackers.is_dir());
    assert!(store.paths().templates.join("env-file.toml").is_file());
    assert!(store.paths().templates.join("base.env").is_file());
}

#[test]
fn tracker_template_can_be_added_without_core_knowing_the_resource_type() {
    let (_tempdir, root) = utf8_tempdir();
    let store = MetadataStore::init(&root, "example").expect("metadata init should succeed");

    let path = store
        .add_tracker_from_template("preview", "external")
        .expect("tracker should be written");
    let definitions = store
        .load_tracker_definitions()
        .expect("tracker definitions should load");

    assert!(path.is_file());
    assert_eq!(definitions[0].name, "preview");
    assert_eq!(definitions[0].kind, "external");
}

#[test]
fn branch_spawn_record_binds_tracker_definitions() {
    let (_tempdir, root) = utf8_tempdir();
    let store = MetadataStore::init(&root, "example").expect("metadata init should succeed");
    let tracker = starter_template("process", "app").expect("template should parse");
    store
        .write_tracker_definition(&tracker, false)
        .expect("tracker should be written");

    let definitions = store
        .load_tracker_definitions()
        .expect("tracker definitions should load");
    let workspace = root.join("workspaces").join(branch_slug("auth-refactor"));
    let branch = BranchInstance::new("auth-refactor", workspace, &definitions)
        .expect("branch should be created");

    RealDirMaterializer
        .materialize(&branch)
        .expect("workspace should be created");
    let branch_file = store
        .write_branch(&branch)
        .expect("branch metadata should be written");

    assert!(branch.workspace_path.is_dir());
    assert!(branch_file.is_file());
    assert!(branch.trackers.contains_key("app"));
}
