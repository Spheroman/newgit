// The real `newgit serve`, started as its own process on a free port, and
// the helpers its tests share: `include!`d, so each test binary has its own copy.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::process::{Child, Command, Output, Stdio};

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;

const SECRET: &str = "test-dev-login";

struct Server {
    child: Child,
    url: String,
    #[allow(dead_code, reason = "read by some of the binaries that include this, not all")]
    data: Utf8PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(temp: &Utf8Path) -> Server {
    let data = temp.join("serve");
    let mut child = Command::new(env!("CARGO_BIN_EXE_newgit"))
        .args(["serve", "--data", data.as_str(), "--listen", "127.0.0.1:0", "--allow", "*"])
        .env("NEWGIT_SERVE_DEV_LOGIN_SECRET", SECRET)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("newgit serve starts");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("it says where it listens");
    let url = line
        .trim()
        .rsplit(' ')
        .next()
        .expect("a URL")
        .to_owned();
    Server { child, url, data }
}

/// A raw HTTP request: enough for the dev login and a few API reads.
fn http(url: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, Value) {
    let host = url.trim_start_matches("http://");
    let mut stream = TcpStream::connect(host).expect("connect");
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).expect("send");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("read");
    let status = reply
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = reply.split("\r\n\r\n").nth(1).unwrap_or("");
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

/// Log `login` in with the dev secret, and save it in `home` as
/// `newgit login` would, beside any other remote's login. Returns the token.
fn login(server: &Server, home: &Utf8Path, login: &str) -> String {
    let (status, answer) = http(
        &server.url,
        "POST",
        "/v1/dev/login",
        &[("x-dev-login", SECRET)],
        &format!("{{\"login\":\"{login}\"}}"),
    );
    assert_eq!(status, 200, "dev login: {answer}");
    let token = answer["token"].as_str().expect("a token").to_owned();
    // Appended, as a second `newgit login` to another remote leaves it.
    std::fs::create_dir_all(home.join(".newgit")).expect("mkdir");
    let path = home.join(".newgit/auth.toml");
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    text.push_str(&format!(
        "[remotes.\"{}\"]\nlogin = \"{login}\"\ntoken = \"{token}\"\ntoken_id = \"{}\"\n\n",
        server.url,
        answer["token_id"].as_str().expect("an id")
    ));
    std::fs::write(&path, text).expect("write auth");
    token
}

fn run(program: &str, home: &Utf8Path, dir: &Utf8Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(dir.as_str())
        .args(args)
        .env("HOME", home.as_str())
        // Members' public keys from a directory, never github.com.
        .env("NEWGIT_KEYS_URL", format!("file://{home}/published-keys"))
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("runs")
}

fn ok(program: &str, home: &Utf8Path, dir: &Utf8Path, args: &[&str]) -> String {
    let output = run(program, home, dir, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn newgit(home: &Utf8Path, dir: &Utf8Path, args: &[&str]) -> String {
    ok(env!("CARGO_BIN_EXE_newgit"), home, dir, args)
}

fn git(home: &Utf8Path, dir: &Utf8Path, args: &[&str]) -> String {
    ok("git", home, dir, args)
}

/// Git, authenticated with `token` and no credential helper.
fn git_as(token: &str, dir: &Utf8Path, args: &[&str]) -> Output {
    let header = format!("http.extraHeader=Authorization: Bearer {token}");
    let mut all = vec!["-c", "credential.helper=", "-c", header.as_str()];
    all.extend_from_slice(args);
    run("git", dir, dir, &all)
}

/// A directory of its own for one test. A counter, not the clock: tests in
/// one binary start at once, and two started on the same tick shared a
/// directory and wrote into each other's project.
fn tempdir() -> (TempDir, Utf8PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("newgit-serve-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = Utf8PathBuf::from_path_buf(dir.canonicalize().expect("canonical")).expect("utf8");
    (TempDir(path.clone()), path)
}

struct TempDir(Utf8PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
