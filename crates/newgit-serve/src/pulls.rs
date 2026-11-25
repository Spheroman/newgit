//! Pull requests (#167, #170): an instance's pushed work proposed for a
//! branch.
//!
//! A pull request names an instance and a base branch, never a commit. Its
//! head is the instance's latest push, read when asked, so each push moves it
//! and is a revision. It is marked merged by the landing that puts that head
//! on its branch, whoever lands it and however (`newgit merge` of the author's
//! instance or of a reviewer's `pr-N`, or a mirror's pull): the landing
//! decides, never a client's word (`crate::landing`).

use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{
    ApiRequest, ApiResponse, Caller, ProjectContext, body, created, ok, require_owner,
};
use crate::db;
use crate::error::{ApiResult, fail};

/// The longest title a pull request may have, in characters.
const TITLE_MAX: usize = 256;
/// The longest body, in bytes.
const BODY_MAX: usize = 64 * 1024;
/// How many pull requests a list returns unless asked, and at most.
const LIST_DEFAULT: usize = 30;
const LIST_MAX: usize = 100;

/// The commits a push sent, by tracker: its record's `code.head`.
pub fn pushed_commits(checkpoint: &Value) -> Vec<(String, String)> {
    checkpoint["record"]["code"]["head"]
        .as_object()
        .map(|head| {
            head.iter()
                .filter_map(|(tracker, commit)| {
                    commit
                        .as_str()
                        .map(|commit| (tracker.clone(), commit.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The commit a push's workspace was at.
fn pushed_head(checkpoint: &Value) -> Option<&str> {
    checkpoint["record"]["source"]["head_rev"].as_str()
}

/// `POST /pulls`: propose an instance's pushed work for a branch. `headRev`,
/// the commit the caller's workspace is at, must be the one its latest push
/// was at, so nothing unpushed is proposed by mistake.
pub fn create(
    project: &ProjectContext<'_>,
    db: &Connection,
    caller: &Caller,
    request: &ApiRequest,
) -> ApiResult<ApiResponse> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Spec {
        instance: Option<String>,
        base: Option<String>,
        title: Option<String>,
        #[serde(default)]
        body: String,
        #[serde(default)]
        draft: bool,
        head_rev: Option<String>,
    }
    let spec: Spec = body(request)?;
    let Some(name) = spec.instance else {
        return fail(400, "instance is required");
    };
    let title = spec.title.as_deref().map(str::trim).unwrap_or_default();
    if title.is_empty() {
        return fail(400, "a pull request needs a title");
    }
    if title.chars().count() > TITLE_MAX {
        return fail(400, format!("a title is at most {TITLE_MAX} characters"));
    }
    if spec.body.len() > BODY_MAX {
        return fail(400, format!("a body is at most {} KiB", BODY_MAX / 1024));
    }
    let key = &project.key;
    let push_first = || {
        fail(
            409,
            format!(
                "`{name}` has no push on {key} yet: push it first (`git push` in its workspace)"
            ),
        )
    };
    let Some(instance) =
        db::instance(db, key, &name)?.filter(|found| found.visible_to(&caller.login))
    else {
        return push_first();
    };
    require_owner(
        &instance.owner,
        &caller.login,
        &format!("open a pull request for `{name}`"),
    )?;
    caller.require_this_instance(&name)?;
    if instance.private {
        return fail(
            409,
            format!(
                "`{name}` is private: a pull request would show it to the project. Merge it, or spawn the work again without --private"
            ),
        );
    }
    let base = spec
        .base
        .unwrap_or_else(|| project.info.default_branch.clone());
    let Some(list) = db::list_of(db, key, &base)? else {
        return fail(404, format!("no branch `{base}` in {key}"));
    };
    let Some(head) = db::latest_push(db, key, &name)? else {
        return push_first();
    };
    // Skipped when either side cannot say: a convenience, not a guarantee.
    if let (Some(mine), Some(pushed)) = (spec.head_rev.as_deref(), pushed_head(&head))
        && mine != pushed
    {
        return fail(
            409,
            format!(
                "`{name}` is at {} here and its last push was at {}: push it first (`git push` in its workspace)",
                short(mine),
                short(pushed)
            ),
        );
    }
    // Already on the branch: it would be merged by the next landing, and
    // there is nothing to look at.
    let pushed = pushed_commits(&head);
    let landed = |(tracker, commit): &(String, String)| {
        list.get(tracker)
            .is_some_and(|now| now == commit || project.tracker(tracker).is_ancestor(commit, now))
    };
    if !pushed.is_empty() && pushed.iter().all(landed) {
        return fail(
            409,
            format!(
                "`{name}`'s last push has nothing `{base}` does not: there is nothing to propose"
            ),
        );
    }
    let opened = db::create_pull(
        db,
        key,
        &db::NewPull {
            title,
            body: &spec.body,
            author: &caller.login,
            agent: caller.agent.as_deref(),
            draft: spec.draft,
            instance: &instance,
            base: &base,
        },
    )?;
    let pull = match opened {
        Ok(pull) => pull,
        Err(number) => {
            return fail(
                409,
                format!("`{name}` already has pull request #{number} open against `{base}`"),
            );
        }
    };
    let mut answer = pull.summary();
    answer["body"] = json!(pull.body);
    answer["head"] = head_json(&head);
    created(answer)
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

/// A push as a pull request's head.
fn head_json(checkpoint: &Value) -> Value {
    json!({
        "checkpoint": format!(
            "{}/{}",
            checkpoint["instance"].as_str().unwrap_or_default(),
            checkpoint["id"].as_str().unwrap_or_default()
        ),
        "instance": checkpoint["instance"], "id": checkpoint["id"],
        "createdBy": checkpoint["createdBy"], "agent": checkpoint["agent"],
        "createdAt": checkpoint["createdAt"], "commit": pushed_head(checkpoint),
    })
}

/// `GET /pulls/<number>`: one pull request, head and body included. A hidden
/// one (on another user's private instance) is answered as a missing one.
pub fn view(
    project: &ProjectContext<'_>,
    db: &Connection,
    login: &str,
    number: &str,
) -> ApiResult<ApiResponse> {
    let key = &project.key;
    let missing = || fail(404, format!("no pull request #{number} in {key}"));
    let Some(pull) = number
        .parse::<i64>()
        .ok()
        .map(|number| db::pull(db, key, number))
        .transpose()?
        .flatten()
    else {
        return missing();
    };
    let instance = db::pull_instance(db, key, &pull)?;
    if instance
        .as_ref()
        .is_some_and(|instance| !instance.visible_to(login))
    {
        return missing();
    }
    // No head once its instance is gone, or another has its name.
    let head = match &instance {
        Some(_) => db::latest_push(db, key, &pull.instance)?.map(|head| head_json(&head)),
        None => None,
    };
    let mut answer = pull.summary();
    answer["body"] = json!(pull.body);
    answer["head"] = head.unwrap_or(Value::Null);
    ok(answer)
}

/// `GET /pulls?state=open|closed|merged|all&limit=n`: newest activity first,
/// bodies left out.
pub fn list(
    project: &ProjectContext<'_>,
    db: &Connection,
    login: &str,
    request: &ApiRequest,
) -> ApiResult<ApiResponse> {
    let state = match request.query.get("state").map(String::as_str) {
        None | Some("open") => Some("open"),
        Some("all") => None,
        Some(state @ ("closed" | "merged")) => Some(state),
        Some(other) => {
            return fail(
                400,
                format!("state is open, closed, merged, or all, not `{other}`"),
            );
        }
    };
    let limit = match request.query.get("limit") {
        None => LIST_DEFAULT,
        Some(limit) => match limit.parse::<usize>() {
            Ok(limit) if (1..=LIST_MAX).contains(&limit) => limit,
            _ => return fail(400, format!("limit is 1 to {LIST_MAX}")),
        },
    };
    let pulls: Vec<Value> = db::pulls(db, &project.key, state, login, limit)?
        .iter()
        .map(db::Pull::summary)
        .collect();
    ok(json!({ "pulls": pulls }))
}
