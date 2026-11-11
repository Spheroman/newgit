//! Encrypting trackers outside Git to their audience (#338).
//!
//! A tracker outside Git that syncs (`storage = "remote"`) and whose
//! audience is not `public` is encrypted on this machine, file by file, with
//! [age], before anything leaves it. Each file is encrypted whole and stored
//! as an ordinary blob in the tracker's history, so Git's object model is
//! untouched.
//!
//! **Who it is encrypted to is computed, never read back.** The recipients
//! are the members the audience names, as the remote lists them now, by the
//! `ssh-ed25519` and `ssh-rsa` keys GitHub publishes for each of them
//! (`https://github.com/<login>.keys`). Nothing else: not the record of a
//! rev, not `main`, not anything a remote serves. The list is written into
//! each sealed rev at [`RECIPIENTS_PATH`] for people to read, and no code
//! takes a recipient from it. Whether a rev is sealed to a set is decided by
//! the ciphertext itself: each age file's header names the current keys, by
//! a tag of every SSH key it is encrypted to ([`stanzas`]). That identifies
//! the keys; it does not prove them (the tag is public, and the header's MAC
//! is not checked without a key), so it decides what to encrypt again, never
//! who may read.
//!
//! What stays readable without a key: file names, approximate sizes, link
//! targets (a link is stored as a link), commit messages, authors, and
//! dates. Only file contents are encrypted, and nothing is signed: anyone
//! can encrypt to public keys, so a remote can replace a file with content
//! that decrypts cleanly.
//!
//! The workspace always holds plaintext. This machine also keeps the
//! plaintext of every blob it encrypted or decrypted in a cache under
//! `.newgit/local/` (never pushed, the same exposure as the workspace), so
//! comparing a capture against its parent does not need a key every time.
//!
//! [age]: https://age-encryption.org

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read as _, Write as _};
use std::str::FromStr as _;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use base64::Engine as _;
use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest as _, Sha256};

use crate::error::{NewgitError, Result};

/// Where a sealed rev's tree records its recipients, for people to read.
/// Under `.newgit/`, which no tracker outside Git may own, so it can never
/// be one of its files. Never read back as recipients.
pub const RECIPIENTS_PATH: &str = ".newgit/recipients";

/// Where GitHub's published keys come from instead: an `https://` base, or
/// a `file://` directory of `<login>.keys`. Tests point it at a directory so
/// they never touch the network.
pub const KEYS_URL_ENV: &str = "NEWGIT_KEYS_URL";

/// Private keys to decrypt with, before `~/.ssh/id_ed25519` and
/// `~/.ssh/id_rsa`: a path, or several separated by `:`, each an SSH private
/// key (or an age identity file). For a key without a passphrase, added to
/// the developer's GitHub account, so agents never meet a passphrase prompt.
pub const IDENTITY_ENV: &str = "NEWGIT_IDENTITY";

const DEFAULT_KEYS_URL: &str = "https://github.com";

/// The first line of every age file.
const AGE_HEADER: &[u8] = b"age-encryption.org/v1\n";

/// One recipient: who (`user:<login>`), and the public key it was encrypted
/// to (`ssh-ed25519 AAAA…` or `ssh-rsa AAAA…`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Recipient {
    pub who: String,
    pub key: String,
}

impl Recipient {
    /// The key's SHA-256 fingerprint as `ssh-keygen -l` prints it:
    /// `SHA256:<base64>`.
    pub fn fingerprint(&self) -> String {
        match key_blob(&self.key) {
            Some(blob) => format!(
                "SHA256:{}",
                base64::engine::general_purpose::STANDARD_NO_PAD.encode(Sha256::digest(&blob))
            ),
            None => "(unreadable key)".to_owned(),
        }
    }

    /// What an age header names this key by: its type and a tag of the key
    /// (the first four bytes of its SHA-256), as age writes them.
    fn stanza(&self) -> Option<(String, String)> {
        let kind = self.key.split_whitespace().next()?;
        let blob = key_blob(&self.key)?;
        let tag =
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(&Sha256::digest(&blob)[..4]);
        Some((kind.to_owned(), tag))
    }
}

/// The wire-format key an OpenSSH public key line carries.
fn key_blob(key: &str) -> Option<Vec<u8>> {
    let body = key.split_whitespace().nth(1)?;
    base64::engine::general_purpose::STANDARD.decode(body).ok()
}

/// Everyone a capture is encrypted to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recipients(BTreeSet<Recipient>);

impl Recipients {
    pub fn new(recipients: impl IntoIterator<Item = Recipient>) -> Self {
        Self(recipients.into_iter().collect())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Recipient> {
        self.0.iter()
    }

    pub fn insert(&mut self, recipient: Recipient) {
        self.0.insert(recipient);
    }

    /// The people, each once: `ana, jack`.
    pub fn logins(&self) -> String {
        let logins: BTreeSet<&str> = self
            .0
            .iter()
            .map(|r| r.who.strip_prefix("user:").unwrap_or(&r.who))
            .collect();
        logins.into_iter().collect::<Vec<_>>().join(", ")
    }

    /// Each login with its keys' fingerprints, one per line, indented by
    /// `indent`: what a person checks against `ssh-keygen -lf`.
    pub fn listing(&self, indent: &str) -> String {
        let mut by_login: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for recipient in &self.0 {
            let kind = recipient.key.split_whitespace().next().unwrap_or_default();
            by_login
                .entry(
                    recipient
                        .who
                        .strip_prefix("user:")
                        .unwrap_or(&recipient.who),
                )
                .or_default()
                .push(format!("{} ({kind})", recipient.fingerprint()));
        }
        by_login
            .into_iter()
            .map(|(login, keys)| format!("{indent}{login}: {}", keys.join(", ")))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// What changed from `previous` to these, one line each: `+ carol
    /// SHA256:…`, `- bob SHA256:…`.
    pub fn changes_from(&self, previous: &Self, indent: &str) -> String {
        let mut lines = Vec::new();
        for recipient in self.0.difference(&previous.0) {
            lines.push(format!(
                "{indent}+ {} {}",
                recipient
                    .who
                    .strip_prefix("user:")
                    .unwrap_or(&recipient.who),
                recipient.fingerprint()
            ));
        }
        for recipient in previous.0.difference(&self.0) {
            lines.push(format!(
                "{indent}- {} {}",
                recipient
                    .who
                    .strip_prefix("user:")
                    .unwrap_or(&recipient.who),
                recipient.fingerprint()
            ));
        }
        lines.join("\n")
    }

    /// The record written at [`RECIPIENTS_PATH`], and how this machine
    /// remembers the last set it used: one `<who> <key>` per line.
    pub fn to_text(&self) -> String {
        let mut text = String::from(
            "# Every other file in this rev is encrypted with age (https://age-encryption.org)\n\
             # to each key below. Written by newgit (#338) for people to read: newgit never\n\
             # takes recipients from it, and nothing here is signed.\n",
        );
        for recipient in &self.0 {
            text.push_str(&format!("{} {}\n", recipient.who, recipient.key));
        }
        text
    }

    /// Lines written by [`Self::to_text`], back. Only for what this machine
    /// wrote itself (its memory of the last set it used), never a tree's.
    pub fn parse(text: &str) -> Option<Self> {
        let mut set = BTreeSet::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (who, key) = line.split_once(' ')?;
            set.insert(Recipient {
                who: who.to_owned(),
                key: key.trim().to_owned(),
            });
        }
        Some(Self(set))
    }

    /// What an age file encrypted to exactly these names in its header.
    fn stanzas(&self) -> BTreeSet<(String, String)> {
        self.0.iter().filter_map(Recipient::stanza).collect()
    }

    /// Whether `ciphertext` is an age file whose header names exactly these
    /// keys: every one of them, and nothing else (no X25519, no passphrase,
    /// no SSH key not listed here). Names, not proves: see the module doc.
    pub fn sealed(&self, ciphertext: &[u8]) -> bool {
        stanzas(ciphertext).is_some_and(|found| found == self.stanzas())
    }

    /// `plaintext`, encrypted to every recipient: an age file, binary.
    pub fn encrypt(&self, tracker: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
        let failed = |message: String| NewgitError::Encryption {
            tracker: tracker.to_owned(),
            message,
        };
        let mut keys: Vec<Box<dyn age::Recipient + Send>> = Vec::new();
        for recipient in &self.0 {
            let key = age::ssh::Recipient::from_str(&recipient.key).map_err(|_| {
                failed(format!(
                    "the key of {} is not one age can encrypt to",
                    recipient.who
                ))
            })?;
            keys.push(Box::new(key));
        }
        if keys.is_empty() {
            return Err(failed("it has no recipients to encrypt to".to_owned()));
        }
        let encryptor = age::Encryptor::with_recipients(keys.iter().map(|key| key.as_ref() as _))
            .map_err(|error| failed(error.to_string()))?;
        let mut out = Vec::with_capacity(plaintext.len() + 256);
        let mut writer = encryptor
            .wrap_output(&mut out)
            .map_err(|error| failed(error.to_string()))?;
        writer
            .write_all(plaintext)
            .and_then(|()| writer.finish().map(drop))
            .map_err(|error| failed(error.to_string()))?;
        Ok(out)
    }
}

/// Whether `bytes` is an age file (its first line), as every file of a
/// sealed rev is.
pub fn is_age(bytes: &[u8]) -> bool {
    bytes.starts_with(AGE_HEADER)
}

/// What an age file's header says it is encrypted to: each stanza's type
/// and first argument (an SSH key's tag), grease left out. `None` when it
/// is not an age file.
pub fn stanzas(ciphertext: &[u8]) -> Option<BTreeSet<(String, String)>> {
    let rest = ciphertext.strip_prefix(AGE_HEADER)?;
    let mut found = BTreeSet::new();
    for line in rest.split(|byte| *byte == b'\n') {
        if line.starts_with(b"---") {
            return Some(found);
        }
        let Some(stanza) = line.strip_prefix(b"-> ") else {
            continue;
        };
        let stanza = std::str::from_utf8(stanza).ok()?;
        let mut fields = stanza.split(' ');
        let kind = fields.next()?.to_owned();
        if kind.ends_with("-grease") {
            continue;
        }
        found.insert((kind, fields.next().unwrap_or_default().to_owned()));
    }
    None
}

/// The key types age encrypts to: `ssh-ed25519` and `ssh-rsa` (2048 bits or
/// more). `ecdsa-*` and `sk-*` keys are not, so they are left out. A line as
/// GitHub publishes it, normalized to `<type> <base64>`.
pub fn usable_key(line: &str) -> Option<String> {
    let mut fields = line.split_whitespace();
    let (kind, body) = (fields.next()?, fields.next()?);
    let key = format!("{kind} {body}");
    age::ssh::Recipient::from_str(&key).ok().map(|_| key)
}

/// Where each login's public keys come from.
pub trait KeySource: std::fmt::Debug + Send + Sync {
    /// Every key published for `login`, one per line as published, usable
    /// or not. An empty list is a login with no keys, not an error.
    fn keys(&self, login: &str) -> Result<Vec<String>>;
}

/// `https://github.com/<login>.keys`, or [`KEYS_URL_ENV`]'s base instead.
#[derive(Debug)]
pub struct PublishedKeys {
    base: String,
}

impl PublishedKeys {
    pub fn from_env() -> Self {
        Self {
            base: std::env::var(KEYS_URL_ENV)
                .ok()
                .filter(|base| !base.is_empty())
                .unwrap_or_else(|| DEFAULT_KEYS_URL.to_owned())
                .trim_end_matches('/')
                .to_owned(),
        }
    }
}

impl KeySource for PublishedKeys {
    fn keys(&self, login: &str) -> Result<Vec<String>> {
        let failed = |message: String| NewgitError::Remote { status: 0, message };
        let text = if let Some(dir) = self.base.strip_prefix("file://") {
            let path = Utf8Path::new(dir).join(format!("{login}.keys"));
            match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(source) => return Err(NewgitError::io(path, source)),
            }
        } else if self.base.starts_with("https://") {
            let url = format!("{}/{login}.keys", self.base);
            let mut response = match ureq::get(&url).call() {
                Ok(response) => response,
                Err(ureq::Error::StatusCode(404)) => return Ok(Vec::new()),
                Err(error) => {
                    return Err(failed(format!("could not read {url}: {error}")));
                }
            };
            response
                .body_mut()
                .read_to_string()
                .map_err(|error| failed(format!("could not read {url}: {error}")))?
        } else {
            // Keys decide who can read a secret: never over plain HTTP,
            // where anyone on the path could answer with their own.
            return Err(failed(format!(
                "${KEYS_URL_ENV} is `{}`: keys are read only from an `https://` base or a \
                 `file://` directory",
                self.base
            )));
        };
        Ok(text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }
}

/// The private keys this machine can decrypt with, loaded once per process.
struct Keyring {
    identities: Vec<Box<dyn age::Identity + Send + Sync>>,
    /// What was looked at, for the message when nothing fits.
    tried: Vec<String>,
}

impl Keyring {
    /// [`IDENTITY_ENV`], then `~/.ssh/id_ed25519` and `~/.ssh/id_rsa`. A
    /// passphrase-protected SSH key is asked for once, on a terminal;
    /// without one it is skipped.
    fn load() -> Self {
        let mut ring = Self {
            identities: Vec::new(),
            tried: Vec::new(),
        };
        if let Ok(list) = std::env::var(IDENTITY_ENV) {
            for path in list.split(':').filter(|path| !path.is_empty()) {
                ring.add_file(Utf8Path::new(path));
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            for name in ["id_ed25519", "id_rsa"] {
                let path = Utf8Path::new(&home).join(".ssh").join(name);
                if path.is_file() {
                    ring.add_file(&path);
                }
            }
        }
        ring
    }

    fn add_file(&mut self, path: &Utf8Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            self.tried.push(format!("{path} (unreadable)"));
            return;
        };
        if text.contains("AGE-SECRET-KEY-") {
            for line in text
                .lines()
                .filter(|line| line.starts_with("AGE-SECRET-KEY-"))
            {
                if let Ok(identity) = age::x25519::Identity::from_str(line.trim()) {
                    self.identities.push(Box::new(identity));
                }
            }
            self.tried.push(path.to_string());
            return;
        }
        match age::ssh::Identity::from_buffer(text.as_bytes(), Some(path.to_string())) {
            Ok(age::ssh::Identity::Unencrypted(key)) => {
                self.identities
                    .push(Box::new(age::ssh::Identity::Unencrypted(key)));
                self.tried.push(path.to_string());
            }
            Ok(age::ssh::Identity::Encrypted(key)) => {
                use std::io::IsTerminal as _;
                if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
                    self.tried.push(format!(
                        "{path} (passphrase-protected, and no terminal to ask; point \
                         ${IDENTITY_ENV} at a key without one)"
                    ));
                    return;
                }
                let passphrase = rpassword::prompt_password(format!(
                    "newgit needs {path} to decrypt tracker content. Passphrase: "
                ));
                match passphrase
                    .ok()
                    .map(|passphrase| key.decrypt(age::secrecy::SecretString::from(passphrase)))
                {
                    Some(Ok(key)) => {
                        self.identities
                            .push(Box::new(age::ssh::Identity::Unencrypted(key)));
                        self.tried.push(path.to_string());
                    }
                    _ => self
                        .tried
                        .push(format!("{path} (passphrase-protected, not unlocked)")),
                }
            }
            Ok(age::ssh::Identity::Unsupported(_)) => {
                self.tried
                    .push(format!("{path} (a key type age cannot decrypt with)"));
            }
            Err(_) => self.tried.push(format!("{path} (not a key)")),
        }
    }

    fn decrypt(&self, ciphertext: &[u8]) -> Option<Vec<u8>> {
        let decryptor = age::Decryptor::new_buffered(ciphertext).ok()?;
        let mut reader = decryptor
            .decrypt(
                self.identities
                    .iter()
                    .map(|identity| identity.as_ref() as _),
            )
            .ok()?;
        let mut plaintext = Vec::new();
        reader.read_to_end(&mut plaintext).ok()?;
        Some(plaintext)
    }

    fn tried(&self) -> String {
        if self.tried.is_empty() {
            format!(
                "no key was found (looked at ${IDENTITY_ENV}, ~/.ssh/id_ed25519 and \
                 ~/.ssh/id_rsa)"
            )
        } else {
            format!("tried {}", self.tried.join(", "))
        }
    }
}

/// One keyring per process, so a passphrase is asked for once.
static KEYRING: LazyLock<Mutex<Option<Arc<Keyring>>>> = LazyLock::new(Default::default);

fn keyring() -> Arc<Keyring> {
    KEYRING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(|| Arc::new(Keyring::load()))
        .clone()
}

/// Reads sealed blobs back: the plaintext cache first, then the keyring.
#[derive(Debug, Clone)]
pub struct Reader {
    /// `<store>/.newgit/local/tracker-plain`: plaintext by ciphertext blob id.
    cache: Utf8PathBuf,
}

impl Reader {
    /// For the tracker store at `git_dir` (`.newgit/local/trackers.git`).
    pub fn for_store(git_dir: &Utf8Path) -> Self {
        let local = git_dir
            .parent()
            .map_or_else(|| git_dir.to_path_buf(), Utf8Path::to_path_buf);
        Self {
            cache: local.join("tracker-plain"),
        }
    }

    /// The cached plaintext of blob `oid`, if this machine has it.
    pub fn cached(&self, oid: &str) -> Option<Vec<u8>> {
        std::fs::read(self.cache.join(oid)).ok()
    }

    /// Remember `plaintext` as blob `oid`'s.
    pub fn remember(&self, oid: &str, plaintext: &[u8]) -> Result<()> {
        if self.cache.join(oid).is_file() {
            return Ok(());
        }
        crate::materializer::create_dir_all(&self.cache)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&self.cache, std::fs::Permissions::from_mode(0o700));
        }
        let staging = self.cache.join(format!("{oid}.tmp"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&staging)
            .map_err(|source| NewgitError::io(&staging, source))?;
        file.write_all(plaintext)
            .map_err(|source| NewgitError::io(&staging, source))?;
        drop(file);
        let path = self.cache.join(oid);
        std::fs::rename(&staging, &path).map_err(|source| NewgitError::io(path, source))
    }

    /// Decrypt `ciphertext` (blob `oid`), or say which keys were tried.
    pub fn decrypt(
        &self,
        tracker: &str,
        oid: &str,
        ciphertext: &[u8],
        what: &str,
    ) -> Result<Vec<u8>> {
        if let Some(plaintext) = self.cached(oid) {
            return Ok(plaintext);
        }
        let keyring = keyring();
        match keyring.decrypt(ciphertext) {
            Some(plaintext) => {
                self.remember(oid, &plaintext)?;
                Ok(plaintext)
            }
            None => Err(NewgitError::Encryption {
                tracker: tracker.to_owned(),
                message: format!(
                    "{what} is encrypted, and no key on this machine opens it ({})",
                    keyring.tried()
                ),
            }),
        }
    }

    /// Drop cached plaintext of blobs `keep` no longer holds.
    pub fn prune(&self, keep: impl Fn(&str) -> bool) -> Result<()> {
        let Ok(entries) = std::fs::read_dir(&self.cache) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !keep(&name) {
                let path = entry.path();
                std::fs::remove_file(&path).map_err(|source| {
                    NewgitError::io(path.to_string_lossy().into_owned(), source)
                })?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED25519: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKV4Ja5y2qIZvK4ONhorVcP5DZgXXL60Z+0zu4A55NMU";

    fn jack() -> Recipients {
        Recipients::new([Recipient {
            who: "user:jack".to_owned(),
            key: ED25519.to_owned(),
        }])
    }

    #[test]
    fn a_header_names_exactly_its_recipients() {
        let sealed = jack().encrypt("t", b"SECRET=1\n").expect("encrypt");
        assert!(is_age(&sealed));
        assert!(!sealed.windows(6).any(|w| w == b"SECRET"));
        assert!(jack().sealed(&sealed));
        assert!(!Recipients::default().sealed(&sealed));
        assert!(
            !jack().sealed(b"SECRET=1\n"),
            "plaintext is sealed to nobody"
        );

        // One more recipient is a different set, whatever a record says.
        let x25519 = age::x25519::Identity::generate().to_public();
        let recipients: Vec<Box<dyn age::Recipient + Send>> = vec![
            Box::new(age::ssh::Recipient::from_str(ED25519).expect("key")),
            Box::new(x25519),
        ];
        let encryptor =
            age::Encryptor::with_recipients(recipients.iter().map(|key| key.as_ref() as _))
                .expect("encryptor");
        let mut out = Vec::new();
        let mut writer = encryptor.wrap_output(&mut out).expect("wrap");
        writer.write_all(b"x").expect("write");
        writer.finish().expect("finish");
        assert!(
            !jack().sealed(&out),
            "an extra X25519 stanza is not jack alone"
        );
    }

    #[test]
    fn fingerprints_are_what_ssh_keygen_prints() {
        let recipient = jack().iter().next().cloned().expect("one");
        // `ssh-keygen -lf` of the same key.
        assert_eq!(
            recipient.fingerprint(),
            "SHA256:P5oYhksqR5fqc/761xs+owXCMFcrYd9BgBg+i9QCmjc"
        );
        assert_eq!(
            jack().listing("  "),
            "  jack: SHA256:P5oYhksqR5fqc/761xs+owXCMFcrYd9BgBg+i9QCmjc (ssh-ed25519)"
        );
    }

    #[test]
    fn only_ed25519_and_rsa_keys_are_usable() {
        assert!(usable_key(ED25519).is_some());
        assert!(usable_key("ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBEmKSENjQEezOmxkZMy7opKgwFB9nkt5YRrYMjNuG5N87uRgg6CLrbo5wAdT/y6v0mKV0U2w0WZ2YB/++Tpockg=").is_none());
        assert!(usable_key("not a key").is_none());
    }

    #[test]
    fn keys_are_never_read_over_plain_http() {
        let source = PublishedKeys {
            base: "http://example.invalid".to_owned(),
        };
        let error = source.keys("jack").expect_err("refused");
        assert!(error.to_string().contains("https://"), "{error}");
    }
}
