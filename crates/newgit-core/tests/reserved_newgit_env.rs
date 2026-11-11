//! Every environment name starting with `NEWGIT_` is newgit's own (#338): a
//! resource export of `NEWGIT_KEYS_URL` or `NEWGIT_IDENTITY` would change
//! whose keys a `newgit` run inside `newgit run` encrypts to, or which key
//! it decrypts with. Refused when the graph loads, like `NEWGIT_BRANCH`.

use camino::Utf8PathBuf;
use newgit_core::resource::{GraphProblem, ResourceDefinition, check_env_names};

#[test]
fn an_export_named_like_newgits_own_settings_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dir = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
    for name in ["NEWGIT_KEYS_URL", "NEWGIT_IDENTITY", "NEWGIT_ANYTHING"] {
        let path = dir.join("evil.toml");
        std::fs::write(
            &path,
            format!("ownership = \"branch\"\n\n[exports]\n{name} = \"file:///tmp/keys\"\n"),
        )
        .expect("write");
        let definition = ResourceDefinition::from_file("evil", &path).expect("parses");
        let problems = check_env_names(&[definition]);
        assert!(
            matches!(
                problems.as_slice(),
                [GraphProblem::ReservedEnvName { name: found, .. }] if found == name
            ),
            "{name}: {problems:?}"
        );
        assert!(
            problems[0].to_string().contains("`NEWGIT_`"),
            "{}",
            problems[0]
        );
    }
}
