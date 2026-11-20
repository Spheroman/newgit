//! Preflight for resource definitions (#87): does each program a hook's
//! command starts exist on this machine? A `[checkpoint] command` naming
//! `pg_dump` on a host with no Postgres client fails at the first checkpoint,
//! and a `[restore]` one only during the undo someone is relying on. This
//! finds both with no instance and no spawn, the way `render --check` checks
//! committed content.
//!
//! The command is read as shell, not guessed at. `sh -n` (the shell that
//! will run it, told to parse and not execute) says whether it parses at
//! all; [`brush_parser`] gives the tree, and the walk below follows what the
//! shell would run: every simple command in every `if`, loop, `case` arm,
//! group, subshell, pipeline, function body, and `$(...)`, past assignments
//! and wrappers (`exec`, `env`, `nohup`, `timeout`, `xargs`, `sh -c`,
//! `eval`), from the directory a `cd` left it in.
//!
//! Each program comes out as one of three answers, and the third is the
//! point: `ok` (found), `fail` (it will not run), or `skip` (newgit cannot
//! know before an instance runs it, and says why: a template or a variable
//! names it, a `cd` it cannot follow moved it, or the command checks for it
//! itself with `command -v`). An agent fixes what fails and leaves what is
//! skipped; a guess in either direction would send it after nothing.

use std::collections::{BTreeMap, BTreeSet};

use brush_parser::ast::{
    AndOr, AndOrList, AssignmentValue, Command, CommandPrefixOrSuffixItem, CompoundCommand,
    CompoundList, ExtendedTestExpr, IoFileRedirectTarget, IoRedirect, Pipeline, SeparatorOperator,
    SimpleCommand, Word,
};
use brush_parser::word::{TildeExpr, WordPiece, WordPieceWithSource};
use camino::{Utf8Component, Utf8Path, Utf8PathBuf};

use crate::resource::ResourceDefinition;

/// One program a definition's command starts, and what the check made of
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramCheck {
    pub resource: String,
    /// Where the command is: `[actions.prepare]`, `[checkpoint]`,
    /// `[identity] key_command`, ...
    pub hook: String,
    /// The word as the definition wrote it; `None` when the answer is about
    /// the whole command (it does not parse).
    pub program: Option<String>,
    pub outcome: Outcome,
}

impl ProgramCheck {
    pub fn is_missing(&self) -> bool {
        matches!(self.outcome, Outcome::Missing(_))
    }
}

/// One finding in one command: the word, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub program: Option<String>,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Found, here.
    Found(Utf8PathBuf),
    /// It will not run, and why: not on `PATH`, no such file, not
    /// executable, or the command does not parse.
    Missing(String),
    /// Not checked, and why: only a running instance knows.
    Unchecked(String),
}

impl Outcome {
    /// `ok`, `fail`, or `skip`: the word `--json` and the text output use.
    pub fn status(&self) -> &'static str {
        match self {
            Outcome::Found(_) => "ok",
            Outcome::Missing(_) => "fail",
            Outcome::Unchecked(_) => "skip",
        }
    }
}

/// Every command string `definition` holds, with where it is and the
/// directory it runs in, relative to the workspace root (`None` for the
/// root itself).
pub fn commands(definition: &ResourceDefinition) -> Vec<(String, String, Option<Utf8PathBuf>)> {
    let resource_dir = definition.workdir_for(None).map(Utf8Path::to_owned);
    let mut commands = Vec::new();
    for (name, action) in &definition.actions {
        if let Some(command) = &action.command {
            commands.push((
                format!("[actions.{name}]"),
                command.clone(),
                definition.workdir_for(Some(action)).map(Utf8Path::to_owned),
            ));
        }
    }
    // `key_command` runs at the workspace root whatever `workdir` says.
    if let Some(command) = definition
        .identity
        .as_ref()
        .and_then(|identity| identity.key_command.clone())
    {
        commands.push(("[identity] key_command".to_owned(), command, None));
    }
    let hooks = [
        (
            "[checkpoint]",
            definition
                .checkpoint
                .as_ref()
                .and_then(|c| c.command.clone()),
        ),
        (
            "[restore]",
            definition.restore.as_ref().and_then(|r| r.command.clone()),
        ),
        (
            "[cleanup]",
            definition.cleanup.as_ref().and_then(|c| c.command.clone()),
        ),
        (
            "[ready]",
            definition.ready.as_ref().and_then(|r| r.command.clone()),
        ),
    ];
    for (hook, command) in hooks {
        if let Some(command) = command {
            commands.push((hook.to_owned(), command, resource_dir.clone()));
        }
    }
    commands
}

/// Check every program `definitions` start. `root` is the project root,
/// standing in for the workspace a hook would run in; `path` is the `PATH`
/// to search.
pub fn check(definitions: &[ResourceDefinition], root: &Utf8Path, path: &str) -> Vec<ProgramCheck> {
    let mut checks = Vec::new();
    for definition in definitions {
        for (hook, command, workdir) in commands(definition) {
            // `EDIT ME: your install command` names no program. The
            // definition check reports it as the placeholder it is
            // ([`crate::definition_check`]), rather than this as `EDIT`
            // missing from `PATH`.
            if crate::definition_check::is_placeholder(&command) {
                continue;
            }
            for finding in check_command(&command, root, workdir.as_deref(), path) {
                checks.push(ProgramCheck {
                    resource: definition.name.clone(),
                    hook: hook.clone(),
                    program: finding.program,
                    outcome: finding.outcome,
                });
            }
        }
    }
    checks
}

/// Check one command, run from `workdir` under `root` (the root itself
/// when `None`), against `path`. Each word is reported once, in the order
/// the shell would reach it.
pub fn check_command(
    command: &str,
    root: &Utf8Path,
    workdir: Option<&Utf8Path>,
    path: &str,
) -> Vec<Finding> {
    let (text, templates) = hide_templates(command);
    let show = |text: &str| show_templates(text, &templates);
    if let Some(error) = sh_syntax_error(&text) {
        return vec![Finding {
            program: None,
            outcome: Outcome::Missing(format!("sh cannot parse it: {}", show(&error))),
        }];
    }
    let dir = match workdir {
        None => Dir::Known(root.to_owned()),
        Some(workdir) if workdir.as_str().contains("{{") => Dir::Unknown(format!(
            "it runs in `workdir = \"{workdir}\"`, which a template decides"
        )),
        Some(workdir) => Dir::Known(normalize(&root.join(workdir))),
    };
    let scope = Scope {
        dir,
        stack: Vec::new(),
    };
    // Twice: the first walk only learns the functions the command defines,
    // so a call that comes before its definition in the text (inside an
    // earlier function's body) is still known to be one.
    let mut learn = Walker::new(root, path, &templates);
    learn.source(&text, &mut scope.clone());
    let mut walker = Walker::new(root, path, &templates);
    walker.functions = learn.functions;
    walker.source(&text, &mut scope.clone());
    walker.findings
}

/// Where `{{...}}` placeholders stand in the text the shell parses: an
/// identifier, so a template can never change how the rest parses.
const TEMPLATE_TOKEN: &str = "__newgit_template_";

/// `command` with each `{{...}}` replaced by a token, and the templates in
/// token order.
fn hide_templates(command: &str) -> (String, Vec<String>) {
    let mut text = String::new();
    let mut templates = Vec::new();
    let mut rest = command;
    while let Some(start) = rest.find("{{") {
        let Some(len) = rest[start..].find("}}") else {
            break;
        };
        text.push_str(&rest[..start]);
        text.push_str(&format!("{TEMPLATE_TOKEN}{}__", templates.len()));
        templates.push(rest[start..start + len + 2].to_owned());
        rest = &rest[start + len + 2..];
    }
    text.push_str(rest);
    (text, templates)
}

fn show_templates(text: &str, templates: &[String]) -> String {
    let mut shown = text.to_owned();
    for (index, template) in templates.iter().enumerate() {
        shown = shown.replace(&format!("{TEMPLATE_TOKEN}{index}__"), template);
    }
    shown
}

/// What `sh -n` says about `text`, when it refuses it. `None` when it
/// parses, or when `sh` could not be asked.
fn sh_syntax_error(text: &str) -> Option<String> {
    let output = std::process::Command::new("sh")
        .arg("-n")
        .arg("-c")
        .arg(text)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        return None;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    Some(if message.is_empty() {
        format!("`sh -n` exited with {}", output.status)
    } else {
        message
    })
}

fn parser_options() -> brush_parser::ParserOptions {
    brush_parser::ParserOptions {
        // `sh` has no extglob; with it on, `!(` would read as a pattern.
        enable_extended_globbing: false,
        ..Default::default()
    }
}

/// Builtins: the shell runs these itself, so there is nothing on `PATH`
/// to find. POSIX's, and bash's (macOS's `sh` is bash).
const BUILTINS: &[&str] = &[
    ":",
    ".",
    "[",
    "alias",
    "bg",
    "bind",
    "break",
    "builtin",
    "caller",
    "cd",
    "command",
    "compgen",
    "complete",
    "compopt",
    "continue",
    "declare",
    "dirs",
    "disown",
    "echo",
    "enable",
    "eval",
    "exec",
    "exit",
    "export",
    "false",
    "fc",
    "fg",
    "getopts",
    "hash",
    "help",
    "history",
    "jobs",
    "kill",
    "let",
    "local",
    "logout",
    "mapfile",
    "popd",
    "printf",
    "pushd",
    "pwd",
    "read",
    "readarray",
    "readonly",
    "return",
    "set",
    "shift",
    "shopt",
    "source",
    "suspend",
    "test",
    "times",
    "trap",
    "true",
    "type",
    "typeset",
    "ulimit",
    "umask",
    "unalias",
    "unset",
    "wait",
];

/// Programs that run a shell script given as `-c` text, a file, or stdin.
const SHELLS: &[&str] = &["sh", "bash", "dash", "zsh", "ksh", "mksh", "ash"];

/// Why a relative path cannot be looked for when a conditional `cd` may or
/// may not have run before it.
const UNSURE_CD: &str = "it runs after a `cd` that may not have happened";

/// The directory commands run in at a point in the walk.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Dir {
    Known(Utf8PathBuf),
    /// Not known, and why.
    Unknown(String),
}

/// What a command inherits from the commands before it: its directory and
/// `pushd`'s stack.
#[derive(Debug, Clone)]
struct Scope {
    dir: Dir,
    stack: Vec<Dir>,
}

struct Walker<'a> {
    root: &'a Utf8Path,
    path: &'a str,
    templates: &'a [String],
    /// Shell functions the command defines: calls to them are not programs.
    functions: BTreeSet<String>,
    /// Programs the command probes for itself (`command -v X`, `[ -x X ]`),
    /// with the probe: absent, it decides what to do, so absence is not a
    /// failure this check can call.
    guards: BTreeMap<String, String>,
    findings: Vec<Finding>,
}

impl<'a> Walker<'a> {
    fn new(root: &'a Utf8Path, path: &'a str, templates: &'a [String]) -> Self {
        Self {
            root,
            path,
            templates,
            functions: BTreeSet::new(),
            guards: BTreeMap::new(),
            findings: Vec::new(),
        }
    }

    fn show(&self, text: &str) -> String {
        show_templates(text, self.templates)
    }

    fn push(&mut self, program: Option<String>, outcome: Outcome) {
        let finding = Finding { program, outcome };
        if !self.findings.contains(&finding) {
            self.findings.push(finding);
        }
    }

    fn unchecked(&mut self, word: &Word, reason: impl Into<String>) {
        self.push(
            Some(self.show(&word.value)),
            Outcome::Unchecked(reason.into()),
        );
    }

    /// Walk shell text `sh -n` has accepted (or that `eval`, `sh -c`, or a
    /// `$(...)` holds).
    fn source(&mut self, text: &str, scope: &mut Scope) {
        let mut parser =
            brush_parser::Parser::new(std::io::BufReader::new(text.as_bytes()), &parser_options());
        match parser.parse_program() {
            Ok(program) => {
                for list in &program.complete_commands {
                    self.list(list, scope);
                }
            }
            Err(error) => self.push(
                Some(self.show(text.trim())),
                Outcome::Unchecked(format!(
                    "newgit could not read it ({error}), so nothing in it was checked"
                )),
            ),
        }
    }

    fn list(&mut self, list: &CompoundList, scope: &mut Scope) {
        for item in &list.0 {
            match item.1 {
                // `&` runs it in a subshell: its `cd` stays there.
                SeparatorOperator::Async => self.and_or(&item.0, &mut scope.clone()),
                SeparatorOperator::Sequence => self.and_or(&item.0, scope),
            }
        }
    }

    fn and_or(&mut self, list: &AndOrList, scope: &mut Scope) {
        let start = scope.dir.clone();
        self.pipeline(&list.first, scope);
        for next in &list.additional {
            match next {
                AndOr::And(pipeline) => self.pipeline(pipeline, scope),
                AndOr::Or(pipeline) => {
                    // It runs when something before it failed, which may
                    // have been the `cd`.
                    let mut alternative = scope.clone();
                    if alternative.dir != start {
                        alternative.dir = Dir::Unknown(UNSURE_CD.to_owned());
                    }
                    self.pipeline(pipeline, &mut alternative);
                    // `cd web || exit 1`: past it, the `cd` happened.
                    if !exits(pipeline) && alternative.dir != scope.dir {
                        scope.dir = Dir::Unknown(UNSURE_CD.to_owned());
                    }
                }
            }
        }
    }

    fn pipeline(&mut self, pipeline: &Pipeline, scope: &mut Scope) {
        if let [command] = pipeline.seq.as_slice() {
            self.command(command, scope);
        } else {
            // Each part of a pipeline runs in its own subshell.
            for command in &pipeline.seq {
                self.command(command, &mut scope.clone());
            }
        }
    }

    /// Walk what may run zero or more times; a `cd` inside leaves the
    /// directory after it unknown.
    fn conditional(&mut self, scope: &mut Scope, walk: impl FnOnce(&mut Self, &mut Scope)) {
        let mut alternative = scope.clone();
        walk(self, &mut alternative);
        if alternative.dir != scope.dir {
            scope.dir = Dir::Unknown(UNSURE_CD.to_owned());
        }
    }

    fn command(&mut self, command: &Command, scope: &mut Scope) {
        match command {
            Command::Simple(simple) => self.simple(simple, scope),
            Command::Compound(compound, redirects) => {
                self.compound(compound, scope);
                for redirect in redirects.iter().flat_map(|list| &list.0) {
                    self.redirect(redirect, scope);
                }
            }
            Command::Function(definition) => {
                self.functions.insert(definition.fname.value.clone());
                let mut body = scope.clone();
                self.compound(&definition.body.0, &mut body);
                for redirect in definition.body.1.iter().flat_map(|list| &list.0) {
                    self.redirect(redirect, &mut body);
                }
            }
            Command::ExtendedTest(test, redirects) => {
                self.extended_test(&test.expr, scope);
                for redirect in redirects.iter().flat_map(|list| &list.0) {
                    self.redirect(redirect, scope);
                }
            }
        }
    }

    fn compound(&mut self, compound: &CompoundCommand, scope: &mut Scope) {
        match compound {
            CompoundCommand::Arithmetic(_) => {}
            CompoundCommand::ArithmeticForClause(clause) => {
                self.conditional(scope, |walker, scope| walker.list(&clause.body.list, scope));
            }
            CompoundCommand::BraceGroup(group) => self.list(&group.list, scope),
            CompoundCommand::Subshell(subshell) => self.list(&subshell.list, &mut scope.clone()),
            CompoundCommand::ForClause(clause) => {
                for value in clause.values.iter().flatten() {
                    self.substitutions(value, scope);
                }
                self.conditional(scope, |walker, scope| walker.list(&clause.body.list, scope));
            }
            CompoundCommand::CaseClause(clause) => {
                // The patterns are patterns, not commands.
                self.substitutions(&clause.value, scope);
                for item in &clause.cases {
                    if let Some(list) = &item.cmd {
                        self.conditional(scope, |walker, scope| walker.list(list, scope));
                    }
                }
            }
            CompoundCommand::IfClause(clause) => {
                self.list(&clause.condition, scope);
                self.conditional(scope, |walker, scope| walker.list(&clause.then, scope));
                for other in clause.elses.iter().flatten() {
                    self.conditional(scope, |walker, scope| {
                        if let Some(condition) = &other.condition {
                            walker.list(condition, scope);
                        }
                        walker.list(&other.body, scope);
                    });
                }
            }
            CompoundCommand::WhileClause(clause) | CompoundCommand::UntilClause(clause) => {
                self.conditional(scope, |walker, scope| {
                    walker.list(&clause.0, scope);
                    walker.list(&clause.1.list, scope);
                });
            }
            CompoundCommand::Coprocess(coprocess) => {
                self.command(&coprocess.body, &mut scope.clone());
            }
        }
    }

    fn extended_test(&mut self, expr: &ExtendedTestExpr, scope: &mut Scope) {
        match expr {
            ExtendedTestExpr::And(left, right) | ExtendedTestExpr::Or(left, right) => {
                self.extended_test(left, scope);
                self.extended_test(right, scope);
            }
            ExtendedTestExpr::Not(inner) | ExtendedTestExpr::Parenthesized(inner) => {
                self.extended_test(inner, scope);
            }
            ExtendedTestExpr::UnaryTest(_, word) => self.substitutions(word, scope),
            ExtendedTestExpr::BinaryTest(_, left, right) => {
                self.substitutions(left, scope);
                self.substitutions(right, scope);
            }
        }
    }

    fn redirect(&mut self, redirect: &IoRedirect, scope: &mut Scope) {
        match redirect {
            IoRedirect::File(_, _, target) => match target {
                IoFileRedirectTarget::Filename(word) | IoFileRedirectTarget::Duplicate(word) => {
                    self.substitutions(word, scope);
                }
                IoFileRedirectTarget::Fd(_) => {}
                IoFileRedirectTarget::ProcessSubstitution(_, subshell) => {
                    self.list(&subshell.list, &mut scope.clone());
                }
            },
            IoRedirect::HereDocument(_, doc) => {
                // A quoted delimiter (`<<'SQL'`) means the body is only
                // input; unquoted, its `$(...)` runs.
                if doc.requires_expansion
                    && let Ok(pieces) =
                        brush_parser::word::parse_heredoc(&doc.doc.value, &parser_options())
                {
                    self.pieces(&pieces, scope);
                }
            }
            IoRedirect::HereString(_, word) | IoRedirect::OutputAndError(word, _) => {
                self.substitutions(word, scope);
            }
        }
    }

    /// Walk the commands a word's `$(...)` and backquotes run.
    fn substitutions(&mut self, word: &Word, scope: &mut Scope) {
        if let Ok(pieces) = brush_parser::word::parse(&word.value, &parser_options()) {
            self.pieces(&pieces, scope);
        }
    }

    fn pieces(&mut self, pieces: &[WordPieceWithSource], scope: &mut Scope) {
        for piece in pieces {
            match &piece.piece {
                WordPiece::DoubleQuotedSequence(inner)
                | WordPiece::GettextDoubleQuotedSequence(inner) => self.pieces(inner, scope),
                WordPiece::CommandSubstitution(text) => self.source(text, &mut scope.clone()),
                WordPiece::BackquotedCommandSubstitution(text) => {
                    let text = text
                        .replace("\\`", "`")
                        .replace("\\$", "$")
                        .replace("\\\\", "\\");
                    self.source(&text, &mut scope.clone());
                }
                _ => {}
            }
        }
    }

    fn simple(&mut self, command: &SimpleCommand, scope: &mut Scope) {
        let mut words: Vec<&Word> = Vec::new();
        let mut heredoc = None;
        let prefix = command.prefix.iter().flat_map(|prefix| &prefix.0);
        let suffix = command.suffix.iter().flat_map(|suffix| &suffix.0);
        for item in prefix {
            self.item(item, scope, &mut heredoc, None);
        }
        if let Some(name) = &command.word_or_name {
            self.substitutions(name, scope);
            words.push(name);
        }
        for item in suffix {
            self.item(item, scope, &mut heredoc, Some(&mut words));
        }
        if !words.is_empty() {
            self.run(&words, heredoc.as_deref(), scope);
        }
    }

    /// One word, assignment, or redirection around a simple command: walk
    /// what it runs, and keep an argument in `words`.
    fn item<'w>(
        &mut self,
        item: &'w CommandPrefixOrSuffixItem,
        scope: &mut Scope,
        heredoc: &mut Option<String>,
        words: Option<&mut Vec<&'w Word>>,
    ) {
        match item {
            CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                if let IoRedirect::HereDocument(_, doc) = redirect {
                    *heredoc = Some(doc.doc.value.clone());
                }
                self.redirect(redirect, scope);
            }
            CommandPrefixOrSuffixItem::Word(word) => {
                self.substitutions(word, scope);
                if let Some(words) = words {
                    words.push(word);
                }
            }
            CommandPrefixOrSuffixItem::AssignmentWord(assignment, word) => {
                match &assignment.value {
                    AssignmentValue::Scalar(value) => self.substitutions(value, scope),
                    AssignmentValue::Array(values) => {
                        for (key, value) in values {
                            if let Some(key) = key {
                                self.substitutions(key, scope);
                            }
                            self.substitutions(value, scope);
                        }
                    }
                }
                // `export A=b`: after the name, an assignment is an argument.
                if let Some(words) = words {
                    words.push(word);
                }
            }
            CommandPrefixOrSuffixItem::ProcessSubstitution(_, subshell) => {
                self.list(&subshell.list, &mut scope.clone());
            }
        }
    }

    /// Run `words` as a simple command does: the first is the program,
    /// unless it is a builtin, a function, or a wrapper that runs the words
    /// after it.
    fn run(&mut self, words: &[&Word], heredoc: Option<&str>, scope: &mut Scope) {
        let Some((&first, args)) = words.split_first() else {
            return;
        };
        let name = match self.literal(first) {
            Ok(name) => name,
            Err(reason) => return self.unchecked(first, reason),
        };
        if self.functions.contains(&name) {
            return;
        }
        match name.as_str() {
            "cd" | "pushd" => self.cd(&name, args, scope),
            "popd" => {
                scope.dir = scope.stack.pop().unwrap_or_else(|| {
                    Dir::Unknown("it runs after a `popd` with no `pushd` before it".to_owned())
                });
            }
            "exec" => {
                if let Some(rest) = self.after_options(&name, args, &["-a"], |option| {
                    matches!(option, "-c" | "-l" | "-cl" | "-lc")
                }) {
                    self.run(rest, heredoc, scope);
                }
            }
            "command" => {
                let probe = args.iter().any(|arg| {
                    self.literal(arg)
                        .is_ok_and(|arg| arg.starts_with('-') && arg.contains(['v', 'V']))
                });
                if probe {
                    self.probe("command -v", args);
                } else if let Some(rest) =
                    self.after_options(&name, args, &[], |option| option == "-p")
                {
                    self.run(rest, heredoc, scope);
                }
            }
            "type" | "hash" => self.probe(&name, args),
            "which" => {
                self.program(first, &name, scope);
                self.probe("which", args);
            }
            "test" | "[" => self.test_guard(args),
            "eval" => {
                let text: Result<Vec<String>, _> =
                    args.iter().map(|arg| self.literal(arg)).collect();
                match text {
                    Ok(text) => self.source(&text.join(" "), scope),
                    Err(_) => {
                        for arg in args {
                            self.unchecked(arg, "`eval` runs text a variable decides");
                        }
                    }
                }
            }
            "." | "source" => {
                if let Some(file) = args.first() {
                    self.script(file, scope);
                }
            }
            "env" => {
                self.program(first, &name, scope);
                self.env(args, heredoc, scope);
            }
            "nohup" => {
                self.program(first, &name, scope);
                if let Some(rest) = self.after_options(&name, args, &[], |_| false) {
                    self.run(rest, heredoc, scope);
                }
            }
            "nice" => {
                self.program(first, &name, scope);
                if let Some(rest) =
                    self.after_options(&name, args, &["-n", "--adjustment"], |option| {
                        option[1..].chars().all(|c| c.is_ascii_digit())
                    })
                {
                    self.run(rest, heredoc, scope);
                }
            }
            "timeout" => {
                self.program(first, &name, scope);
                if let Some(rest) = self.after_options(
                    &name,
                    args,
                    &["-s", "-k", "--signal", "--kill-after"],
                    |option| {
                        matches!(
                            option,
                            "-f" | "-p" | "-v" | "--foreground" | "--preserve-status" | "--verbose"
                        )
                    },
                ) {
                    // The duration, then the program.
                    self.run(rest.get(1..).unwrap_or_default(), heredoc, scope);
                }
            }
            "xargs" => {
                self.program(first, &name, scope);
                let with_value = &[
                    "-I",
                    "-n",
                    "-L",
                    "-P",
                    "-s",
                    "-d",
                    "-E",
                    "-a",
                    "-J",
                    "-R",
                    "-S",
                    "--max-args",
                    "--max-lines",
                    "--max-procs",
                    "--max-chars",
                    "--delimiter",
                    "--eof",
                    "--arg-file",
                    "--replace",
                    "--process-slot-var",
                ];
                if let Some(rest) = self.after_options(&name, args, with_value, |option| {
                    matches!(
                        option,
                        "-0" | "-r"
                            | "-t"
                            | "-p"
                            | "-x"
                            | "-o"
                            | "--null"
                            | "--no-run-if-empty"
                            | "--verbose"
                            | "--interactive"
                            | "--exit"
                            | "--open-tty"
                    ) || option.starts_with("-I")
                        || option.starts_with("-i")
                        || option.starts_with("-e")
                }) {
                    self.run(rest, None, scope);
                }
            }
            name if SHELLS.contains(&name) => {
                self.program(first, name, scope);
                self.shell(name, args, heredoc, scope);
            }
            name if BUILTINS.contains(&name) => {}
            name => self.program(first, name, scope),
        }
    }

    /// The words after a wrapper's options. `None`, with a `skip` for the
    /// word after it, when an option is one this does not know: which word
    /// is the program then depends on what it means.
    fn after_options<'w>(
        &mut self,
        wrapper: &str,
        args: &'w [&'w Word],
        with_value: &[&str],
        is_flag: impl Fn(&str) -> bool,
    ) -> Option<&'w [&'w Word]> {
        let mut index = 0;
        while let Some(&word) = args.get(index) {
            let Ok(option) = self.literal(word) else {
                return Some(&args[index..]);
            };
            if option == "--" {
                return Some(&args[index + 1..]);
            }
            if !option.starts_with('-') || option == "-" {
                return Some(&args[index..]);
            }
            let long_name = option.split_once('=').map(|(name, _)| name);
            if with_value.contains(&option.as_str()) {
                index += 2;
            } else if is_flag(&option)
                || long_name.is_some_and(|name| with_value.contains(&name))
                || with_value
                    .iter()
                    .any(|short| short.len() == 2 && option.starts_with(short))
            {
                index += 1;
            } else {
                if let Some(next) = args.get(index + 1) {
                    self.unchecked(
                        next,
                        format!("it follows `{wrapper} {option}`, an option newgit does not know"),
                    );
                }
                return None;
            }
        }
        Some(&[])
    }

    fn env(&mut self, args: &[&Word], heredoc: Option<&str>, scope: &mut Scope) {
        let mut scope = scope.clone();
        let mut index = 0;
        while let Some(&word) = args.get(index) {
            let Ok(arg) = self.literal(word) else {
                break;
            };
            let value_next = |index: usize| args.get(index + 1).and_then(|w| self.literal(w).ok());
            match arg.as_str() {
                "--" => {
                    index += 1;
                    break;
                }
                "-i" | "-" | "-0" | "-v" | "--ignore-environment" | "--null" | "--debug" => {
                    index += 1;
                }
                "-u" | "--unset" => index += 2,
                "-C" | "--chdir" => {
                    let dir = value_next(index);
                    self.chdir(&mut scope, dir.as_deref(), &arg);
                    index += 2;
                }
                "-S" | "--split-string" | "-P" => {
                    if let Some(next) = args.get(index + 1) {
                        self.unchecked(
                            next,
                            format!("`env {arg}` decides how it is split or searched for"),
                        );
                    }
                    return;
                }
                arg if arg.starts_with("--unset=") || (arg.starts_with("-u") && arg.len() > 2) => {
                    index += 1;
                }
                arg if arg.starts_with("--chdir=") || (arg.starts_with("-C") && arg.len() > 2) => {
                    let dir = arg
                        .strip_prefix("--chdir=")
                        .or_else(|| arg.strip_prefix("-C"))
                        .map(str::to_owned);
                    self.chdir(&mut scope, dir.as_deref(), arg);
                    index += 1;
                }
                arg if arg.starts_with('-') => {
                    if let Some(next) = args.get(index + 1) {
                        self.unchecked(
                            next,
                            format!("it follows `env {arg}`, an option newgit does not know"),
                        );
                    }
                    return;
                }
                arg if is_assignment(arg) => index += 1,
                _ => break,
            }
        }
        self.run(args.get(index..).unwrap_or_default(), heredoc, &mut scope);
    }

    /// `sh -c TEXT`, `sh FILE`, or `sh <<EOF`: what the child shell runs.
    fn shell(&mut self, shell: &str, args: &[&Word], heredoc: Option<&str>, scope: &mut Scope) {
        let mut text_next = false;
        // `sh -s`: the script is stdin, and the operands are its arguments.
        let mut from_stdin = false;
        let mut index = 0;
        while let Some(&word) = args.get(index) {
            let Ok(arg) = self.literal(word) else {
                break;
            };
            if arg == "--" {
                index += 1;
                break;
            }
            if arg == "-o" || arg == "+o" {
                index += 2;
                continue;
            }
            if arg.len() > 1 && (arg.starts_with('-') || arg.starts_with('+')) {
                if !arg.starts_with("--") && arg.contains('c') {
                    text_next = true;
                }
                if !arg.starts_with("--") && arg.contains('s') {
                    from_stdin = true;
                }
                index += 1;
                continue;
            }
            break;
        }
        let operand = args.get(index);
        match (text_next, operand) {
            (true, Some(&text)) => match self.literal(text) {
                Ok(text) => self.source(&text, &mut scope.clone()),
                Err(reason) => {
                    self.unchecked(
                        text,
                        format!("`{shell} -c` runs text newgit cannot read: {reason}"),
                    );
                }
            },
            (true, None) => {}
            (false, Some(&file)) if !from_stdin => self.script(file, scope),
            (false, _) => {
                if let Some(body) = heredoc {
                    self.source(body, &mut scope.clone());
                }
            }
        }
    }

    /// Record the probed names: the command decides for itself what to do
    /// when they are absent.
    fn probe(&mut self, probe: &str, args: &[&Word]) {
        for arg in args {
            if let Ok(name) = self.literal(arg)
                && !name.starts_with('-')
            {
                self.guards
                    .entry(name.clone())
                    .or_insert_with(|| format!("{probe} {name}"));
            }
        }
    }

    /// `[ -x ./bin/tool ]` probes for a file the way `command -v` probes
    /// for a program.
    fn test_guard(&mut self, args: &[&Word]) {
        let literals: Vec<String> = args
            .iter()
            .filter_map(|arg| self.literal(arg).ok())
            .collect();
        for pair in literals.windows(2) {
            if matches!(pair[0].as_str(), "-x" | "-f" | "-e" | "-s" | "-r") {
                self.guards
                    .entry(pair[1].clone())
                    .or_insert_with(|| format!("[ {} {} ]", pair[0], pair[1]));
            }
        }
    }

    fn cd(&mut self, name: &str, args: &[&Word], scope: &mut Scope) {
        let mut index = 0;
        while let Some(arg) = args.get(index).and_then(|arg| self.literal(arg).ok()) {
            if matches!(arg.as_str(), "-L" | "-P" | "-e" | "-@" | "-n") {
                index += 1;
            } else {
                if arg == "--" {
                    index += 1;
                }
                break;
            }
        }
        if name == "pushd" {
            scope.stack.push(scope.dir.clone());
        }
        match args.get(index) {
            None => {
                scope.dir = Dir::Unknown(format!(
                    "it runs after a bare `{name}`, which goes to the home directory"
                ));
            }
            Some(word) => match self.literal(word) {
                Ok(dir) => self.chdir(scope, Some(&dir), name),
                Err(reason) => {
                    scope.dir = Dir::Unknown(format!(
                        "it runs after `{name} {}`, and {reason}",
                        self.show(&word.value)
                    ));
                }
            },
        }
    }

    /// Move `scope` to `dir`, as `cd` (or `env -C`) does.
    fn chdir(&self, scope: &mut Scope, dir: Option<&str>, how: &str) {
        let Some(dir) = dir.filter(|dir| *dir != "-") else {
            scope.dir = Dir::Unknown(format!("it runs after a `{how}` newgit cannot follow"));
            return;
        };
        let target = if dir.starts_with('/') {
            normalize(Utf8Path::new(dir))
        } else {
            match &scope.dir {
                Dir::Known(current) => normalize(&current.join(dir)),
                Dir::Unknown(_) => return,
            }
        };
        scope.dir = if target.is_dir() {
            Dir::Known(target)
        } else {
            Dir::Unknown(format!(
                "it runs after `{how} {dir}`, and {} is not a directory before anything runs",
                self.display(&target)
            ))
        };
    }

    /// A script a shell reads (`sh FILE`, `. FILE`): it must exist, and
    /// need not be executable.
    fn script(&mut self, word: &Word, scope: &Scope) {
        let shown = self.show(&word.value);
        let name = match self.literal(word) {
            Ok(name) => name,
            Err(reason) => return self.unchecked(word, reason),
        };
        let in_dir = |dir: &Utf8Path| normalize(&dir.join(&name));
        let candidate = if name.starts_with('/') {
            Utf8PathBuf::from(&name)
        } else {
            match &scope.dir {
                Dir::Known(dir) => in_dir(dir),
                Dir::Unknown(reason) => return self.unchecked(word, reason.clone()),
            }
        };
        if candidate.is_file() {
            return self.push(Some(shown), Outcome::Found(candidate));
        }
        if !name.contains('/')
            && let Some(found) = self.on_path(&name, |path| path.is_file())
        {
            return self.push(Some(shown), Outcome::Found(found));
        }
        let outcome = self.absent(&candidate);
        self.push(Some(shown), outcome);
    }

    /// Look `name` up as the shell would run it.
    fn program(&mut self, word: &Word, name: &str, scope: &Scope) {
        let shown = self.show(&word.value);
        let outcome = if name.contains('/') {
            let candidate = if name.starts_with('/') {
                Utf8PathBuf::from(name)
            } else {
                match &scope.dir {
                    Dir::Known(dir) => normalize(&dir.join(name)),
                    Dir::Unknown(reason) => return self.unchecked(word, reason.clone()),
                }
            };
            if candidate.is_dir() {
                Outcome::Missing(format!("{} is a directory", self.display(&candidate)))
            } else if is_executable(&candidate) {
                Outcome::Found(candidate)
            } else if candidate.is_file() {
                let shown_path = self.display(&candidate);
                Outcome::Missing(format!(
                    "{shown_path} is not executable (`chmod +x {shown_path}`, or run it with `sh`)"
                ))
            } else {
                self.absent(&candidate)
            }
        } else {
            match self.on_path(name, is_executable) {
                Some(found) => Outcome::Found(found),
                None => Outcome::Missing("not found on PATH".to_owned()),
            }
        };
        let outcome = match (outcome, self.guards.get(name)) {
            (Outcome::Missing(reason), Some(probe)) => Outcome::Unchecked(format!(
                "{reason}, but the command checks for it first (`{probe}`)"
            )),
            (outcome, _) => outcome,
        };
        self.push(Some(shown), outcome);
    }

    /// A path that is not there: a failure, unless Git ignores it, in
    /// which case it is built rather than committed and an earlier step
    /// (an install, a build) may well make it.
    fn absent(&self, candidate: &Utf8Path) -> Outcome {
        let shown = self.display(candidate);
        if self.ignored(candidate) {
            Outcome::Unchecked(format!(
                "{shown} does not exist yet; Git ignores it, so something may build it first"
            ))
        } else {
            Outcome::Missing(format!("{shown} does not exist"))
        }
    }

    fn ignored(&self, candidate: &Utf8Path) -> bool {
        let Ok(relative) = candidate.strip_prefix(self.root) else {
            return false;
        };
        crate::git::command()
            .arg("-C")
            .arg(self.root.as_str())
            .args(["check-ignore", "-q", "--no-index", "--"])
            .arg(relative.as_str())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn on_path(&self, name: &str, usable: impl Fn(&Utf8Path) -> bool) -> Option<Utf8PathBuf> {
        self.path
            .split(':')
            .filter(|dir| !dir.is_empty())
            .map(|dir| Utf8Path::new(dir).join(name))
            .find(|candidate| usable(candidate))
    }

    /// A path as the output shows it: relative to the project root when it
    /// is under it.
    fn display(&self, path: &Utf8Path) -> String {
        match path.strip_prefix(self.root) {
            Ok(relative) if relative.as_str().is_empty() => "the project root".to_owned(),
            Ok(relative) => relative.to_string(),
            Err(_) => path.to_string(),
        }
    }

    /// The text a word stands for, when nothing but the definition decides
    /// it; `Err` says what else does.
    fn literal(&self, word: &Word) -> Result<String, String> {
        if let Some(index) = (0..self.templates.len())
            .find(|index| word.value.contains(&format!("{TEMPLATE_TOKEN}{index}__")))
        {
            return Err(format!(
                "`{}` decides it, and only a running instance knows its value",
                self.templates[index]
            ));
        }
        let pieces = brush_parser::word::parse(&word.value, &parser_options())
            .map_err(|_| "newgit could not read the word".to_owned())?;
        let mut text = String::new();
        literal_pieces(&pieces, &mut text, false)?;
        Ok(text)
    }
}

fn literal_pieces(
    pieces: &[WordPieceWithSource],
    text: &mut String,
    quoted: bool,
) -> Result<(), String> {
    for piece in pieces {
        match &piece.piece {
            WordPiece::Text(part) => {
                let glob = part.contains(['*', '?'])
                    || part
                        .find('[')
                        .is_some_and(|open| part[open..].contains(']'));
                if !quoted && glob {
                    return Err("a glob decides it".to_owned());
                }
                text.push_str(part);
            }
            WordPiece::SingleQuotedText(part) => text.push_str(part),
            WordPiece::EscapeSequence(part) => {
                text.push_str(part.strip_prefix('\\').unwrap_or(part));
            }
            WordPiece::DoubleQuotedSequence(inner)
            | WordPiece::GettextDoubleQuotedSequence(inner) => literal_pieces(inner, text, true)?,
            WordPiece::TildeExpansion(TildeExpr::Home) if text.is_empty() => {
                let home = std::env::var("HOME").map_err(|_| "`~` decides it".to_owned())?;
                text.push_str(&home);
            }
            WordPiece::TildeExpansion(_) => return Err("a `~` expansion decides it".to_owned()),
            WordPiece::AnsiCQuotedText(_) => return Err("`$'...'` quoting decides it".to_owned()),
            WordPiece::ParameterExpansion(_) => return Err("a variable decides it".to_owned()),
            WordPiece::CommandSubstitution(_) | WordPiece::BackquotedCommandSubstitution(_) => {
                return Err("a command's output decides it".to_owned());
            }
            WordPiece::ArithmeticExpression(_) => return Err("arithmetic decides it".to_owned()),
        }
    }
    Ok(())
}

/// Whether `pipeline` is `exit` or `return` (or a `{ ...; exit 1; }`
/// ending in one): what comes after it in the list never sees its `cd`.
fn exits(pipeline: &Pipeline) -> bool {
    match pipeline.seq.as_slice() {
        [Command::Simple(simple)] => simple
            .word_or_name
            .as_ref()
            .is_some_and(|word| matches!(word.value.as_str(), "exit" | "return")),
        [Command::Compound(CompoundCommand::BraceGroup(group), _)] => {
            group.list.0.last().is_some_and(|item| {
                let last = item
                    .0
                    .additional
                    .last()
                    .map_or(&item.0.first, |next| match next {
                        AndOr::And(pipeline) | AndOr::Or(pipeline) => pipeline,
                    });
                exits(last)
            })
        }
        _ => false,
    }
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// `path` with `.` and `..` resolved by name, as `cd` resolves them.
fn normalize(path: &Utf8Path) -> Utf8PathBuf {
    let mut normal = Utf8PathBuf::new();
    for component in path.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other.as_str()),
        }
    }
    normal
}

fn is_executable(path: &Utf8Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}
