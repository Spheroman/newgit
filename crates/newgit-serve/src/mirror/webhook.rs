//! `POST /v1/webhooks/github?project=<owner>/<name>`: GitHub saying a
//! mirror moved. Verified, recorded, and answered at once; the pull runs on
//! the mirror thread ([`super::pull`]).
//!
//! In order: the project must have a webhook secret here (else 503), and
//! `X-Hub-Signature-256` must be the HMAC-SHA256 of the raw body under it,
//! compared in constant time (else 401). `ping` is 200. A `pull_request`
//! event closing a merged pull request records which head it merged, so a
//! squash's pull need not ask GitHub. A `push` must come from a repository
//! one of `main`'s definitions exports to (else 403), to that target's
//! branch; whoever pushed must be a member who may merge root (else 403).
//! It is then queued (202), unless a pull of that commit to that target is
//! waiting already (200).

use hmac::{KeyInit as _, Mac as _};
use serde::Deserialize;
use serde_json::json;

use crate::App;
use crate::api::{ApiRequest, ApiResponse, ProjectContext};
use crate::db;
use crate::error::{ApiResult, HttpError, fail};
use crate::names::parse_project_key;
use crate::policy;

use super::store::{self, Queued};

/// Whether `header` is GitHub's signature of `body` under `secret`:
/// `sha256=` and 64 hex digits of the HMAC, compared in constant time.
pub fn verify(secret: &str, body: &[u8], header: Option<&str>) -> bool {
    let Some(hex) = header.and_then(|header| header.strip_prefix("sha256=")) else {
        return false;
    };
    if hex.len() != 64 {
        return false;
    }
    let Some(given) = (0..32)
        .map(|at| u8::from_str_radix(hex.get(at * 2..at * 2 + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()
    else {
        return false;
    };
    let Ok(mut mac) = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&given).is_ok()
}

/// GitHub's signature of `body` under `secret`, as the header carries it.
pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC takes a key of any length");
    mac.update(body);
    let bytes = mac.finalize().into_bytes();
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256={hex}")
}

#[derive(Deserialize, Default)]
struct Repository {
    full_name: Option<String>,
}

#[derive(Deserialize, Default)]
struct Push {
    #[serde(rename = "ref")]
    reference: Option<String>,
    after: Option<String>,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    repository: Repository,
    pusher: Option<Pusher>,
}

#[derive(Deserialize)]
struct Pusher {
    name: Option<String>,
}

#[derive(Deserialize, Default)]
struct PullRequestEvent {
    action: Option<String>,
    #[serde(default)]
    repository: Repository,
    pull_request: Option<PullRequest>,
}

#[derive(Deserialize)]
struct PullRequest {
    #[serde(default)]
    merged: bool,
    merge_commit_sha: Option<String>,
    head: Option<Head>,
}

#[derive(Deserialize)]
struct Head {
    sha: Option<String>,
    #[serde(rename = "ref")]
    reference: Option<String>,
    repo: Option<Repository>,
}

fn is_commit(id: &str) -> bool {
    id.len() == 40
        && id.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !id.bytes().all(|byte| byte == b'0')
}

fn ignored(why: impl Into<String>) -> ApiResult<ApiResponse> {
    Ok(ApiResponse {
        status: 200,
        body: json!({ "ignored": why.into() }),
    })
}

/// Refused, and said in the server's log: GitHub's own page shows only the
/// status and this answer.
fn refused(status: u16, why: String, project: &str, repo: &str) -> ApiResult<ApiResponse> {
    eprintln!(
        "{}",
        json!({ "webhook": "refused", "project": project, "repo": repo, "status": status, "why": why })
    );
    fail(status, why)
}

pub fn receive(app: &App, request: &ApiRequest) -> ApiResult<ApiResponse> {
    let project_key = request
        .query
        .get("project")
        .map(|key| crate::api::decode(key));
    let Some(id) = project_key.as_deref().and_then(parse_project_key) else {
        return fail(400, "the webhook's URL needs `?project=<owner>/<name>`");
    };
    let home = id.key();
    let Some(github) = app.github.project(&home) else {
        return fail(
            503,
            format!(
                "this remote has no webhook secret for `{home}`: its operator sets one in the file `--github` names"
            ),
        );
    };
    if !verify(
        &github.webhook_secret,
        &request.body,
        request.header("x-hub-signature-256"),
    ) {
        return fail(401, "bad signature");
    }
    let event = request.header("x-github-event").unwrap_or_default();
    let delivery = request.header("x-github-delivery");
    match event {
        "ping" => {
            return Ok(ApiResponse {
                status: 200,
                body: json!({ "pong": true }),
            });
        }
        "pull_request" => return pull_request(app, &home, &request.body),
        "push" => {}
        other => return ignored(format!("`{other}` events are not used")),
    }
    let push: Push = serde_json::from_slice(&request.body)
        .map_err(|_| HttpError::new(400, "the event is not JSON"))?;
    let repo = push.repository.full_name.clone().unwrap_or_default();
    let reference = push.reference.clone().unwrap_or_default();
    if reference.starts_with("refs/tags/") {
        return ignored("tags made on a mirror are not brought in by `newgit serve` yet");
    }
    let commit = push.after.clone().unwrap_or_default();
    if push.deleted || !is_commit(&commit) {
        return ignored("the branch was deleted");
    }

    let db = app.db.lock();
    let project = match ProjectContext::open(app, &db, &id) {
        Ok(project) => project,
        Err(error) => return refused(error.status, error.message, &home, &repo),
    };
    let Some(list) = db::list_of(&db, &home, "main")? else {
        return refused(404, format!("no branch `main` in {home}"), &home, &repo);
    };
    let pusher = push
        .pusher
        .and_then(|pusher| pusher.name)
        .unwrap_or_default()
        .to_lowercase();
    let member = db::is_member(&db, &home, &pusher)?;
    let root_facts = project
        .facts(&db, "root")?
        .or_else(|| policy::implicit_facts("root"));
    drop(db);

    // Which target this repository is, as `main`'s definitions say.
    let bench = super::Workbench::open(&project, &pusher_dir(&pusher), list)?;
    let targets = bench.targets()?;
    let Some(target) = super::target_for_repo(&targets, &repo) else {
        return refused(
            403,
            format!("no tracker on `main` exports to {repo}"),
            &home,
            &repo,
        );
    };
    if reference != format!("refs/heads/{}", target.branch) {
        return ignored(format!("not `{}`", target.branch));
    }
    // Landing here is a merge by whoever pushed there.
    let who = policy::Requester {
        login: pusher.clone(),
        member,
    };
    let may = root_facts
        .map(|facts| policy::may_maintain(&facts.maintainers, &who, &project.info.owner, "root"))
        .unwrap_or_else(|| Err("root is not defined".to_owned()));
    if pusher.is_empty() || may.is_err() {
        return refused(
            403,
            format!("`{pusher}` pushed this, and may not merge into {home}"),
            &home,
            &repo,
        );
    }
    let db = app.db.lock();
    let queued = store::queue(
        &db,
        &home,
        &target.name,
        &repo,
        &target.branch,
        &commit,
        &pusher,
        delivery,
        super::now(),
    )?;
    drop(db);
    app.mirrors.wake_pulls();
    eprintln!(
        "{}",
        json!({ "webhook": "queued", "project": home, "target": target.name, "commit": commit, "pusher": pusher, "delivery": delivery })
    );
    Ok(match queued {
        Queued::New(_) => ApiResponse {
            status: 202,
            body: json!({ "queued": commit }),
        },
        Queued::Already(_) => ApiResponse {
            status: 200,
            body: json!({ "queued": commit, "already": true }),
        },
    })
}

/// The login a workbench borrows landings of: a GitHub login is a newgit
/// login, and an empty one borrows none.
fn pusher_dir(pusher: &str) -> String {
    if pusher.is_empty() {
        "-".to_owned()
    } else {
        pusher.to_owned()
    }
}

/// A merged pull request: which head its merge commit (a squash, a merge, or
/// the last of a rebase) brought in, so its pull need not ask GitHub.
fn pull_request(app: &App, home: &str, body: &[u8]) -> ApiResult<ApiResponse> {
    let event: PullRequestEvent =
        serde_json::from_slice(body).map_err(|_| HttpError::new(400, "the event is not JSON"))?;
    let repo = event.repository.full_name.unwrap_or_default();
    let Some(pull) = event.pull_request else {
        return ignored("no pull request in it");
    };
    if event.action.as_deref() != Some("closed") || !pull.merged {
        return ignored("only a merged pull request says anything");
    }
    let (Some(merged), Some(head)) = (pull.merge_commit_sha, pull.head) else {
        return ignored("no merge commit or head");
    };
    let from_here = head
        .repo
        .and_then(|from| from.full_name)
        .is_some_and(|from| from.eq_ignore_ascii_case(&repo));
    let (Some(sha), Some(branch)) = (head.sha, head.reference) else {
        return ignored("no head");
    };
    if !from_here || !is_commit(&merged) || !is_commit(&sha) {
        return ignored("a pull request from a fork names no instance here");
    }
    let db = app.db.lock();
    if db::project(&db, home)?.is_none() {
        return fail(404, format!("no project `{home}`"));
    }
    store::note_head(&db, home, &repo, &merged, &sha, &branch, super::now())?;
    Ok(ApiResponse {
        status: 200,
        body: json!({ "noted": merged, "head": sha }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_verify_only_under_their_secret() {
        let body = br#"{"zen":"hi"}"#;
        let header = sign("s3cret", body);
        assert!(verify("s3cret", body, Some(&header)));
        assert!(!verify("other", body, Some(&header)));
        assert!(!verify("s3cret", b"{}", Some(&header)));
        assert!(!verify("s3cret", body, None));
        assert!(!verify("s3cret", body, Some("sha256=zz")));
        assert!(!verify(
            "s3cret",
            body,
            Some(&header.replace("sha256=", "sha1="))
        ));
    }
}
