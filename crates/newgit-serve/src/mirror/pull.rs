//! The pull queue: what a mirror has beyond `main`, landed as a merge.
//!
//! One thread moves every project's queue on, oldest pull first, and the
//! queue is rows in SQLite, so a restart (a deploy) resumes it where it was
//! (#353). Each step:
//!
//! 1. **Looks.** Fetches the mirror's branch into a scratch repository,
//!    composes `main` as the target sees it, and walks what the mirror has
//!    beyond that. Nothing: our own export, or a redelivery; it ends there.
//! 2. **Checks whose work it is, by content** (#404, #451). A branch
//!    exported for a pull request leaves out what the target withholds, so
//!    landing its commits alone would drop an instance's work in a withheld
//!    tracker without a word. Every commit the pull would land is matched
//!    against every instance holding such work, composed as the target sees
//!    it: a commit that *is* (or whose pull request's head is) an instance's
//!    commit whose withheld pins are not on `main` is refused, naming it.
//!    Nothing is read from a branch's name. A squash or rebase has new ids,
//!    so for each commit nothing else accounts for, the pull request it came
//!    from is read from GitHub's `pull_request` event when one came, or
//!    asked of GitHub; while GitHub cannot say, the pull waits and asks
//!    again, never refusing on a guess (#398).
//! 3. **Brings the commits in** (`compose::bring_in`: the withheld
//!    trackers' files put back from their parents, #401's one-sided rule
//!    included), splits them by tracker, puts each tracker's new commits in
//!    the pusher's landing, and lands them with `merge::begin` and
//!    `merge::finish`, as the pusher: a fast-forward when `main` did not
//!    move since the mirror's base, the work re-made on `main` when what
//!    landed since changed other paths, Git's merge over the composed tree
//!    otherwise. One landing, by the same rules as any member's merge.
//!
//! A refusal is an answer: the pull ends, saying why and what to do. A later
//! pull refused only because it carries a refused pull's commits says so,
//! once, on the first (`store::state`). A failure is retried with backoff,
//! up to [`ATTEMPTS`] times or [`LIMIT`], then gives up, saying so.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use camino::Utf8Path;
use newgit_core::compose::{self, ComposeOptions, SplitError, SplitOptions};
use newgit_core::export_target::ExportTarget;
use newgit_core::objects::{GITLINK_MODE, Objects};
use newgit_core::tracker::TrackerDefinition;
use serde_json::json;

use crate::App;
use crate::api::{Caller, ProjectContext};
use crate::db::{self, List};
use crate::error::{ApiResult, HttpError};
use crate::merge;
use crate::names::{RepoRef, lineage, parse_project_key};
use crate::server_merge::Made;

use super::store::{self, Lookup, Pull};
use super::{Workbench, core, git_ok, is_ancestor, mirror_git_ok, now};

/// How often the thread looks at the queue when nothing wakes it.
pub const TICK: Duration = Duration::from_secs(1);
/// Past this, a pull still not landed gives up; a redelivery starts it again.
pub const LIMIT: i64 = 2 * 60 * 60;
/// How many failed attempts a pull makes before it gives up: with the wait
/// doubling from [`RETRY_FIRST`] to [`RETRY_MAX`], about three quarters of an
/// hour, longer than an outage of GitHub's or a restart.
pub const ATTEMPTS: u32 = 10;
pub const RETRY_FIRST: i64 = 15;
pub const RETRY_MAX: i64 = 10 * 60;
/// How soon GitHub is asked again after it could not answer, doubling.
pub const ASK_AGAIN: i64 = 30;
/// How long "no pull request" is not believed: GitHub's list can lag the
/// push that told us. Past this, a commit with none is a direct push.
pub const INDEX_WAIT: i64 = 2 * 60;
/// The most commits one pull asks GitHub about; past it, nobody can say.
pub const ASKED_MAX: usize = 20;
const GITHUB_TIMEOUT: Duration = Duration::from_secs(10);

/// How long after its `attempts`th failure something is tried again.
pub fn retry_delay(attempts: u32, first: i64) -> i64 {
    let doublings = attempts.saturating_sub(1).min(20);
    (scaled(first).saturating_mul(1 << doublings)).min(RETRY_MAX)
}

/// `seconds`, or one second for each minute of it when
/// `NEWGIT_SERVE_MIRROR_WAITS=short`: for the tests, which wait on GitHub
/// that is not there.
pub fn scaled(seconds: i64) -> i64 {
    static SHORT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *SHORT.get_or_init(|| {
        std::env::var("NEWGIT_SERVE_MIRROR_WAITS").is_ok_and(|value| value == "short")
    }) {
        (seconds / 60).max(1)
    } else {
        seconds
    }
}

/// How a step of a pull came out.
#[derive(Debug)]
enum Step {
    Landed,
    /// Its commit is on `main` already.
    Nothing,
    Refused {
        why: String,
        /// The commits it would have landed: a later pull carrying one of
        /// them is refused with it.
        walk: Vec<String>,
    },
    GaveUp(String),
    /// Waiting on GitHub: nothing failed, nothing is spent.
    Wait,
    /// `main` moved under it: start again at once.
    Again,
}

/// Move the head of every project's queue on by one step.
pub fn step_all(app: &App) {
    let heads = {
        let db = app.db.lock();
        store::heads(&db).unwrap_or_default()
    };
    for pull in heads {
        let now = now();
        if pull.retry_at.is_some_and(|at| now < at) && now - pull.queued_at <= LIMIT {
            continue;
        }
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| step(app, &pull)));
        let outcome = match ran {
            Ok(outcome) => outcome,
            Err(panic) => Err(HttpError::new(
                500,
                format!("the pull panicked: {}", super::panic_text(&*panic)),
            )),
        };
        if let Err(error) = settle(app, &pull, outcome) {
            eprintln!(
                "{}",
                json!({ "webhook": "could not record how a pull went", "project": pull.home, "why": error.cause() })
            );
        }
    }
}

fn log(pull: &Pull, what: &str, why: Option<&str>) {
    eprintln!(
        "{}",
        json!({ "webhook": what, "project": pull.home, "target": pull.target, "commit": pull.commit, "pusher": pull.pusher, "why": why })
    );
}

/// Write down how a step came out: an end, a failure, or nothing.
fn settle(app: &App, pull: &Pull, outcome: ApiResult<Step>) -> ApiResult<()> {
    let now = now();
    let db = app.db.lock();
    let over_time = now - pull.queued_at > LIMIT;
    match outcome {
        Ok(Step::Landed) => {
            log(pull, "landed", None);
            store::end(&db, pull, "landed", None, None, now)?;
        }
        Ok(Step::Nothing) => {
            log(pull, "nothing to bring in", None);
            store::end(&db, pull, "nothing", None, None, now)?;
        }
        Ok(Step::Refused { why, walk }) => {
            let blocked_by = first_refusal(&db, pull, &walk)?;
            let why = match &blocked_by {
                Some((commit, at)) => format!(
                    "it carries commit {}, whose pull was refused at {}, and is refused with it: {why}",
                    &commit[..commit.len().min(12)],
                    db::iso_at(*at)
                ),
                None => why,
            };
            log(pull, "refused", Some(&why));
            store::end(
                &db,
                pull,
                "refused",
                Some(&why),
                blocked_by.as_ref().map(|(commit, _)| commit.as_str()),
                now,
            )?;
        }
        Ok(Step::GaveUp(why)) => {
            log(pull, "gave up", Some(&why));
            store::end(&db, pull, "gave up", Some(&why), None, now)?;
        }
        Ok(Step::Wait | Step::Again) if over_time => {
            let why = format!(
                "it waited {} hours, {}. Nothing moved: {}",
                LIMIT / 3600,
                match pull
                    .lookup
                    .as_ref()
                    .and_then(|lookup| lookup.failed.as_deref())
                {
                    Some(failed) => format!(
                        "and GitHub still could not say which pull request it came from ({failed})"
                    ),
                    None => "without finishing".to_owned(),
                },
                super::bring_it_in(&pull.target)
            );
            log(pull, "gave up", Some(&why));
            store::end(&db, pull, "gave up", Some(&why), None, now)?;
        }
        Ok(Step::Wait | Step::Again) => {}
        Err(error) => {
            let why = error.cause().to_owned();
            let attempts = store::failed(
                &db,
                pull.id,
                &why,
                now + retry_delay(pull.attempts + 1, RETRY_FIRST),
                now,
            )?;
            log(pull, "pull failed", Some(&why));
            if attempts >= ATTEMPTS || over_time {
                let why = format!(
                    "it failed {attempts} times; the last: {why}. Nothing moved: {}",
                    super::bring_it_in(&pull.target)
                );
                log(pull, "gave up", Some(&why));
                store::end(&db, pull, "gave up", Some(&why), None, now)?;
            }
        }
    }
    Ok(())
}

/// The first refusal still standing for this target whose commit this pull
/// would also land: the one it waits on. Standing means no pull of the
/// target landed or found nothing since.
fn first_refusal(
    db: &rusqlite::Connection,
    pull: &Pull,
    walk: &[String],
) -> ApiResult<Option<(String, i64)>> {
    let mut standing: Vec<store::Ended> = Vec::new();
    for ended in store::ended(db, &pull.home)? {
        if ended.target != pull.target {
            continue;
        }
        match ended.outcome.as_str() {
            "landed" | "nothing" => standing.clear(),
            "refused" if ended.commit != pull.commit => standing.push(ended),
            _ => {}
        }
    }
    let carried = standing
        .iter()
        .find(|ended| ended.blocked_by.is_none() && walk.contains(&ended.commit));
    Ok(carried.map(|ended| (ended.commit.clone(), ended.at)))
}

/// Who a pull lands as: whoever pushed to the mirror, with no token of
/// their own. Their rules are a member's merge's rules.
fn caller(pull: &Pull) -> Caller {
    Caller {
        login: pull.pusher.clone(),
        token: String::new(),
        token_id: format!("mirror-pull-{}", pull.id),
        kind: "login".to_owned(),
        agent: None,
        project: Some(pull.home.clone()),
        instance: None,
        expires: now() + 3600,
    }
}

fn step(app: &App, pull: &Pull) -> ApiResult<Step> {
    let now = now();
    // Waiting for GitHub: not asked again before its time.
    if let Some(lookup) = &pull.lookup {
        let wait = match lookup.failed {
            Some(_) => retry_delay(lookup.failures.max(1), ASK_AGAIN),
            None => scaled(ASK_AGAIN),
        };
        if now - lookup.last < wait {
            return Ok(Step::Wait);
        }
    }
    let Some(id) = parse_project_key(&pull.home) else {
        return Ok(Step::GaveUp(format!("`{}` is not a project", pull.home)));
    };
    let caller = caller(pull);
    let (project, list) = {
        let db = app.db.lock();
        store::set_step(&db, pull.id, "looking", now)?;
        let project = ProjectContext::load(app, &db, &caller, &id)?;
        let Some(list) = db::list_of(&db, &pull.home, "main")? else {
            return Ok(Step::GaveUp(format!("there is no `main` in {}", pull.home)));
        };
        (project, list)
    };
    let token = app
        .github
        .project(&pull.home)
        .and_then(|github| github.token.clone());
    let bench = Workbench::open(&project, &pull.pusher, list)?;
    let dir = bench.dir().to_owned();
    let Some(target) = bench
        .targets()?
        .into_iter()
        .find(|target| target.name == pull.target)
    else {
        return Ok(Step::GaveUp(format!(
            "`{}` is no longer an export target on `main`",
            pull.target
        )));
    };
    mirror_git_ok(
        &dir,
        token.as_deref(),
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            &target.push_url(),
            &format!("+refs/heads/{}:refs/mirrors/tip", pull.branch),
        ],
    )?;
    let is_commit =
        git_ok(&dir, &["cat-file", "-t", &pull.commit]).is_ok_and(|kind| kind == "commit");
    if !is_commit || !is_ancestor(&dir, &pull.commit, "refs/mirrors/tip") {
        return Ok(Step::Refused {
            why: format!(
                "`{}` on `{}` no longer holds commit {}",
                pull.branch, target.name, pull.commit
            ),
            walk: Vec::new(),
        });
    }
    Landing {
        app,
        pull,
        project: &project,
        caller: &caller,
        bench: &bench,
        dir: &dir,
        target: &target,
        at: target.declared_by().to_owned(),
        token: token.as_deref(),
    }
    .run()
}

/// One pull's look, check, and landing.
struct Landing<'a> {
    app: &'a App,
    pull: &'a Pull,
    project: &'a ProjectContext<'a>,
    caller: &'a Caller,
    bench: &'a Workbench,
    dir: &'a Utf8Path,
    target: &'a ExportTarget,
    /// The tracker the target is rooted at: `root`, or its own.
    at: String,
    token: Option<&'a str>,
}

/// An instance holding work in a tracker the target withholds that `main`
/// does not have.
struct Holder {
    instance: String,
    /// Its fork of the tracker the target is rooted at, by branch.
    heads: Vec<String>,
}

impl Landing<'_> {
    fn run(&self) -> ApiResult<Step> {
        let dir = self.dir;
        let pin = self
            .bench
            .list
            .get(&self.at)
            .cloned()
            .ok_or_else(|| HttpError::new(409, format!("`{}` is not on `main`", self.at)))?;
        let withheld = self.bench.withheld(self.target);
        let holders = self.holders(&withheld)?;

        // `main` as the target sees it, and each holder's work the same way,
        // in one composition: composing is deterministic, so a commit that
        // came from here is in it under the very id it has there.
        let ships = |name: &str| self.bench.ships(self.target, name);
        let mut tips = vec![("refs/mirrors/view/target".to_owned(), pin.clone())];
        for (at, holder) in holders.iter().enumerate() {
            for (n, head) in holder.heads.iter().enumerate() {
                tips.push((format!("refs/mirrors/view/held/{at}/{n}"), head.clone()));
            }
        }
        let views = compose::compose(
            dir,
            &self.at,
            &tips,
            &ComposeOptions {
                readable: &ships,
                admit: None,
            },
        )
        .map_err(core)?;
        let exported = views
            .first()
            .and_then(|view| view.tip.clone())
            .ok_or_else(|| HttpError::new(409, format!("`{}` composes to nothing", self.at)))?;
        let walk = lines(&git_ok(
            dir,
            &[
                "rev-list",
                "--topo-order",
                "--reverse",
                &self.pull.commit,
                "--not",
                &exported,
            ],
        )?);
        if walk.is_empty() {
            return Ok(Step::Nothing);
        }
        let refused = |why: String| Step::Refused {
            why,
            walk: walk.clone(),
        };

        // Whose work each commit is, by what it is.
        let mut theirs: HashMap<String, (String, String)> = HashMap::new();
        let mut at = 1;
        for holder in &holders {
            for _ in &holder.heads {
                if let Some(view) = views.get(at).and_then(|view| view.tip.clone()) {
                    for commit in lines(&git_ok(dir, &["rev-list", &view, "--not", &exported])?) {
                        if let Some((_, own)) = compose::read_root(dir, &commit).map_err(core)? {
                            theirs.insert(commit, (holder.instance.clone(), own));
                        }
                    }
                }
                at += 1;
            }
        }
        if !holders.is_empty() {
            // What a merge commit in the walk brought in from its other
            // parents is a pull request's own commits, its head among them
            // and checked as itself: nothing to ask GitHub. Only commits on
            // the line of first parents are new ids (a squash, a rebase, a
            // direct push) whose pull request must be asked.
            let mut merged_in: BTreeSet<String> = BTreeSet::new();
            for commit in &walk {
                let parents = parents_of(dir, commit)?;
                if let Some((first, others)) = parents.split_first()
                    && !others.is_empty()
                {
                    let mut args = vec!["rev-list".to_owned()];
                    args.extend(others.iter().cloned());
                    args.extend(["--not".to_owned(), first.clone(), exported.clone()]);
                    let args: Vec<&str> = args.iter().map(String::as_str).collect();
                    merged_in.extend(lines(&git_ok(dir, &args)?));
                }
            }
            let mut asked = Vec::new();
            for commit in &walk {
                if let Some((instance, own)) = theirs.get(commit) {
                    if let Some(why) = self.leaves_out(commit, commit, instance, own, &withheld)? {
                        return Ok(refused(why));
                    }
                    continue;
                }
                if parents_of(dir, commit)?.len() == 1 && !merged_in.contains(commit) {
                    asked.push(commit.clone());
                }
            }
            match self.heads_of(&asked)? {
                Heads::Known(heads) => {
                    for (commit, head) in heads {
                        if let Some((instance, own)) = theirs.get(&head)
                            && let Some(why) =
                                self.leaves_out(&commit, &head, instance, own, &withheld)?
                        {
                            return Ok(refused(why));
                        }
                    }
                }
                Heads::Wait => return Ok(Step::Wait),
                Heads::GaveUp(why) => return Ok(Step::GaveUp(why)),
            }
        }

        {
            let db = self.app.db.lock();
            store::set_step(&db, self.pull.id, "landing", now())?;
        }
        match self.land(&walk, &withheld, &pin) {
            Ok(step) => Ok(step),
            Err(LandError::Refused(Refusal(why))) => Ok(refused(why)),
            Err(LandError::Again) => Ok(Step::Again),
            Err(LandError::Failed(error)) => Err(error),
        }
    }

    /// Every instance with a fork of a withheld tracker holding a commit
    /// that tracker's `main` does not, and its fork of the target's tracker.
    fn holders(&self, withheld: &[TrackerDefinition]) -> ApiResult<Vec<Holder>> {
        if withheld.is_empty() {
            return Ok(Vec::new());
        }
        let instances = {
            let db = self.app.db.lock();
            db::instances(&db, &self.project.key)?
        };
        let mut holders = Vec::new();
        for instance in instances {
            let fork = |tracker: &str| {
                self.project.locate(
                    &RepoRef::Fork {
                        project: self.project.id.clone(),
                        instance: instance.name.clone(),
                        tracker: tracker.to_owned(),
                    },
                    instance.private,
                )
            };
            let mut holds = false;
            for definition in withheld {
                if !instance.forks.contains(&definition.name) {
                    continue;
                }
                let main = self.bench.list.get(&definition.name);
                let tracker_main = format!("refs/trackers/{}/heads/main", definition.name);
                for (_, head) in fork(&definition.name).list_refs("refs/heads/")? {
                    let landed = main
                        .is_some_and(|main| *main == head || is_ancestor(self.dir, &head, main))
                        || is_ancestor(self.dir, &head, &tracker_main);
                    if !landed {
                        holds = true;
                    }
                }
            }
            if !holds {
                continue;
            }
            let refs = fork(&self.at).list_refs("refs/heads/")?;
            let heads: Vec<String> = refs.iter().map(|(_, head)| head.clone()).collect();
            if heads.is_empty() {
                // No fork of the target's own tracker: nothing it holds can
                // be in what the target ships.
                continue;
            }
            self.bench.add_fork_refs(&self.at, &instance.name, &refs)?;
            holders.push(Holder {
                instance: instance.name,
                heads,
            });
        }
        Ok(holders)
    }

    /// Whether landing `commit` (on the mirror) leaves out work: `head` is
    /// `instance`'s commit `own`, as the target sees it, and `own` pins a
    /// withheld tracker at work `main` does not have.
    fn leaves_out(
        &self,
        commit: &str,
        head: &str,
        instance: &str,
        own: &str,
        withheld: &[TrackerDefinition],
    ) -> ApiResult<Option<String>> {
        let index = compose::index(self.dir, &self.at, own).map_err(core)?;
        for definition in withheld {
            let Some(pin) = index.get(&definition.name) else {
                continue;
            };
            let main = self.bench.list.get(&definition.name);
            let tracker_main = format!("refs/trackers/{}/heads/main", definition.name);
            let landed = main.is_some_and(|main| main == pin || is_ancestor(self.dir, pin, main))
                || is_ancestor(self.dir, pin, &tracker_main);
            if landed {
                continue;
            }
            let what = if commit == head {
                format!("is `{instance}`'s commit {}", short(own))
            } else {
                format!(
                    "brings in pull request head {}, which is `{instance}`'s commit {}",
                    short(head),
                    short(own)
                )
            };
            let target = &self.target.name;
            let tracker = &definition.name;
            return Ok(Some(format!(
                "commit {} on `{target}` {what}, and that commit holds work in `{tracker}` that `main` does not have, which `{target}` does not ship: landing it alone would leave that work out. Nothing moved. To land it with that work, run `newgit merge {instance}` from the project root, then redeliver this event (or push to `{target}` again); to land only what `{target}` has, run `newgit pull --remote {target}` from the project root, and `{instance}`'s work in `{tracker}` stays on its fork",
                short(commit)
            )));
        }
        Ok(None)
    }

    /// The pull request heads `commits` came from: what GitHub's events said,
    /// or else what GitHub says now.
    fn heads_of(&self, commits: &[String]) -> ApiResult<Heads> {
        let now = now();
        let mut heads = Vec::new();
        let mut ask = Vec::new();
        {
            let db = self.app.db.lock();
            for commit in commits {
                let known = store::heads_of(&db, &self.pull.home, &self.pull.repo, commit)?;
                if known.is_empty() {
                    ask.push(commit.clone());
                }
                heads.extend(known.into_iter().map(|head| (commit.clone(), head)));
            }
        }
        if ask.is_empty() {
            return Ok(Heads::Known(heads));
        }
        if ask.len() > ASKED_MAX {
            return Ok(Heads::GaveUp(format!(
                "whose work commit {} holds cannot be known: it would land {} commits beyond `main` that no pull request event or merge commit accounts for, and GitHub is asked about at most {ASKED_MAX}. Nothing moved: run `newgit pull --remote {}` from the project root",
                short(&self.pull.commit),
                ask.len(),
                self.pull.target
            )));
        }
        let lookup = self.pull.lookup.clone().unwrap_or_default();
        let mut unlisted = false;
        for commit in &ask {
            match self.ask_github(commit) {
                Ok(found) => {
                    unlisted |= found.is_empty();
                    heads.extend(found.into_iter().map(|head| (commit.clone(), head)));
                }
                Err(why) => {
                    // Not knowing is not a refusal: nobody is named, and the
                    // pull waits, asking again later each time.
                    let db = self.app.db.lock();
                    store::set_lookup(
                        &db,
                        self.pull.id,
                        &Lookup {
                            since: lookup.since,
                            last: now,
                            failed: Some(why),
                            failures: lookup.failures + 1,
                        },
                        now,
                    )?;
                    return Ok(Heads::Wait);
                }
            }
        }
        let since = lookup.since.unwrap_or(now);
        if unlisted && now - since < scaled(INDEX_WAIT) {
            let db = self.app.db.lock();
            store::set_lookup(
                &db,
                self.pull.id,
                &Lookup {
                    since: Some(since),
                    last: now,
                    failed: None,
                    failures: 0,
                },
                now,
            )?;
            return Ok(Heads::Wait);
        }
        Ok(Heads::Known(heads))
    }

    /// `GET /repos/{repo}/commits/{commit}/pulls`: the heads of the pull
    /// requests from this repository GitHub lists for it, merged or not (the
    /// event can beat the mark).
    fn ask_github(&self, commit: &str) -> Result<Vec<String>, String> {
        let url = format!(
            "{}/repos/{}/commits/{commit}/pulls",
            self.app.github.api, self.pull.repo
        );
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(GITHUB_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .new_agent();
        let mut request = agent
            .get(&url)
            .header("accept", "application/vnd.github+json")
            .header("user-agent", "newgit-serve");
        if let Some(token) = self.token {
            request = request.header("authorization", &format!("Bearer {token}"));
        }
        let mut answer = request
            .call()
            .map_err(|error| super::redact(&error.to_string(), self.token))?;
        if !answer.status().is_success() {
            return Err(format!(
                "GitHub answered {} for {commit}",
                answer.status().as_u16()
            ));
        }
        let pulls: serde_json::Value = answer
            .body_mut()
            .read_json()
            .map_err(|error| format!("GitHub's answer for {commit}: {error}"))?;
        Ok(pulls
            .as_array()
            .into_iter()
            .flatten()
            .filter(|pull| {
                pull["head"]["repo"]["full_name"]
                    .as_str()
                    .is_some_and(|from| from.eq_ignore_ascii_case(&self.pull.repo))
            })
            .filter_map(|pull| pull["head"]["sha"].as_str().map(str::to_owned))
            .collect())
    }

    /// Bring `walk` in, split it, and land it as a merge.
    fn land(
        &self,
        walk: &[String],
        withheld: &[TrackerDefinition],
        pin: &str,
    ) -> Result<Step, LandError> {
        let dir = self.dir;
        let commit = &self.pull.commit;
        // The views of here every commit from there stands on, composed
        // whole once: what `bring_in` puts the withheld files back from.
        let inside: BTreeSet<&String> = walk.iter().collect();
        let mut roots: BTreeSet<String> = [pin.to_owned()].into();
        for walked in walk {
            for parent in parents_of(dir, walked)? {
                if inside.contains(&parent) {
                    continue;
                }
                if let Some((_, own)) = compose::read_root(dir, &parent).map_err(core)? {
                    roots.insert(own);
                }
            }
        }
        let roots: Vec<String> = roots.into_iter().collect();
        let tips: Vec<(String, String)> = roots
            .iter()
            .enumerate()
            .map(|(n, own)| (format!("refs/mirrors/view/full/{n}"), own.clone()))
            .collect();
        let everything = |_: &str| true;
        let full = compose::compose(
            dir,
            &self.at,
            &tips,
            &ComposeOptions {
                readable: &everything,
                admit: None,
            },
        )
        .map_err(core)?;
        let full_of: HashMap<String, String> = roots
            .iter()
            .zip(&full)
            .filter_map(|(own, view)| Some((own.clone(), view.tip.clone()?)))
            .collect();
        let main_view = full_of
            .get(pin)
            .cloned()
            .ok_or_else(|| HttpError::new(409, format!("`{}` composes to nothing", self.at)))?;
        let mut known = |id: &str| -> newgit_core::error::Result<Option<String>> {
            if inside.contains(&id.to_owned()) {
                return Ok(None);
            }
            Ok(compose::read_root(dir, id)?.and_then(|(_, own)| full_of.get(&own).cloned()))
        };
        let mapped = compose::bring_in(dir, &self.target.name, walk, withheld, &mut known)
            .map_err(refusal_or)?;
        let tip = mapped
            .get(commit)
            .cloned()
            .ok_or_else(|| HttpError::internal(format!("commit {commit} did not come in")))?;
        if let Some((found, path, owner)) = compose::find_outside_git(
            dir,
            &[&tip, "--not", &main_view],
            &[],
            &self.bench.definitions,
        )
        .map_err(core)?
        {
            return Err(Refusal(format!(
                "commit {} on `{}` adds `{path}`, which belongs to `{owner}`, a tracker outside Git: its files never come in or leave through Git, so nothing was pulled",
                short(&found),
                self.target.name
            ))
            .into());
        }
        let bound = |_: &str| true;
        let audience = |tracker: &str| {
            self.bench
                .definitions
                .iter()
                .find(|definition| definition.name == tracker)
                .map(|definition| definition.audience.clone())
        };
        let split = compose::split(
            dir,
            &tip,
            &SplitOptions {
                root: &self.at,
                bound: &bound,
                audience: &audience,
                newgit_made: None,
                need_history: &|_| Ok(()),
                forks_before: &[],
            },
        )
        .map_err(|error| match error {
            SplitError::Refused(refusal) => LandError::Refused(Refusal(format!(
                "what `{}` has cannot be split by tracker: {refusal}",
                self.target.name
            ))),
            SplitError::Git(error) => LandError::Failed(core(error)),
        })?;
        if split.root == pin || is_ancestor(dir, &split.root, pin) {
            return Ok(Step::Nothing);
        }
        let Some(base_own) = git_ok(dir, &["merge-base", &split.root, pin])
            .ok()
            .filter(|found| !found.is_empty())
        else {
            return Err(Refusal(format!(
                "commit {} on `{}` shares no history with `main`",
                short(commit),
                self.target.name
            ))
            .into());
        };

        // The merge: from the root commit on `main` whose `at` is where the
        // mirror's history left it, to the mirror's commits on it.
        let (base, list, mut made) = if self.at == "root" {
            let base = merge::index_of(self.project, &base_own, None)?;
            let mut list = base.clone();
            for (tracker, commit) in &split.index {
                list.insert(tracker.clone(), commit.clone());
            }
            (base, list, Made::default())
        } else {
            self.rooted(&base_own, &split.index)?
        };
        for (tracker, commit) in &list {
            if base.get(tracker) == Some(commit) || made.has(tracker, commit) {
                continue;
            }
            self.place(tracker, commit, base.get(tracker), &mut made)?;
        }

        let begun = {
            let db = self.app.db.lock();
            let project = ProjectContext::load(self.app, &db, self.caller, &self.project.id)?;
            merge::begin(
                &project,
                &db,
                self.caller,
                "main",
                merge::Begin::new(base.clone(), list.clone()),
                "",
            )
        };
        let writes: Vec<String> = match begun {
            Ok(answer) => serde_json::from_value(answer.body["writes"].clone()).unwrap_or_default(),
            Err(error) => return Err(self.answered(error)),
        };
        // Each child's `main` follows what lands, as a member's merge asks:
        // from where it is now (the server moves it to the commit that lands
        // in its place when `main` here moved meanwhile).
        let moves: Vec<serde_json::Value> = list
            .iter()
            .filter(|(tracker, commit)| {
                tracker.as_str() != "root"
                    && base.get(*tracker) != Some(*commit)
                    && writes.contains(*tracker)
            })
            .map(|(tracker, commit)| {
                let old = self.project.tracker(tracker).branch_head("main");
                json!({ "tracker": tracker, "branch": "main", "old": old, "new": commit })
            })
            .collect();
        let moves = serde_json::Value::Array(moves);
        let finished = {
            let db = self.app.db.lock();
            let project = ProjectContext::load(self.app, &db, self.caller, &self.project.id)?;
            merge::finish(
                &project,
                db,
                self.caller,
                "main",
                &base,
                &list,
                Some(&moves),
            )
        };
        match finished {
            Ok(_) => {
                made.landed();
                Ok(Step::Landed)
            }
            Err(error) => Err(self.answered(error)),
        }
    }

    /// For a target rooted at a tracker below root: the root commit on
    /// `main` that pinned it where its history on the mirror left it, and
    /// the list landing the mirror's commits there, each tracker above it
    /// given one commit moving its pin.
    fn rooted(
        &self,
        base_own: &str,
        index: &compose::State,
    ) -> Result<(List, List, Made), LandError> {
        let dir = self.dir;
        let main_root = &self.bench.list["root"];
        // Newest first along `main`: the first that pins `at` at `base_own`.
        let mut base_root = None;
        for root in lines(&git_ok(
            dir,
            &["rev-list", "--first-parent", "-n", "2000", main_root],
        )?) {
            let state = compose::index(dir, "root", &root).map_err(core)?;
            if state.get(&self.at).map(String::as_str) == Some(base_own) {
                base_root = Some(root);
                break;
            }
        }
        let Some(base_root) = base_root else {
            return Err(Refusal(format!(
                "`{}` on `{}` left `{}` at {}, which no recent root commit on `main` pins: run `newgit pull --remote {}` from the project root",
                self.pull.branch,
                self.target.name,
                self.at,
                short(base_own),
                self.target.name
            ))
            .into());
        };
        let base = merge::index_of(self.project, &base_root, None)?;
        let mut list = base.clone();
        for (tracker, commit) in index {
            list.insert(tracker.clone(), commit.clone());
        }
        let mut objects = Objects::open(dir).map_err(core)?;
        let when = now();
        let by = format!(
            "{login} <{login}@users.noreply.github.com> {when} +0000",
            login = self.pull.pusher
        );
        let message = format!(
            "Pull `{}` into `{}`\n",
            self.target.name,
            self.at.replace("__", "/")
        );
        let mut child = self.at.clone();
        for parent in lineage(&self.at).into_iter().skip(1) {
            let old = &base[parent];
            let old_commit = objects.commit(old).map_err(core)?;
            let name = if parent == "root" {
                child.clone()
            } else {
                child
                    .strip_prefix(&format!("{parent}__"))
                    .unwrap_or(&child)
                    .to_owned()
            };
            let tree = objects
                .set_path(
                    &old_commit.tree,
                    format!(".newgit/pins/{name}"),
                    Some((GITLINK_MODE.to_vec(), list[&child].clone())),
                )
                .map_err(core)?;
            let made = objects.make_commit(
                &tree,
                std::slice::from_ref(old),
                by.as_bytes(),
                by.as_bytes(),
                b"",
                message.as_bytes(),
            );
            list.insert(parent.to_owned(), made);
            child = parent.to_owned();
        }
        objects.flush().map_err(core)?;
        Ok((base, list, Made::default()))
    }

    /// `commit` and what it adds over `old` (and over its tracker's branches)
    /// into the pusher's landing for `tracker`: the one place a merge's
    /// commits wait before the landing puts them in a store.
    fn place(
        &self,
        tracker: &str,
        commit: &str,
        old: Option<&String>,
        made: &mut Made,
    ) -> ApiResult<()> {
        let mut revs = format!("{commit}\n");
        if let Some(old) = old {
            revs.push_str(&format!("^{old}\n"));
        }
        for (_, head) in self.project.tracker(tracker).list_refs("refs/heads/")? {
            revs.push_str(&format!("^{head}\n"));
        }
        let pack = crate::repos::git_timed(
            self.dir,
            &["pack-objects", "--revs", "--stdout", "-q"],
            Some(revs.as_bytes()),
            crate::server_merge::GIT_LIMIT,
        )?;
        if !pack.status.success() {
            return Err(HttpError::internal(format!(
                "packing {commit} for `{tracker}`: {}",
                String::from_utf8_lossy(&pack.stderr).trim()
            )));
        }
        let landing = self
            .project
            .tracker(tracker)
            .open_landing(&self.pull.pusher)?;
        let indexed = crate::repos::git_timed(
            &landing,
            &["index-pack", "--stdin", "--fix-thin"],
            Some(&pack.stdout),
            crate::server_merge::GIT_LIMIT,
        )?;
        if !indexed.status.success() {
            return Err(HttpError::internal(format!(
                "indexing {commit} into `{tracker}`'s landing: {}",
                String::from_utf8_lossy(&indexed.stderr).trim()
            )));
        }
        made.wait(&landing, tracker, commit)
    }

    /// What a refusal from `merge` means for a pull: the same refusal, with
    /// what to do in terms of the mirror. A busy or failing server is a
    /// failed attempt, retried; `main` moving under it starts it again.
    fn answered(&self, error: HttpError) -> LandError {
        let target = &self.target.name;
        let branch = &self.pull.branch;
        match error.status {
            409 if error.message.contains("merge onto what it is now")
                || error.message.contains("start the merge again") =>
            {
                LandError::Again
            }
            409 if error.message.starts_with("this merge conflicts") => {
                let files = error
                    .message
                    .split_once(" since your base, in ")
                    .and_then(|(_, rest)| rest.rsplit_once(": bring"))
                    .map_or("some files", |(files, _)| files);
                LandError::Refused(Refusal(format!(
                    "`main` and `{branch}` on `{target}` both moved, and merging them conflicts in {files}. Nothing moved: merge `main` into `{branch}` on `{target}` and the push brings it in, or run `newgit pull --remote {target}` from the project root, resolve, and `newgit merge main`"
                )))
            }
            400 | 403 | 409 => LandError::Refused(Refusal(format!(
                "landing it as `{}`'s merge was refused: {}. Nothing moved",
                self.pull.pusher,
                error.message.trim_end_matches('.')
            ))),
            _ => LandError::Failed(error),
        }
    }
}

/// A refusal reached while landing: the pull's answer, not a failure.
struct Refusal(String);

enum LandError {
    Refused(Refusal),
    Again,
    Failed(HttpError),
}

impl From<Refusal> for LandError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<HttpError> for LandError {
    fn from(error: HttpError) -> Self {
        Self::Failed(error)
    }
}

/// `bring_in`'s refusals are answers (a commit touching a withheld mount,
/// two lines of a withheld tracker), the rest failures.
fn refusal_or(error: newgit_core::error::NewgitError) -> LandError {
    match error {
        newgit_core::error::NewgitError::SplitRefused(why)
        | newgit_core::error::NewgitError::Unsupported(why) => LandError::Refused(Refusal(why)),
        other => LandError::Failed(core(other)),
    }
}

enum Heads {
    /// `(commit, head)`: each commit asked about, with each pull request
    /// head it came from.
    Known(Vec<(String, String)>),
    Wait,
    GaveUp(String),
}

fn parents_of(dir: &Utf8Path, commit: &str) -> ApiResult<Vec<String>> {
    Ok(
        git_ok(dir, &["rev-list", "--no-walk", "--parents", commit])?
            .split_whitespace()
            .skip(1)
            .map(str::to_owned)
            .collect(),
    )
}

fn lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_double_up_to_ten_minutes() {
        assert_eq!(retry_delay(1, RETRY_FIRST), 15);
        assert_eq!(retry_delay(2, RETRY_FIRST), 30);
        assert_eq!(retry_delay(5, RETRY_FIRST), 240);
        assert_eq!(retry_delay(9, RETRY_FIRST), RETRY_MAX);
        assert_eq!(retry_delay(1, ASK_AGAIN), 30);
        assert_eq!(retry_delay(30, ASK_AGAIN), RETRY_MAX);
    }
}
