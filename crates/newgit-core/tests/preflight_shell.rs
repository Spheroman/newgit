//! #87: `newgit resource check` reads a hook command as shell. Every
//! program in a position the shell would run is looked up, nothing that is
//! not a program is, and what only a running instance could know is a
//! `skip` with a reason, never a guess.

use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::preflight::{Finding, Outcome, check_command};

struct Fixture {
    _dir: tempfile::TempDir,
    root: Utf8PathBuf,
    bin: Utf8PathBuf,
}

/// A project (a Git repository) and a `PATH` of one directory holding
/// `have`, `sleep`, and the wrappers, so what is "installed" is exactly
/// what the test says.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let base =
        Utf8PathBuf::from_path_buf(dir.path().canonicalize().expect("canonicalize")).expect("utf8");
    let root = base.join("project");
    let bin = base.join("bin");
    std::fs::create_dir_all(&root).expect("mkdir");
    std::fs::create_dir_all(&bin).expect("mkdir");
    let status = Command::new("git")
        .args(["init", "-q"])
        .arg(root.as_str())
        .status()
        .expect("git runs");
    assert!(status.success());
    for name in [
        "have", "sleep", "env", "nohup", "nice", "timeout", "xargs", "sh", "which", "psql_x",
    ] {
        executable(&bin.join(name));
    }
    Fixture {
        _dir: dir,
        root,
        bin,
    }
}

fn executable(path: &Utf8Path) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, "#!/bin/sh\n").expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

impl Fixture {
    fn check(&self, command: &str) -> Vec<Finding> {
        check_command(command, &self.root, None, self.bin.as_str())
    }

    fn check_in(&self, command: &str, workdir: &str) -> Vec<Finding> {
        check_command(
            command,
            &self.root,
            Some(Utf8Path::new(workdir)),
            self.bin.as_str(),
        )
    }
}

/// `(program, status)` for each finding.
fn statuses(findings: &[Finding]) -> Vec<(String, &'static str)> {
    findings
        .iter()
        .map(|finding| {
            (
                finding.program.clone().unwrap_or_default(),
                finding.outcome.status(),
            )
        })
        .collect()
}

fn finding<'a>(findings: &'a [Finding], program: &str) -> &'a Finding {
    findings
        .iter()
        .find(|finding| finding.program.as_deref() == Some(program))
        .unwrap_or_else(|| panic!("`{program}` is not in {:?}", statuses(findings)))
}

fn status_of(findings: &[Finding], program: &str) -> &'static str {
    finding(findings, program).outcome.status()
}

fn reason_of(findings: &[Finding], program: &str) -> String {
    match &finding(findings, program).outcome {
        Outcome::Found(found) => format!("found at {found}"),
        Outcome::Missing(reason) | Outcome::Unchecked(reason) => reason.clone(),
    }
}

/// Commands where `PROG` is in a position the shell (or a wrapper the
/// shell runs) executes.
const POSITIONS: &[&str] = &[
    "PROG",
    "if PROG --version; then :; fi",
    "if true; then PROG -c 1; fi",
    "if false; then :; elif PROG; then :; fi",
    "if false; then :; else PROG; fi",
    "! PROG && true",
    "true && PROG || true",
    "{ PROG; }",
    "(PROG)",
    "for f in a b; do PROG $f; done",
    "while ! PROG; do sleep 1; done",
    "until PROG; do sleep 1; done",
    "case \"$1\" in prod|staging) PROG ;; *) ;; esac",
    "f() { PROG; }; f",
    "have | PROG",
    "PROG &",
    "x=$(PROG)",
    "have \"$(PROG --json)\"",
    "have `PROG`",
    "time PROG",
    "FOO=1 PROG",
    "exec PROG",
    "command PROG",
    "env -u FOO PROG",
    "env -i PATH=/bin BAR=2 PROG",
    "nohup PROG >/dev/null 2>&1 &",
    "nice -n 5 PROG",
    "timeout -s KILL 5 PROG",
    "have | xargs -n1 PROG",
    "sh -c 'have; PROG'",
    "sh -ec \"PROG\"",
    "eval PROG",
    "sh <<EOF\nPROG\nEOF",
    "have <<EOF\n$(PROG)\nEOF",
];

#[test]
fn a_missing_program_fails_in_every_position_the_shell_runs_one() {
    let fixture = fixture();
    for position in POSITIONS {
        let command = position.replace("PROG", "nope_prog");
        let findings = fixture.check(&command);
        assert_eq!(
            status_of(&findings, "nope_prog"),
            "fail",
            "{command:?}: {:?}",
            statuses(&findings)
        );
    }
}

#[test]
fn an_installed_program_is_ok_in_every_position_and_nothing_else_fails() {
    let fixture = fixture();
    for position in POSITIONS {
        let command = position.replace("PROG", "have");
        let findings = fixture.check(&command);
        assert_eq!(status_of(&findings, "have"), "ok", "{command:?}");
        assert!(
            findings
                .iter()
                .all(|finding| finding.outcome.status() == "ok"),
            "{command:?}: {:?}",
            statuses(&findings)
        );
    }
}

#[test]
fn what_is_not_a_program_is_not_looked_up() {
    let fixture = fixture();
    for command in [
        // Case patterns are patterns.
        "case \"$1\" in prod|staging) have ;; start) have ;; esac",
        // Arithmetic names variables.
        "sleep $((RANDOM % 5))",
        "x=$((COUNT + 1)); (( x > 2 )) && have",
        // Functions are not programs, even one called before its definition
        // in the text.
        "a() { b; }; b() { have; }; a",
        "f() { have; }; f && f",
        // Builtins, bash's included.
        "declare -a list; local_var=1; export A=b; set -eu; trap 'have' EXIT; read -r line </dev/null || true",
        "type have >/dev/null; hash have; printf '%s' x; test -n x && [ -n x ]",
        // `env`'s options and assignments are not the program.
        "env -u FOO have",
        "env -uFOO --unset=BAR -i PATH=/bin have",
        // Here-document bodies are input.
        "psql_x <<'SQL'\ndo $$ begin truncate t; end $$;\nSQL\nhave",
        "psql_x <<-SQL\n\tnot_a_command\n\tSQL",
        // Redirection targets are files.
        "have > out.log 2>&1 < /dev/null",
        // Comments.
        "# not_a_command\nhave # nor_this",
        // `sh -s`'s operands are the script's arguments, not a file.
        "have | sh -s -- --yes",
        // Quotes hide separators.
        "have -c 'import os; os.exit(0)' && sleep 1",
    ] {
        let findings = fixture.check(command);
        assert!(
            findings
                .iter()
                .all(|finding| finding.outcome.status() == "ok"),
            "{command:?}: {:?}",
            statuses(&findings)
        );
    }
}

#[test]
fn a_relative_path_is_looked_for_where_cd_left_the_command() {
    let fixture = fixture();
    executable(&fixture.root.join("web/bin/x"));

    let findings = fixture.check("cd web && ./bin/x");
    assert_eq!(status_of(&findings, "./bin/x"), "ok");

    let findings = fixture.check("cd web || exit 1\n./bin/x");
    assert_eq!(status_of(&findings, "./bin/x"), "ok");

    let findings = fixture.check("pushd web >/dev/null && ./bin/x; popd >/dev/null; ./web/bin/x");
    assert_eq!(status_of(&findings, "./bin/x"), "ok");
    assert_eq!(status_of(&findings, "./web/bin/x"), "ok");

    let findings = fixture.check("cd web/bin && cd .. && ./bin/x");
    assert_eq!(status_of(&findings, "./bin/x"), "ok");

    // A subshell's `cd` stays in the subshell.
    let findings = fixture.check("(cd web && ./bin/x) && ./web/bin/x");
    assert_eq!(status_of(&findings, "./bin/x"), "ok");
    assert_eq!(status_of(&findings, "./web/bin/x"), "ok");

    // Without the `cd`, `./bin/x` is not there.
    let findings = fixture.check("./bin/x");
    assert_eq!(status_of(&findings, "./bin/x"), "fail");
    assert!(reason_of(&findings, "./bin/x").contains("bin/x does not exist"));

    // A `workdir` is where the command starts.
    let findings = fixture.check_in("./bin/x", "web");
    assert_eq!(status_of(&findings, "./bin/x"), "ok");
    let findings = fixture.check_in("cd .. && ./web/bin/x", "web");
    assert_eq!(status_of(&findings, "./web/bin/x"), "ok");
}

#[test]
fn a_cd_newgit_cannot_follow_skips_the_relative_paths_after_it() {
    let fixture = fixture();
    std::fs::create_dir_all(fixture.root.join("web")).expect("mkdir");
    for (command, why) in [
        ("cd \"$DIR\" && ./bin/x", "cd \"$DIR\""),
        ("cd {{workspace}}/web && ./bin/x", "{{workspace}}"),
        ("cd && ./bin/x", "home directory"),
        (
            "mkdir -p build && cd build && ./bin/x",
            "build is not a directory",
        ),
        (
            "if [ -n \"$1\" ]; then cd web; fi; ./bin/x",
            "may not have happened",
        ),
        ("cd web || true; ./bin/x", "may not have happened"),
    ] {
        let findings = fixture.check(command);
        assert_eq!(status_of(&findings, "./bin/x"), "skip", "{command:?}");
        assert!(
            reason_of(&findings, "./bin/x").contains(why),
            "{command:?}: {}",
            reason_of(&findings, "./bin/x")
        );
    }
    // An absolute path does not depend on where `cd` went.
    let have = format!("{}/have", fixture.bin);
    let findings = fixture.check(&format!("cd \"$DIR\" && {have}"));
    assert_eq!(status_of(&findings, &have), "ok");
}

#[test]
fn a_script_must_be_executable_to_run_by_path() {
    let fixture = fixture();
    let script = fixture.root.join("scripts/seed.sh");
    std::fs::create_dir_all(script.parent().expect("parent")).expect("mkdir");
    std::fs::write(&script, "#!/bin/sh\n").expect("write");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).expect("chmod");

    let findings = fixture.check("./scripts/seed.sh");
    assert_eq!(status_of(&findings, "./scripts/seed.sh"), "fail");
    assert!(reason_of(&findings, "./scripts/seed.sh").contains("is not executable"));

    // A shell reading it needs it to exist, not to be executable.
    let findings = fixture.check("sh ./scripts/seed.sh && . ./scripts/seed.sh");
    assert_eq!(status_of(&findings, "./scripts/seed.sh"), "ok");
    let findings = fixture.check("sh ./scripts/gone.sh");
    assert_eq!(status_of(&findings, "./scripts/gone.sh"), "fail");

    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let findings = fixture.check("./scripts/seed.sh");
    assert_eq!(status_of(&findings, "./scripts/seed.sh"), "ok");

    let findings = fixture.check("./scripts");
    assert_eq!(status_of(&findings, "./scripts"), "fail");
    assert!(reason_of(&findings, "./scripts").contains("is a directory"));
}

#[test]
fn a_program_the_command_checks_for_itself_is_not_a_failure() {
    let fixture = fixture();
    for command in [
        "command -v nope >/dev/null && nope || echo skip",
        "if ! command -v nope >/dev/null 2>&1; then exit 0; fi\nnope run",
        "type nope >/dev/null 2>&1 && nope",
        "which nope && nope",
    ] {
        let findings = fixture.check(command);
        assert_eq!(status_of(&findings, "nope"), "skip", "{command:?}");
        assert!(
            reason_of(&findings, "nope").contains("checks for it first"),
            "{command:?}"
        );
    }
    let findings = fixture.check("[ -x ./bin/maybe ] && ./bin/maybe");
    assert_eq!(status_of(&findings, "./bin/maybe"), "skip");

    // Installed, it is simply found.
    let findings = fixture.check("command -v have >/dev/null && have || echo skip");
    assert_eq!(statuses(&findings), [("have".to_owned(), "ok")]);
}

#[test]
fn what_only_an_instance_knows_is_skipped_with_why() {
    let fixture = fixture();
    std::fs::write(fixture.root.join(".gitignore"), "node_modules/\n").expect("write");
    for (command, program, why) in [
        ("$TOOL run", "$TOOL", "a variable decides it"),
        (
            "\"${TOOL:-have}\" run",
            "\"${TOOL:-have}\"",
            "a variable decides it",
        ),
        (
            "{{scripts}}/seed.sh --fast",
            "{{scripts}}/seed.sh",
            "`{{scripts}}` decides it",
        ),
        (
            "$(which have) run",
            "$(which have)",
            "a command's output decides it",
        ),
        ("./tools/*.sh", "./tools/*.sh", "a glob decides it"),
        (
            "./node_modules/.bin/vite build",
            "./node_modules/.bin/vite",
            "Git ignores it",
        ),
        ("sh -c \"$SCRIPT\"", "\"$SCRIPT\"", "a variable decides it"),
        (
            "eval \"$SETUP\"",
            "\"$SETUP\"",
            "`eval` runs text a variable decides",
        ),
        ("env -S 'have -x' run", "'have -x'", "env -S"),
    ] {
        let findings = fixture.check(command);
        assert_eq!(status_of(&findings, program), "skip", "{command:?}");
        assert!(
            reason_of(&findings, program).contains(why),
            "{command:?}: {}",
            reason_of(&findings, program)
        );
    }
}

#[test]
fn a_command_sh_cannot_parse_fails_as_a_whole() {
    let fixture = fixture();
    let findings = fixture.check("if have; then have");
    assert_eq!(findings.len(), 1, "{:?}", statuses(&findings));
    assert_eq!(findings[0].program, None);
    match &findings[0].outcome {
        Outcome::Missing(reason) => assert!(reason.starts_with("sh cannot parse it"), "{reason}"),
        other => panic!("{other:?}"),
    }
    // Templates do not upset the parse.
    let findings = fixture.check("have {{branch.slug}} > {{snapshot.path}}/db.sql && have");
    assert_eq!(statuses(&findings), [("have".to_owned(), "ok")]);
}

#[test]
fn each_program_is_reported_once_per_command() {
    let fixture = fixture();
    let findings = fixture.check("have a && have b; nope; nope");
    assert_eq!(
        statuses(&findings),
        [("have".to_owned(), "ok"), ("nope".to_owned(), "fail")]
    );
}

/// What only bash parses, in a position bash runs a program or in input bash
/// does not run. Hooks run under `sh`: where `sh` is bash (macOS) these
/// read like any other command, and where it is dash (Debian, Ubuntu) the
/// command cannot run at all, which is what the check must say instead.
const BASH_ONLY: &[(&str, Option<&str>)] = &[
    ("function g { have; }; g", Some("have")),
    ("have <(have)", Some("have")),
    ("have <<< \"not a command\"", None),
];

fn sh_parses(command: &str) -> bool {
    Command::new("sh")
        .args(["-n", "-c", command])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn bash_only_syntax_is_read_where_sh_is_bash_and_refused_where_it_is_not() {
    let fixture = fixture();
    for &(command, program) in BASH_ONLY {
        let findings = fixture.check(command);
        if sh_parses(command) {
            assert!(
                findings
                    .iter()
                    .all(|finding| finding.outcome.status() == "ok"),
                "{command:?}: {:?}",
                statuses(&findings)
            );
            if let Some(program) = program {
                assert_eq!(status_of(&findings, program), "ok", "{command:?}");
                let missing = command.replace(program, "nope_prog");
                let findings = fixture.check(&missing);
                assert_eq!(status_of(&findings, "nope_prog"), "fail", "{missing:?}");
            }
        } else {
            assert_eq!(findings.len(), 1, "{command:?}: {:?}", statuses(&findings));
            assert_eq!(findings[0].program, None, "{command:?}");
            match &findings[0].outcome {
                Outcome::Missing(reason) => {
                    assert!(reason.starts_with("sh cannot parse it"), "{reason}");
                }
                other => panic!("{command:?}: {other:?}"),
            }
        }
    }
}
