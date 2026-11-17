// `GET /v1/serve/status` (#450): a background extension that fails is
// something an operator can read, not only a line in the server's log, and
// only this server's admins read it.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use newgit_serve::api::{ApiRequest, Caller};
use newgit_serve::{App, Config};
use support::{HOME, config, landing, project_with_root, tempdir};

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(60), "{what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn caller(login: &str) -> Caller {
    Caller {
        login: login.to_owned(),
        token: String::new(),
        token_id: format!("{login}-token"),
        kind: "login".to_owned(),
        agent: None,
        project: None,
        instance: None,
        expires: i64::MAX,
    }
}

fn status(app: &App, login: &str) -> (u16, serde_json::Value) {
    match newgit_serve::status::status(app, app.db.lock(), &caller(login)) {
        Ok(answer) => (answer.status, answer.body),
        Err(error) => (error.status, serde_json::json!({ "error": error.message })),
    }
}

#[test]
fn failing_extensions_are_in_the_status() {
    let data = tempdir("serve-status");
    let faults = data.join("faults");
    std::fs::write(&faults, "rooted-panic\n").expect("arm");
    let output = std::process::Command::new(std::env::current_exe().expect("this test"))
        .args(["--exact", "in_its_own_process", "--ignored", "--nocapture"])
        .env("NEWGIT_SERVE_FAULT_FILE", faults.as_str())
        .env("NEWGIT_SERVE_ROOTED_RETRY_MS", "50")
        .env("SERVE_STATUS_DATA", data.as_str())
        .output()
        .expect("runs");
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
#[ignore = "run by failing_extensions_are_in_the_status, in its own process"]
fn in_its_own_process() {
    let data = camino::Utf8PathBuf::from(std::env::var("SERVE_STATUS_DATA").expect("data"));
    let faults = data.join("faults");
    let app = App::open(Config {
        admin_logins: "jack".to_owned(),
        dev_login_secret: Some("secret".to_owned()),
        ..config(&data.join("serve"))
    })
    .expect("opens");
    let (_root, commits) = project_with_root(&app, HOME, 2);
    let app = Arc::new(app);
    app.log
        .land(&app, landing(&app, "main", None, &commits[1]))
        .expect("lands");

    let (code, answer) = status(&app, "ana");
    assert_eq!(code, 403, "not an admin: {answer}");

    newgit_serve::rooted::start(&app);
    wait_for("a pass fails", || !app.rooted.failing().is_empty());
    let (code, answer) = status(&app, "jack");
    assert_eq!(code, 200, "{answer}");
    let failing = answer["rooted"]["failing"].as_array().expect("a list");
    assert_eq!(failing.len(), 1, "{answer}");
    assert_eq!(failing[0]["home"], HOME);
    assert!(
        failing[0]["error"]
            .as_str()
            .is_some_and(|error| error.contains("panicked")),
        "{answer}"
    );
    assert!(failing[0]["attempts"].as_u64().is_some_and(|n| n >= 1));
    assert_eq!(answer["landing"]["stuck"], serde_json::json!([]));
    assert_eq!(answer["landing"]["applied"], answer["landing"]["tip"]);

    // Fixed, it leaves the status.
    std::fs::write(&faults, "").expect("disarm");
    wait_for("the retry succeeds", || app.rooted.failing().is_empty());
    let (_, answer) = status(&app, "jack");
    assert_eq!(answer["rooted"]["failing"], serde_json::json!([]));

    // And it is a route: an admin's bearer reaches it through the API.
    let request = |method: &str, path: &str, bearer: Option<String>, body: &str| ApiRequest {
        method: method.to_owned(),
        path: path.to_owned(),
        query: Default::default(),
        bearer,
        dev_login: Some("secret".to_owned()),
        headers: Default::default(),
        origin: "http://localhost".to_owned(),
        body: body.as_bytes().to_vec(),
    };
    let token = |login: &str| {
        let answer = newgit_serve::api::handle(
            &app,
            &request(
                "POST",
                "/v1/dev/login",
                None,
                &format!("{{\"login\":\"{login}\"}}"),
            ),
        )
        .expect("logs in");
        answer.body["token"].as_str().expect("a token").to_owned()
    };
    let routed = newgit_serve::api::handle(
        &app,
        &request("GET", "/v1/serve/status", Some(token("jack")), ""),
    )
    .expect("jack reads it");
    assert_eq!(routed.body["rooted"]["failing"], serde_json::json!([]));
    let refused = newgit_serve::api::handle(
        &app,
        &request("GET", "/v1/serve/status", Some(token("ana")), ""),
    )
    .err()
    .expect("ana does not");
    assert_eq!(refused.status, 403, "{}", refused.message);

    // Garbage collection's last pass is there too, once one has run.
    assert_eq!(
        status(&app, "jack").1["gc"]["last"],
        serde_json::Value::Null
    );
    newgit_serve::maintenance::run(&app);
    let (_, answer) = status(&app, "jack");
    assert!(
        answer["gc"]["last"]["stores"]
            .as_u64()
            .is_some_and(|n| n >= 1),
        "{answer}"
    );
    assert_eq!(
        answer["gc"]["last"]["failed"],
        serde_json::json!({}),
        "{answer}"
    );
}
