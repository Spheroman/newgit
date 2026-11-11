//! Encrypting trackers outside Git to their audience (#338): who a capture
//! is encrypted to, checking a rev against that before it is published, and
//! publishing `main` without overwriting what this machine has not seen.
//! The age and Git side is in [`crate::crypt`] and [`crate::tracker_repo`].
//!
//! Recipients are computed, never read back. They are the members the
//! audience names, as the remote lists them (the remote is the membership
//! authority), by the keys GitHub publishes for each. This machine keeps the
//! last list it read in `.newgit/local/audience.json`, so a capture with no
//! network still encrypts to someone; nothing is published until the list
//! has been read again and the rev checked against it.

use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

use super::{BranchManager, tracker_repo};
use crate::branch::BranchInstance;
use crate::crypt::{Recipient, Recipients};
use crate::error::{NewgitError, Result};
use crate::remote::{Acting, Scope, basic_auth_header};
use crate::source::GitSource;
use crate::tracker::{Storage, TrackerDefinition};
use crate::tracker_repo::Branch;

/// `main` of one tracker, encrypted again to its audience as it is now.
#[derive(Debug, Clone)]
pub struct Reencrypted {
    pub tracker: String,
    /// Whether it has a `main` at all (on the remote or here).
    pub merged: bool,
    /// The new `main`, or `None` when `main` was already encrypted to
    /// exactly these recipients (or there is none, or it failed).
    pub rev: Option<String>,
    /// Who it is encrypted to now.
    pub recipients: Recipients,
    /// The set this machine last encrypted it to, when that was another.
    pub previous: Option<Recipients>,
    /// Members of its audience with no key age can encrypt to.
    pub keyless: Vec<String>,
    /// Whether the new `main` reached the remote.
    pub published: bool,
    /// Why it was not re-encrypted, when it was not.
    pub error: Option<String>,
}

/// Who a tracker's audience is right now.
#[derive(Debug, Clone)]
pub(super) struct Audience {
    pub recipients: Recipients,
    pub keyless: Vec<String>,
}

/// Whether the member list has to be read from the remote now, or may come
/// from what this machine last read when the remote cannot be reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Fresh {
    /// Before anything is published.
    Required,
    /// For a capture, which publishes nothing by itself.
    Preferred,
}

/// What this machine last read of the project's members and their keys,
/// and the set it last encrypted each tracker to. Written only by this
/// machine, from the remote and GitHub; never from a tracker's tree.
#[derive(Debug, Default, Serialize, Deserialize)]
struct AudienceCache {
    #[serde(default)]
    members: Option<Vec<String>>,
    #[serde(default)]
    me: Option<String>,
    /// Usable keys by login.
    #[serde(default)]
    keys: BTreeMap<String, Vec<String>>,
    /// The recipients each tracker was last encrypted to here, as
    /// [`Recipients::to_text`] writes them.
    #[serde(default)]
    used: BTreeMap<String, String>,
}

/// Who `recipients` are, each login with its keys' fingerprints, and loudly
/// what changed when `previous` was another set: what is printed wherever a
/// tracker's recipients are.
pub fn describe_recipients(
    tracker: &str,
    recipients: &Recipients,
    previous: Option<&Recipients>,
) -> String {
    let listing = recipients.listing("    ");
    match previous {
        Some(previous) => format!(
            "  CHANGED: `{tracker}` is now encrypted to a different set than this machine last \
             used. Check each key against `ssh-keygen -lf` on its owner's machine:\n{}\n  now:\n{listing}",
            recipients.changes_from(previous, "    ")
        ),
        None => format!("  `{tracker}` is encrypted to:\n{listing}"),
    }
}

impl BranchManager {
    fn audience_cache_path(&self) -> Utf8PathBuf {
        let store = &self.store.paths().tracker_store;
        store
            .parent()
            .map_or_else(|| store.clone(), camino::Utf8Path::to_path_buf)
            .join("audience.json")
    }

    fn load_audience_cache(&self) -> AudienceCache {
        std::fs::read_to_string(self.audience_cache_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save_audience_cache(&self, cache: &AudienceCache) -> Result<()> {
        let text = serde_json::to_string_pretty(cache).map_err(|error| NewgitError::Remote {
            status: 0,
            message: format!("could not write the audience cache: {error}"),
        })?;
        crate::remote::write_private(&self.audience_cache_path(), &text)
    }

    /// Forget the member lists read in this process: after `members add`
    /// or `remove` changed them.
    pub(super) fn forget_audiences(&self) {
        self.audiences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Who `definition`'s audience is: its logins from the remote (the
    /// members for `project-devs`, the caller for `user`, the one named for
    /// `user:<login>`), each by its usable GitHub keys. Read once per
    /// process. With [`Fresh::Preferred`], what this machine last read when
    /// the remote or GitHub cannot be reached. Refused when nobody in it
    /// has a key: content encrypted to no one would be lost, and content
    /// left in plaintext would be a lie.
    pub(super) fn audience_of(
        &self,
        definition: &TrackerDefinition,
        branch: Option<&BranchInstance>,
        fresh: Fresh,
    ) -> Result<Audience> {
        let failed = |message: String| NewgitError::Encryption {
            tracker: definition.name.clone(),
            message,
        };
        let memo = self
            .audiences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&definition.audience)
            .cloned();
        let audience = match memo {
            Some(audience) => audience,
            None => match self.read_audience(definition, branch) {
                Ok(audience) => {
                    self.audiences
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(definition.audience.clone(), audience.clone());
                    audience
                }
                Err(error) if fresh == Fresh::Preferred => {
                    let Some(audience) = self.cached_audience(definition) else {
                        return Err(error);
                    };
                    eprintln!(
                        "warning: could not read who `{}` is for ({error}); encrypting to the \
                         members and keys this machine last read. They are read again, and \
                         the rev encrypted again if they changed, before anything is sent",
                        definition.name
                    );
                    audience
                }
                Err(error) => return Err(error),
            },
        };
        if audience.recipients.is_empty() {
            return Err(failed(format!(
                "nobody in its audience ({}) has a key it can be encrypted to: newgit encrypts \
                 to the ssh-ed25519 and ssh-rsa keys GitHub publishes at \
                 https://github.com/<login>.keys. Add one there; if a server has to read this \
                 tracker, its audience is `public`",
                audience.keyless.join(", ")
            )));
        }
        Ok(audience)
    }

    /// [`Self::audience_of`] from the remote and GitHub, remembered here.
    fn read_audience(
        &self,
        definition: &TrackerDefinition,
        branch: Option<&BranchInstance>,
    ) -> Result<Audience> {
        let link = self
            .remote
            .as_ref()
            .ok_or_else(|| NewgitError::Encryption {
                tracker: definition.name.clone(),
                message: "the project has no remote to read its members from".to_owned(),
            })?;
        let acting = match branch {
            Some(branch) => Acting::Workspace(&branch.workspace_path),
            None => Acting::Login,
        };
        let mut cache = self.load_audience_cache();
        let logins: Vec<String> = match definition.audience.as_str() {
            "project-devs" => {
                let members: Vec<String> = link
                    .connect(acting)?
                    .project(&link.project)?
                    .members
                    .into_iter()
                    .map(|member| member.login.to_lowercase())
                    .collect();
                cache.members = Some(members.clone());
                members
            }
            "user" => {
                let me = link.connect(acting)?.me()?.login.to_lowercase();
                cache.me = Some(me.clone());
                vec![me]
            }
            other => vec![other.strip_prefix("user:").unwrap_or(other).to_lowercase()],
        };
        let mut keys = BTreeMap::new();
        for login in &logins {
            keys.insert(login.clone(), self.usable_keys(login)?);
        }
        cache.keys.extend(keys.clone());
        // Remembering is a convenience for the next capture offline; not
        // being able to is no reason to fail this one.
        let _ = self.save_audience_cache(&cache);
        Ok(audience_from(&logins, &keys))
    }

    /// [`Self::audience_of`] from what this machine last read, if it read
    /// this audience before.
    fn cached_audience(&self, definition: &TrackerDefinition) -> Option<Audience> {
        let cache = self.load_audience_cache();
        let logins = match definition.audience.as_str() {
            "project-devs" => cache.members.clone()?,
            "user" => vec![cache.me.clone()?],
            other => vec![other.strip_prefix("user:").unwrap_or(other).to_lowercase()],
        };
        if !logins.iter().all(|login| cache.keys.contains_key(login)) {
            return None;
        }
        Some(audience_from(&logins, &cache.keys))
    }

    /// Remember `recipients` as what `tracker` was last encrypted to here,
    /// and return what it was before when that was another set (an empty
    /// set the first time).
    fn note_used(&self, tracker: &str, recipients: &Recipients) -> Option<Recipients> {
        let mut cache = self.load_audience_cache();
        let previous = cache
            .used
            .get(tracker)
            .and_then(|text| Recipients::parse(text));
        if previous.as_ref() == Some(recipients) {
            return None;
        }
        cache.used.insert(tracker.to_owned(), recipients.to_text());
        let _ = self.save_audience_cache(&cache);
        Some(previous.unwrap_or_default())
    }

    /// Say who `tracker` is now encrypted to, when that is not the set this
    /// machine last used for it (and the first time).
    fn announce(&self, tracker: &str, audience: &Audience) {
        if let Some(previous) = self.note_used(tracker, &audience.recipients) {
            let previous = (!previous.is_empty()).then_some(&previous);
            eprintln!(
                "{}",
                describe_recipients(tracker, &audience.recipients, previous)
            );
            if !audience.keyless.is_empty() {
                eprintln!(
                    "  warning: {} {} no ssh-ed25519 or ssh-rsa key on GitHub, so they cannot \
                     decrypt `{tracker}` until they add one",
                    audience.keyless.join(", "),
                    if audience.keyless.len() == 1 {
                        "has"
                    } else {
                        "have"
                    },
                );
            }
        }
    }

    /// What a capture of `definition` is encrypted to, or `None` when it is
    /// stored as plaintext (see
    /// [`TrackerDefinition::encrypted_to_audience`]; a project with no
    /// remote has nowhere for it to go, so nothing to encrypt for).
    pub(super) fn seal_for(
        &self,
        definition: &TrackerDefinition,
        branch: Option<&BranchInstance>,
    ) -> Result<Option<Recipients>> {
        if !definition.encrypted_to_audience() || self.remote.is_none() {
            return Ok(None);
        }
        let audience = self.audience_of(definition, branch, Fresh::Preferred)?;
        self.announce(&definition.name, &audience);
        Ok(Some(audience.recipients))
    }

    /// `rev`, or `rev` encrypted again (a commit on `on`) when its files
    /// are not encrypted to exactly the audience as the remote has it now:
    /// what every rev of an encrypted tracker goes through before it is
    /// published.
    pub(super) fn sealed_to_current(
        &self,
        definition: &TrackerDefinition,
        rev: &str,
        branch: Option<&BranchInstance>,
        on: Branch<'_>,
        message: &str,
    ) -> Result<String> {
        if !definition.encrypted_to_audience() || self.remote.is_none() {
            return Ok(rev.to_owned());
        }
        let audience = self.audience_of(definition, branch, Fresh::Required)?;
        let lane = self.lane(&definition.name);
        if lane.sealed_to(rev, &audience.recipients)? {
            return Ok(rev.to_owned());
        }
        self.announce(&definition.name, &audience);
        Ok(lane.reseal(rev, &audience.recipients, on, message)?.rev)
    }

    /// Encrypt every shared tracker's `main` again, to its audience's
    /// members as the remote lists them now: what `members add` and
    /// `members remove` do. Each tracker's `main` is fetched from the remote
    /// first, so a merge made elsewhere is what gets re-encrypted rather
    /// than overwritten. One tracker failing does not stop the rest; each
    /// says how it went.
    pub fn reencrypt_for_audience(&self) -> Vec<Reencrypted> {
        self.forget_audiences();
        if self.remote.is_none() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for definition in &self.trackers {
            // A per-user tracker is its owner's alone, and `user:<login>`
            // is one person whatever the membership.
            if !definition.encrypted_to_audience() || definition.audience != "project-devs" {
                continue;
            }
            let mut entry = Reencrypted {
                tracker: definition.name.clone(),
                merged: false,
                rev: None,
                recipients: Recipients::default(),
                previous: None,
                keyless: Vec::new(),
                published: false,
                error: None,
            };
            if let Err(error) = self.reencrypt_main(definition, &mut entry) {
                entry.error = Some(error.to_string());
            }
            out.push(entry);
        }
        out
    }

    fn reencrypt_main(
        &self,
        definition: &TrackerDefinition,
        entry: &mut Reencrypted,
    ) -> Result<()> {
        let lane = self.lane(&definition.name);
        let main = match self.fetch_remote_main(definition, Acting::Login)? {
            Some(theirs) => Some(lane.adopt_main(&theirs)?),
            // Never published: this machine's own, if it has one.
            None => lane.main(),
        };
        let audience = self.audience_of(definition, None, Fresh::Required)?;
        entry.recipients = audience.recipients.clone();
        entry.keyless = audience.keyless.clone();
        entry.previous = self
            .note_used(&definition.name, &audience.recipients)
            .filter(|previous| !previous.is_empty());
        let Some(main) = main else {
            return Ok(());
        };
        entry.merged = true;
        if lane.sealed_to(&main, &audience.recipients)? {
            return Ok(());
        }
        let outcome = lane.reseal(
            &main,
            &audience.recipients,
            Branch::Main,
            &format!(
                "{}: encrypted to its audience as it is now",
                definition.name
            ),
        )?;
        entry.published = self
            .publish_main(definition, &outcome.rev, Acting::Login)?
            .is_some();
        lane.set_main(&outcome.rev)?;
        entry.rev = Some(outcome.rev);
        Ok(())
    }

    /// The shared (encrypted) trackers someone without a usable key could
    /// not read, by name.
    pub fn encrypted_trackers(&self) -> Vec<String> {
        self.trackers
            .iter()
            .filter(|definition| definition.encrypted_to_audience())
            .map(|definition| definition.name.clone())
            .collect()
    }

    /// The usable keys GitHub publishes for `login`.
    pub fn usable_keys(&self, login: &str) -> Result<Vec<String>> {
        Ok(self
            .keys
            .keys(login)?
            .iter()
            .filter_map(|line| crate::crypt::usable_key(line))
            .collect())
    }

    /// The remote's `main` of `definition`, fetched into this store (not
    /// made this machine's `main`): its commit, or `None` when the remote
    /// has none.
    fn fetch_remote_main(
        &self,
        definition: &TrackerDefinition,
        acting: Acting<'_>,
    ) -> Result<Option<String>> {
        let Some(link) = &self.remote else {
            return Ok(None);
        };
        let remote = link.connect(acting)?;
        let repo = tracker_repo(&link.project, definition, "");
        let credential = match remote.credential(&link.project, &repo, Scope::Read) {
            Ok(credential) => credential,
            // Never published: there is nothing there to bring.
            Err(NewgitError::Remote { status: 404, .. }) => return Ok(None),
            Err(error) => return Err(error),
        };
        let store = self.store.paths().tracker_store.clone();
        crate::tracker_repo::ensure_store(&store)?;
        let landing = format!("refs/newgit/remote-main/{}", definition.name);
        let _ = crate::source::run_git_env(
            &[
                &format!("--git-dir={store}"),
                "update-ref",
                "-d",
                &format!("{landing}/main"),
            ],
            &[],
        );
        GitSource::fetch_refs(
            &[&format!("--git-dir={store}")],
            credential.remote.as_deref().unwrap_or_default(),
            &[format!("+refs/heads/*:{landing}/*")],
            Some(&basic_auth_header(&credential)),
        )?;
        Ok(crate::source::run_git_env(
            &[
                &format!("--git-dir={store}"),
                "rev-parse",
                "-q",
                "--verify",
                &format!("{landing}/main^{{commit}}"),
            ],
            &[],
        )
        .ok()
        .map(|commit| commit.trim().to_owned()))
    }

    /// Move the remote's `main` for a shared tracker whose synced state
    /// lives there (`storage = "remote"`) to `rev`. A checkpoint's sync
    /// pushes only such a tracker's pins and instance branches, never its
    /// `main`, so this is the one way that `main` moves.
    ///
    /// On the remote a tracker repo's `main` is its branch `main`, as that
    /// of a tracker in Git is: the repo's default branch, what the Worker's
    /// `LANDING_PAGE` reads, and what anything reading a tracker by branch
    /// gets. A per-user tracker's `main` already rides every checkpoint.
    ///
    /// Never over what this machine has not seen: the remote's `main` is
    /// fetched first, and `rev` must contain it, contain a rev it only
    /// re-encrypts, or (the one history replacement) be the sealed history
    /// that started in place of a plaintext one containing it. Otherwise it
    /// is refused, and the remote's `main` is made this machine's so
    /// `newgit tracker pull` brings it. The push is leased on the `main` it
    /// checked, so a merge landing in between is not overwritten either.
    pub(super) fn publish_main(
        &self,
        definition: &TrackerDefinition,
        rev: &str,
        acting: Acting<'_>,
    ) -> Result<Option<String>> {
        if definition.storage != Storage::Remote || definition.audience == "user" {
            return Ok(None);
        }
        let Some(link) = &self.remote else {
            return Ok(None);
        };
        self.refuse_plaintext(definition, rev)?;
        let lane = self.lane(&definition.name);
        let theirs = self.fetch_remote_main(definition, acting)?;
        if let Some(theirs) = &theirs
            && !self.incorporates(definition, rev, theirs)?
        {
            lane.adopt_main(theirs)?;
            return Err(NewgitError::Unsupported(format!(
                "the remote's `main` of `{name}` has changes {rev} does not include (merged \
                 from somewhere else since this machine last saw it), so nothing was \
                 published: that would overwrite them. Bring them in with `newgit tracker pull \
                 {name}` in the instance, then capture and merge again",
                name = definition.name,
            )));
        }
        let remote = link.connect(acting)?;
        let repo = tracker_repo(&link.project, definition, "");
        let credential = remote.credential(&link.project, &repo, Scope::Write)?;
        let url = credential.remote.clone().unwrap_or_default();
        let auth = basic_auth_header(&credential);
        let location = format!("--git-dir={}", self.store.paths().tracker_store);
        let pin = format!("refs/trackers/{}/pins/{rev}", definition.name);
        GitSource::push_refs(&[&location], &url, &[format!("{pin}:{pin}")], Some(&auth))?;
        GitSource::push_leased(
            &[&location],
            &url,
            &lane.full(rev)?,
            "refs/heads/main",
            theirs.as_deref(),
            Some(&auth),
        )?;
        Ok(Some(link.project.clone()))
    }

    /// Whether publishing `rev` as `main` keeps everything in `theirs`.
    fn incorporates(
        &self,
        definition: &TrackerDefinition,
        rev: &str,
        theirs: &str,
    ) -> Result<bool> {
        let lane = self.lane(&definition.name);
        if lane.contains(rev, theirs)? {
            return Ok(true);
        }
        // The one replacement: a sealed history started in place of a
        // plaintext one, over a plaintext `main` that one contains.
        if definition.encrypted_to_audience()
            && lane.plaintext_in_history(theirs)?.is_some()
            && let Some(replaced) = lane.replaces(rev)?
        {
            return lane.contains(&replaced, theirs);
        }
        // Commits that only re-encrypt (the same content as their parent,
        // as `members add` and `remove` make), back to one `rev` contains.
        let mut walk = theirs.to_owned();
        for _ in 0..64 {
            let Some(parent) = lane.parent_of(&walk) else {
                return Ok(false);
            };
            if !lane.same_content(&walk, &parent).unwrap_or(false) {
                return Ok(false);
            }
            if lane.contains(rev, &parent)? {
                return Ok(true);
            }
            walk = parent;
        }
        Ok(false)
    }

    /// Refuse to send a rev of an encrypted tracker whose history holds a
    /// file that is not age ciphertext: the backstop on every path where
    /// tracker content leaves this machine, whatever wrote the rev. Every
    /// blob is checked, not the record.
    pub(super) fn refuse_plaintext(&self, definition: &TrackerDefinition, rev: &str) -> Result<()> {
        if !definition.encrypted_to_audience() {
            return Ok(());
        }
        let Some(found) = self.lane(&definition.name).plaintext_in_history(rev)? else {
            return Ok(());
        };
        Err(NewgitError::Encryption {
            tracker: definition.name.clone(),
            message: format!(
                "rev {rev} would send {found} in plaintext, so it is not sent; capture the \
                 tracker again (`newgit checkpoint` or `newgit tracker capture {}`) to encrypt \
                 it",
                definition.name
            ),
        })
    }

    /// Refuse to send `rev` unless it is encrypted to exactly the audience
    /// as the remote has it now: what checkpoint sync checks, after the
    /// checkpoint encrypted anything stale again.
    pub(super) fn refuse_stale(
        &self,
        definition: &TrackerDefinition,
        rev: &str,
        branch: &BranchInstance,
    ) -> Result<()> {
        if !definition.encrypted_to_audience() {
            return Ok(());
        }
        let audience = self.audience_of(definition, Some(branch), Fresh::Required)?;
        if self
            .lane(&definition.name)
            .sealed_to(rev, &audience.recipients)?
        {
            return Ok(());
        }
        Err(NewgitError::Encryption {
            tracker: definition.name.clone(),
            message: format!(
                "rev {rev} is not encrypted to the audience as it is now ({}), so it is not \
                 sent; `newgit checkpoint` again encrypts it to them",
                audience.recipients.logins()
            ),
        })
    }
}

/// An audience from its logins and each one's usable keys.
fn audience_from(logins: &[String], keys: &BTreeMap<String, Vec<String>>) -> Audience {
    let mut recipients = Recipients::default();
    let mut keyless = Vec::new();
    for login in logins {
        let usable = keys.get(login).cloned().unwrap_or_default();
        if usable.is_empty() {
            keyless.push(login.clone());
        }
        for key in usable {
            recipients.insert(Recipient {
                who: format!("user:{login}"),
                key,
            });
        }
    }
    Audience {
        recipients,
        keyless,
    }
}
