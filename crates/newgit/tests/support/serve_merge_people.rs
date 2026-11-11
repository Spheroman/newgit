// A second person on `merge_project`'s project, and changes to `blog`'s
// definition made by hand: the server merge's authorization and audience
// tests. `include!`d after `support/serve.rs` and `support/serve_merge.rs`.

#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn add_member(server: &Server, token: &str, login: &str) {
    let bearer = format!("Bearer {token}");
    let (status, answer) = http(
        &server.url,
        "POST",
        "/v1/projects/jack/store/members",
        &[("authorization", bearer.as_str())],
        &format!("{{\"login\":\"{login}\"}}"),
    );
    assert_eq!(status, 200, "{answer}");
}

#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn remove_member(server: &Server, token: &str, login: &str) {
    let bearer = format!("Bearer {token}");
    let (status, answer) = http(
        &server.url,
        "DELETE",
        &format!("/v1/projects/jack/store/members/{login}"),
        &[("authorization", bearer.as_str())],
        "",
    );
    assert_eq!(status, 200, "{answer}");
}

#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn epoch(server: &Server, token: &str) -> u64 {
    project_json(server, token)["epoch"]
        .as_u64()
        .expect("an epoch")
}

/// A definition's text with each `(key, value)` line set.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn with_fields(text: &str, fields: &[(&str, &str)]) -> String {
    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| {
            !fields
                .iter()
                .any(|(key, _)| line.split('=').next().map(str::trim) == Some(*key))
        })
        .map(str::to_owned)
        .collect();
    for (key, value) in fields {
        lines.push(format!("{key} = \"{value}\""));
    }
    lines.join("\n") + "\n"
}

/// A change to `blog`'s definition by `token`'s caller, made on `base`:
/// access's commit and root's, each in a clone of its own.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
struct Definition {
    list: List,
    access: (Utf8PathBuf, String),
    root: (Utf8PathBuf, String),
}

#[allow(dead_code, reason = "not every binary that includes this uses it")]
impl Definition {
    fn sent(&self) -> Vec<Sent<'_>> {
        vec![
            Sent {
                tracker: "access",
                dir: &self.access.0,
                commit: self.access.1.clone(),
            },
            Sent {
                tracker: "root",
                dir: &self.root.0,
                commit: self.root.1.clone(),
            },
        ]
    }

    /// Access's `main` asked to follow, as `newgit merge` asks.
    fn moves(&self, base: &List) -> Value {
        serde_json::json!([
            { "tracker": "access", "branch": "main", "old": base["access"], "new": self.access.1 },
        ])
    }
}

#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn blog_definition(
    server: &Server,
    token: &str,
    temp: &Utf8Path,
    home: &Utf8Path,
    base: &List,
    fields: &[(&str, &str)],
    name: &str,
) -> Definition {
    let access = clone_tracker(server, token, temp, "access", &format!("access-{name}"));
    let root = clone_tracker(server, token, temp, "root", &format!("root-{name}"));
    git(home, &access, &["checkout", "-q", "--detach", &base["access"]]);
    let text = std::fs::read_to_string(access.join("blog.toml")).expect("blog.toml");
    let access_commit = commit_on(
        home,
        &access,
        &base["access"],
        &[("blog.toml", Some(&with_fields(&text, fields)))],
        &[],
        &format!("blog: {fields:?}"),
    );
    let root_commit = commit_on(
        home,
        &root,
        &base["root"],
        &[],
        &[("access", &access_commit)],
        "Update access",
    );
    let mut list = base.clone();
    list.insert("access".into(), access_commit.clone());
    list.insert("root".into(), root_commit.clone());
    Definition {
        list,
        access: (access, access_commit),
        root: (root, root_commit),
    }
}

/// `blog`'s definition set by jack on `main`, landed.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn set_blog(
    server: &Server,
    jack: &str,
    temp: &Utf8Path,
    home: &Utf8Path,
    fields: &[(&str, &str)],
    name: &str,
) {
    let base = list_of(server, jack, "main");
    let change = blog_definition(server, jack, temp, home, &base, fields, name);
    let (status, answer) = merge_by_hand(
        server,
        jack,
        "main",
        &base,
        &change.list,
        &change.sent(),
        change.moves(&base),
    );
    assert_eq!(status, 200, "{answer}");
}

/// `merge_project`'s project with ana a member: `(jack's home, jack's
/// token, ana's token)`.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn two_people(server: &Server, temp: &Utf8Path) -> (Utf8PathBuf, String, String) {
    let home = temp.join("jack");
    let jack = login(server, &home, "jack");
    let ana = login(server, &temp.join("ana"), "ana");
    let _store = merge_project(temp, &home, server);
    add_member(server, &jack, "ana");
    (home, jack, ana)
}

/// One merge of root's own files by `token`'s caller, made on `base` and
/// begun (its commit sent): its list.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn root_change(
    server: &Server,
    token: &str,
    temp: &Utf8Path,
    home: &Utf8Path,
    base: &List,
    files: &[(&str, Option<&str>)],
    name: &str,
) -> List {
    let root = clone_tracker(server, token, temp, "root", &format!("root-{name}"));
    let commit = commit_on(home, &root, &base["root"], files, &[], name);
    let mut list = base.clone();
    list.insert("root".into(), commit.clone());
    let (status, begun) = begin_by_hand(
        server,
        token,
        "main",
        base,
        &list,
        &[Sent {
            tracker: "root",
            dir: &root,
            commit,
        }],
    );
    assert_eq!(status, 200, "{begun}");
    list
}

/// The members' store and the public one.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn devs_and_public(server: &Server) -> (Utf8PathBuf, Utf8PathBuf) {
    let stores = server.data.join("homes/jack/store/stores");
    (stores.join("project-devs.git"), stores.join("public.git"))
}

/// Every landing ref left in any landing of `login`'s.
#[allow(dead_code, reason = "not every binary that includes this uses it")]
fn landing_refs_of(server: &Server, home: &Utf8Path, login: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut dirs = vec![server.data.join("homes/jack/store/landing").join(login)];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
                continue;
            };
            if !path.is_dir() {
                continue;
            }
            if path.extension() != Some("git") {
                dirs.push(path);
                continue;
            }
            let refs = in_store(
                home,
                &path,
                &["for-each-ref", "--format=%(refname)", "refs/newgit/landing/"],
            );
            found.extend(
                refs.lines()
                    .filter(|line| !line.is_empty())
                    .map(|line| format!("{path}: {line}")),
            );
        }
    }
    found
}
