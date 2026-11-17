//! Preflight for resource definitions (#87): does each program a hook's
//! command starts exist on this machine? A `[checkpoint] command` naming
//! `pg_dump` on a host with no Postgres client fails at the first checkpoint,
//! and a `[restore]` one only during the undo someone is relying on. This
//! finds both with no instance and no spawn, the way `render --check` checks
//! committed content.
//!
//! It reads the first word of each command in a shell string, not the shell
//! language: quotes, `;`, `&&`, `||`, `|`, `&`, newlines, and parentheses
//! are understood well enough to find where a command starts; a word with a
//! template (`{{scripts}}/x`) or a variable (`$TOOL`) in it is skipped, since
//! only a running instance knows what it becomes.

use camino::{Utf8Path, Utf8PathBuf};

use crate::resource::ResourceDefinition;

/// One program a definition's command starts, and whether it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramCheck {
    pub resource: String,
    /// Where the command is: `[actions.prepare]`, `[checkpoint]`,
    /// `[identity] key_command`, ...
    pub hook: String,
    pub program: String,
    /// Where it was found; `None` when it was not.
    pub found: Option<Utf8PathBuf>,
}

impl ProgramCheck {
    pub fn is_missing(&self) -> bool {
        self.found.is_none()
    }
}

/// Every command string `definition` holds, with where it is.
pub fn commands(definition: &ResourceDefinition) -> Vec<(String, String)> {
    let mut commands = Vec::new();
    for (name, action) in &definition.actions {
        if let Some(command) = &action.command {
            commands.push((format!("[actions.{name}]"), command.clone()));
        }
    }
    if let Some(command) = definition
        .identity
        .as_ref()
        .and_then(|identity| identity.key_command.clone())
    {
        commands.push(("[identity] key_command".to_owned(), command));
    }
    if let Some(command) = definition
        .checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint.command.clone())
    {
        commands.push(("[checkpoint]".to_owned(), command));
    }
    if let Some(command) = definition
        .restore
        .as_ref()
        .and_then(|restore| restore.command.clone())
    {
        commands.push(("[restore]".to_owned(), command));
    }
    if let Some(command) = definition
        .cleanup
        .as_ref()
        .and_then(|cleanup| cleanup.command.clone())
    {
        commands.push(("[cleanup]".to_owned(), command));
    }
    if let Some(command) = definition
        .ready
        .as_ref()
        .and_then(|ready| ready.command.clone())
    {
        commands.push(("[ready]".to_owned(), command));
    }
    commands
}

/// Check every program `definitions` start. `root` is the project root,
/// where a relative program (`./scripts/seed.sh`) is looked for, under the
/// command's `workdir` when it has one; `path` is the `PATH` to search.
pub fn check(definitions: &[ResourceDefinition], root: &Utf8Path, path: &str) -> Vec<ProgramCheck> {
    let mut checks = Vec::new();
    for definition in definitions {
        for (hook, command) in commands(definition) {
            let action = hook
                .strip_prefix("[actions.")
                .and_then(|rest| rest.strip_suffix(']'))
                .and_then(|name| definition.actions.get(name));
            let dir = match definition.workdir_for(action) {
                Some(workdir) => root.join(workdir),
                None => root.to_owned(),
            };
            let mut seen = Vec::new();
            for program in programs(&command) {
                if seen.contains(&program) {
                    continue;
                }
                let found = locate(&program, &dir, path);
                seen.push(program.clone());
                checks.push(ProgramCheck {
                    resource: definition.name.clone(),
                    hook: hook.clone(),
                    program,
                    found,
                });
            }
        }
    }
    checks
}

/// Where `program` would be found: a path with a `/` relative to `dir`, a
/// bare name on `path`.
fn locate(program: &str, dir: &Utf8Path, path: &str) -> Option<Utf8PathBuf> {
    if program.contains('/') {
        let candidate = if program.starts_with('/') {
            Utf8PathBuf::from(program)
        } else {
            dir.join(program)
        };
        return candidate.is_file().then_some(candidate);
    }
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Utf8Path::new(dir).join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Utf8Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// What the shell runs that is not a program on `PATH`: builtins and
/// keywords, and the words that run the word after them (`exec`,
/// `command`, `env`, `time`, `nohup`), which are followed through.
const SHELL_WORDS: &[&str] = &[
    "!", ".", ":", "[", "[[", "]]", "alias", "bg", "break", "case", "cd", "continue", "do", "done",
    "echo", "elif", "else", "esac", "eval", "exit", "export", "false", "fg", "fi", "for",
    "function", "getopts", "hash", "if", "in", "jobs", "kill", "local", "printf", "pwd", "read",
    "readonly", "return", "select", "set", "shift", "source", "test", "then", "times", "trap",
    "true", "type", "ulimit", "umask", "unalias", "unset", "until", "wait", "while", "{", "}",
];

/// Words that run the next word as the command.
const PREFIXES: &[&str] = &["exec", "command", "env", "time", "nohup", "builtin"];

/// The programs `command` starts, in order: the first word of each simple
/// command, past variable assignments and the prefixes in [`PREFIXES`].
/// Builtins, keywords, and words a template or variable decides are left
/// out.
pub fn programs(command: &str) -> Vec<String> {
    let mut programs = Vec::new();
    for segment in segments(command) {
        let Some(word) = segment.into_iter().find(|word| {
            !(is_assignment(word) || PREFIXES.contains(&word.as_str()) || word.starts_with('-'))
        }) else {
            continue;
        };
        if word.is_empty()
            || SHELL_WORDS.contains(&word.as_str())
            || word.contains("{{")
            || word.contains('$')
            || word.contains('`')
            || word.contains('*')
        {
            continue;
        }
        programs.push(word);
    }
    programs
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// `command` split into simple commands, each a list of its words with
/// quotes removed. Separators are `;`, `&`, `|`, newlines, and parentheses
/// outside quotes; a redirection target is dropped with its operator.
fn segments(command: &str) -> Vec<Vec<String>> {
    let mut segments = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = command.chars().peekable();
    let mut skip_next_word = false;
    // Here-documents opened on this line, read past at its end.
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    word.push(c);
                }
            }
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(next) = chars.next() {
                                word.push(next);
                            }
                        }
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(next) = chars.next()
                    && next != '\n'
                {
                    word.push(next);
                }
            }
            '#' if !in_word => {
                // A comment runs to the end of the line.
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
                finish_word(&mut words, &mut word, &mut in_word, &mut skip_next_word);
                segments.push(std::mem::take(&mut words));
            }
            '\n' if !heredocs.is_empty() => {
                finish_word(&mut words, &mut word, &mut in_word, &mut skip_next_word);
                segments.push(std::mem::take(&mut words));
                for (delimiter, strip_tabs) in std::mem::take(&mut heredocs) {
                    let mut line = String::new();
                    for c in chars.by_ref() {
                        if c != '\n' {
                            line.push(c);
                            continue;
                        }
                        let candidate = if strip_tabs {
                            line.trim_start_matches('\t')
                        } else {
                            line.as_str()
                        };
                        if candidate == delimiter {
                            break;
                        }
                        line.clear();
                    }
                }
            }
            ';' | '&' | '|' | '\n' | '(' | ')' => {
                finish_word(&mut words, &mut word, &mut in_word, &mut skip_next_word);
                segments.push(std::mem::take(&mut words));
            }
            '<' if chars.peek() == Some(&'<') && !here_string(&chars) => {
                // A here-document: its body, up to the line holding only its
                // delimiter, is input, not commands.
                chars.next();
                let strip_tabs = chars.next_if_eq(&'-').is_some();
                finish_word(&mut words, &mut word, &mut in_word, &mut skip_next_word);
                while chars.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
                let mut delimiter = String::new();
                while let Some(c) =
                    chars.next_if(|c| !c.is_whitespace() && !matches!(c, ';' | '&' | '|' | ')'))
                {
                    if !matches!(c, '\'' | '"' | '\\') {
                        delimiter.push(c);
                    }
                }
                heredocs.push((delimiter, strip_tabs));
            }
            '>' | '<' => {
                // `2>/dev/null`: the descriptor before it is part of the
                // redirection, not a word.
                if in_word && word.chars().all(|c| c.is_ascii_digit()) {
                    word.clear();
                    in_word = false;
                } else {
                    finish_word(&mut words, &mut word, &mut in_word, &mut skip_next_word);
                }
                while chars.peek().is_some_and(|c| matches!(c, '>' | '<' | '&')) {
                    chars.next();
                }
                // `2>&1` names a descriptor, not a file.
                if chars.peek().is_some_and(char::is_ascii_digit) {
                    while chars.peek().is_some_and(char::is_ascii_digit) {
                        chars.next();
                    }
                } else {
                    skip_next_word = true;
                }
            }
            c if c.is_whitespace() => {
                finish_word(&mut words, &mut word, &mut in_word, &mut skip_next_word);
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    finish_word(&mut words, &mut word, &mut in_word, &mut skip_next_word);
    segments.push(words);
    segments.retain(|segment| !segment.is_empty());
    segments
}

/// Whether the `<<` the iterator is at is the `<<<` of a here-string,
/// which redirects one word rather than opening a here-document.
fn here_string(chars: &std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    let mut ahead = chars.clone();
    ahead.next();
    ahead.peek() == Some(&'<')
}

/// End the word being read, if one is: kept, or dropped when it is a
/// redirection's target.
fn finish_word(words: &mut Vec<String>, word: &mut String, in_word: &mut bool, skip: &mut bool) {
    if *in_word {
        if *skip {
            *skip = false;
            word.clear();
        } else {
            words.push(std::mem::take(word));
        }
        *in_word = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_word_of_each_command() {
        assert_eq!(
            programs(
                "pg_dump --data-only \"$DATABASE_URL\" > {{snapshot.path}}/db.sql && gzip db.sql"
            ),
            ["pg_dump", "gzip"]
        );
        assert_eq!(
            programs("supabase stop >/dev/null 2>&1; supabase start"),
            ["supabase", "supabase"]
        );
        assert_eq!(programs("cat a | sort -u || true"), ["cat", "sort"]);
        assert_eq!(programs("(cd web && npm ci) &"), ["npm"]);
    }

    #[test]
    fn assignments_and_prefixes_are_passed_over() {
        assert_eq!(programs("FOO=1 BAR=\"a b\" node server.js"), ["node"]);
        assert_eq!(programs("exec python3 -m http.server"), ["python3"]);
        assert_eq!(programs("env -i PATH=/bin psql -c 'select 1'"), ["psql"]);
    }

    #[test]
    fn quotes_hide_separators() {
        assert_eq!(
            programs("python3 -c 'import os; os.exit(0)' && sleep 1"),
            ["python3", "sleep"]
        );
        assert_eq!(programs("sh -c \"a; b\""), ["sh"]);
    }

    #[test]
    fn what_only_an_instance_knows_is_skipped() {
        assert!(programs("{{scripts}}/seed.sh").is_empty());
        assert!(programs("$TOOL run").is_empty());
        assert!(programs("if test -f x; then echo y; fi").is_empty());
    }

    #[test]
    fn comments_and_redirections_are_not_programs() {
        assert_eq!(programs("# set up\nnpm ci # quietly\n"), ["npm"]);
        assert_eq!(programs("< input.sql psql"), ["psql"]);
    }

    #[test]
    fn a_here_document_is_input_not_commands() {
        assert_eq!(
            programs("psql \"$URL\" <<'SQL'\ndo $$ begin\n  truncate t;\nend $$;\nSQL\nnpm test"),
            ["psql", "npm"]
        );
        assert_eq!(
            programs("cat <<-EOF > out\n\tnot a command\n\tEOF\nsort out"),
            ["cat", "sort"]
        );
        assert_eq!(programs("grep x <<< \"a b\""), ["grep"]);
    }
}
