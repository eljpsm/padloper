//! Fill the db from Atuin, Bash, Zsh, or Fish history.
//!
//! Each shell owns its own on-disk format, and none of them are documented
//! as an interchange format. Every parser here links the upstream reference
//! it was written against.
//!
//! Import refuses a file it cannot read fully rather than keeping the lines
//! it understood. A partial import looks like a complete one, and the
//! command that got dropped is the one you go looking for later.

use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::db::{Db, DbError, Observation};
use crate::timefmt::unix_now;

/// `NoSource` means no atuin db, no HISTFILE, and no history file for
/// $SHELL. `Format` means the file was read but does not hold the format
/// detection said it did.
#[derive(Debug)]
pub enum ImportError {
    NoSource,
    Read { path: PathBuf, message: String },
    Format { path: PathBuf, message: String },
    Atuin { path: PathBuf, message: String },
    Db(DbError),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportError::NoSource => {
                write!(f, "nothing to import: no atuin db and no history file")
            }
            ImportError::Read { path, message } => {
                write!(f, "cannot read {}: {message}", path.display())
            }
            ImportError::Format { path, message } => {
                write!(f, "cannot parse {}: {message}", path.display())
            }
            ImportError::Atuin { path, message } => {
                write!(
                    f,
                    "cannot read the atuin db at {}: {message}",
                    path.display()
                )
            }
            ImportError::Db(e) => e.fmt(f),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Format {
    /// Read the format off the file itself. What HISTFILE gets, since the
    /// variable says nothing about which shell wrote it.
    Detect,
    Bash,
    Zsh,
    Fish,
}

/// Where the history is coming from. Atuin is a sqlite db read directly;
/// everything else is a text file needing a parser.
enum Source {
    Atuin(PathBuf),
    Text { path: PathBuf, format: Format },
}

/// Import into `db` and report how many rows were written. Safe to run more
/// than once: [`crate::db::Db::observe_all`] merges rather than appends.
pub fn run(db: &mut Db) -> Result<u64, ImportError> {
    let mut rows = match locate_source()? {
        Source::Atuin(path) => read_atuin(&path)?,
        Source::Text { path, format } => read_text(&path, format)?,
    };
    // Apply oldest first so the id refresh in OBSERVE leaves same-second
    // commands in the order they were run. The stable sort preserves file
    // order when timestamps tie, which is the best evidence available for
    // history that carries no timestamps at all.
    rows.sort_by_key(|row| row.ts);
    db.observe_all(rows).map_err(ImportError::Db)
}

/// Pick where to import from. Atuin wins, then HISTFILE, then the default
/// history file for $SHELL. The README states this order; keep them in step.
///
/// The choice is final. Once a source is picked, failing to read it is an
/// error, not a reason to try the next one: importing some other shell's
/// history because the chosen file was missing would be worse than failing.
fn locate_source() -> Result<Source, ImportError> {
    if let Ok(dir) = crate::db::data_dir() {
        let atuin = dir.join("atuin").join("history.db");
        if atuin.exists() {
            return Ok(Source::Atuin(atuin));
        }
    }
    if let Some(histfile) = nonempty_env("HISTFILE") {
        return Ok(Source::Text {
            path: histfile.into(),
            format: Format::Detect,
        });
    }

    match nonempty_env("SHELL")
        .as_deref()
        .and_then(|shell| Path::new(shell).file_name())
        .and_then(|name| name.to_str())
    {
        Some("fish") => fish_source(),
        // Zsh has no default history path. Guessing one risks importing
        // some other shell's file, so ask for HISTFILE instead.
        Some("zsh") => Err(ImportError::NoSource),
        Some("bash") => home_source(".bash_history", Format::Bash),
        // An unset or unrecognized SHELL is most often bash.
        _ => home_source(".bash_history", Format::Bash),
    }
}

fn nonempty_env(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

fn home_source(name: &str, format: Format) -> Result<Source, ImportError> {
    let Some(home) = nonempty_env("HOME") else {
        return Err(ImportError::NoSource);
    };
    existing_source(PathBuf::from(home).join(name), format)
}

fn fish_source() -> Result<Source, ImportError> {
    // Default history location:
    // https://fishshell.com/docs/current/interactive.html#searchable-command-history
    let Ok(data) = crate::db::data_dir() else {
        return Err(ImportError::NoSource);
    };
    existing_source(data.join("fish").join("fish_history"), Format::Fish)
}

fn existing_source(path: PathBuf, format: Format) -> Result<Source, ImportError> {
    if path.exists() {
        Ok(Source::Text { path, format })
    } else {
        Err(ImportError::NoSource)
    }
}

fn read_text(path: &Path, requested: Format) -> Result<Vec<Observation>, ImportError> {
    // Shell history files can hold arbitrary bytes, so decode lossily.
    let bytes = std::fs::read(path).map_err(|e| ImportError::Read {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    let text = String::from_utf8_lossy(&bytes);
    let format = match requested {
        Format::Detect => detect_format(&text),
        format => format,
    };
    parse_text(&text, format, unix_now()).map_err(|message| ImportError::Format {
        path: path.to_path_buf(),
        message,
    })
}

/// Guess a format from the first non-blank line. Only Zsh and Fish are
/// identifiable; Bash is the fallback because its plain form has no marker.
fn detect_format(text: &str) -> Format {
    if let Some(line) = text.lines().map(str::trim).find(|line| !line.is_empty()) {
        if line.starts_with("- cmd: ") {
            return Format::Fish;
        }
        if zsh_header(line).is_some() {
            return Format::Zsh;
        }
        return Format::Bash;
    }
    Format::Bash
}

fn parse_text(text: &str, format: Format, fallback_ts: i64) -> Result<Vec<Observation>, String> {
    match format {
        Format::Bash => Ok(parse_bash(text, fallback_ts)),
        Format::Zsh => parse_zsh(text),
        Format::Fish => parse_fish(text, fallback_ts),
        Format::Detect => unreachable!("format must be detected before parsing"),
    }
}

/// Parse Bash history. Cannot fail: any line is a valid command, so an
/// unrecognized file imports as one command per line.
///
/// With HISTTIMEFORMAT set, bash writes a `#<seconds>` line before each
/// entry, and those lines are the only delimiter a multiline command has.
/// Without it there is no timestamp at all and everything gets `fallback_ts`.
fn parse_bash(text: &str, fallback_ts: i64) -> Vec<Observation> {
    // Timestamp lines delimit multiline entries:
    // https://www.gnu.org/software/bash/manual/html_node/Bash-History-Facilities.html
    let mut out = Vec::new();
    // The entry being collected, if a timestamp line has opened one.
    let mut current: Option<(i64, Vec<&str>)> = None;
    // Before the first timestamp every line stands alone. A file can start
    // with untimed entries and pick up timestamps part way through.
    let mut saw_timestamp = false;

    for line in text.lines() {
        if let Some(ts) = timestamp_comment(line) {
            push_multiline(&mut out, current.take());
            current = Some((ts, Vec::new()));
            saw_timestamp = true;
        } else if saw_timestamp {
            if let Some((_, lines)) = &mut current {
                lines.push(line);
            }
        } else {
            push_observation(&mut out, line, fallback_ts);
        }
    }
    push_multiline(&mut out, current);
    out
}

/// A bash `#<seconds>` line. Digits only, so a comment someone typed at the
/// prompt stays a command.
fn timestamp_comment(line: &str) -> Option<i64> {
    let digits = line.strip_prefix('#')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn push_multiline(out: &mut Vec<Observation>, entry: Option<(i64, Vec<&str>)>) {
    if let Some((ts, lines)) = entry {
        push_observation(out, &lines.join("\n"), ts);
    }
}

/// Parse Zsh EXTENDED_HISTORY, `: <start>:<elapsed>;<command>`.
///
/// Strict, unlike the bash parser. A file that looks like zsh history but
/// holds a broken record is more likely a mangled file than a shell padloper
/// should guess at, and every entry here carries a real timestamp to lose.
fn parse_zsh(text: &str) -> Result<Vec<Observation>, String> {
    // EXTENDED_HISTORY record format:
    // https://zsh.sourceforge.io/Doc/Release/Options.html#History
    let mut out = Vec::new();
    let mut current: Option<(i64, String)> = None;

    for (index, line) in text.lines().enumerate() {
        if let Some((ts, cmd)) = zsh_header(line) {
            push_zsh(&mut out, current.take());
            current = Some((ts, cmd.to_string()));
        } else if line.starts_with(": ") {
            // Looks like a record and did not parse as one. A continuation
            // line would be a command starting with ": ", which is rare
            // enough to be worth reporting instead of guessing.
            return Err(format!("line {} has an invalid Zsh record", index + 1));
        } else if let Some((_, cmd)) = &mut current {
            // Zsh escapes the newline in a multiline command, so anything
            // after a header belongs to it.
            cmd.push('\n');
            cmd.push_str(line);
        } else if !line.trim().is_empty() {
            return Err(format!(
                "line {} mixes plain and Zsh extended history",
                index + 1
            ));
        }
    }
    push_zsh(&mut out, current);
    Ok(out)
}

/// Split `: <start>:<elapsed>;<command>` into its start time and command.
/// Both numbers must be digits, which is what keeps a plain command
/// beginning with ": " from reading as a header.
fn zsh_header(line: &str) -> Option<(i64, &str)> {
    let fields = line.strip_prefix(": ")?;
    let (timestamp, rest) = fields.split_once(':')?;
    let (duration, cmd) = rest.split_once(';')?;
    if timestamp.is_empty()
        || duration.is_empty()
        || !timestamp.bytes().all(|b| b.is_ascii_digit())
        || !duration.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some((timestamp.parse().ok()?, cmd))
}

fn push_zsh(out: &mut Vec<Observation>, entry: Option<(i64, String)>) {
    if let Some((ts, cmd)) = entry {
        push_observation(out, &cmd, ts);
    }
}

/// Parse Fish history. Strict, for the same reason as [`parse_zsh`].
///
/// The format looks like YAML but is not, so this reads it line by line the
/// way fish writes it. `when` is optional in principle, hence `fallback_ts`.
fn parse_fish(text: &str, fallback_ts: i64) -> Result<Vec<Observation>, String> {
    // Fish owns this YAML-like format and escape scheme:
    // https://github.com/fish-shell/fish-shell/blob/9654f5e4bd00066e8d0db7fdb66e7b12458f8f4e/src/history/yaml_backend.rs#L8-L70
    let mut out = Vec::new();
    let mut current: Option<(String, i64)> = None;

    for (index, line) in text.lines().enumerate() {
        if let Some(encoded) = line.strip_prefix("- cmd: ") {
            push_fish(&mut out, current.take());
            let cmd = unescape_fish(encoded)
                .map_err(|message| format!("line {} {message}", index + 1))?;
            current = Some((cmd, fallback_ts));
        } else if let Some(value) = line.strip_prefix("  when: ") {
            let Some((_, ts)) = &mut current else {
                return Err(format!("line {} has metadata before a command", index + 1));
            };
            *ts = value
                .parse()
                .map_err(|_| format!("line {} has an invalid Fish timestamp", index + 1))?;
        } else if line.trim().is_empty() || line.starts_with(' ') {
            // Other indented keys, `paths` and its list items. Fish records
            // which files a command touched; padloper has no use for it.
            continue;
        } else {
            return Err(format!("line {} has an invalid Fish record", index + 1));
        }
    }
    push_fish(&mut out, current);
    Ok(out)
}

/// Undo the two escapes fish writes, `\n` and `\\`. An unknown escape is an
/// error rather than a literal backslash, because guessing would silently
/// import a command that differs from the one that ran.
fn unescape_fish(text: &str) -> Result<String, &'static str> {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(_) => return Err("has an unsupported Fish escape"),
            None => return Err("ends with an incomplete Fish escape"),
        }
    }
    Ok(out)
}

fn push_fish(out: &mut Vec<Observation>, entry: Option<(String, i64)>) {
    if let Some((cmd, ts)) = entry {
        push_observation(out, &cmd, ts);
    }
}

/// Append a command, dropping blanks. Imported history carries no cwd, exit
/// status, or session; those columns fill in once the command runs again.
fn push_observation(out: &mut Vec<Observation>, cmd: &str, ts: i64) {
    if !cmd.trim().is_empty() {
        out.push(Observation {
            cmd: cmd.to_string(),
            cwd: None,
            exit: None,
            ts,
            session: None,
        });
    }
}

/// Read an atuin history db. It is the richest source: cwd, exit status, and
/// session all come across.
///
/// Atuin stores nanoseconds and blanks deleted commands, so scale down and
/// skip tombstones. Opened read-only, since atuin may be running.
fn read_atuin(path: &Path) -> Result<Vec<Observation>, ImportError> {
    let atuin_err = |message: String| ImportError::Atuin {
        path: path.to_path_buf(),
        message,
    };
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| atuin_err(e.to_string()))?;
    let mut stmt = conn
        .prepare(
            "select command, cwd, exit, timestamp / 1000000000, session
             from history where deleted_at is null
             order by timestamp, id",
        )
        .map_err(|e| atuin_err(e.to_string()))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Observation {
                cmd: r.get(0)?,
                cwd: r.get(1)?,
                exit: r.get(2)?,
                ts: r.get(3)?,
                session: r.get(4)?,
            })
        })
        .map_err(|e| atuin_err(e.to_string()))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| atuin_err(e.to_string()))
}

// The parsers run against real history files exactly once per machine, and
// a bad one loses history quietly. These fix each format on samples small
// enough to read, plus the malformed input each parser must reject.
#[cfg(test)]
mod tests {
    use super::*;

    fn commands(rows: &[Observation]) -> Vec<(&str, i64)> {
        rows.iter().map(|row| (row.cmd.as_str(), row.ts)).collect()
    }

    #[test]
    fn detects_structured_history_formats() {
        assert_eq!(detect_format("#100\nls\n"), Format::Bash);
        assert_eq!(detect_format(": 100:0;ls\n"), Format::Zsh);
        assert_eq!(detect_format("- cmd: ls\n  when: 100\n"), Format::Fish);
        // A blank file and an unmarked line both land on Bash, the only
        // parser that accepts anything.
        assert_eq!(detect_format("\n  \n"), Format::Bash);
        assert_eq!(detect_format("plain\n"), Format::Bash);
    }

    // "plain" precedes the first timestamp and stands alone; the two lines
    // after #100 are one command, because only a timestamp ends an entry.
    #[test]
    fn bash_timestamps_delimit_multiline_commands() {
        let rows = parse_bash("plain\n#100\nprintf foo\nbar\n#200\npwd\n", 999);
        assert_eq!(
            commands(&rows),
            [("plain", 999), ("printf foo\nbar", 100), ("pwd", 200)]
        );
    }

    #[test]
    fn bash_without_timestamps_keeps_one_command_per_line() {
        let rows = parse_bash("ls\npwd\n", 999);
        assert_eq!(commands(&rows), [("ls", 999), ("pwd", 999)]);
    }

    // Three near misses that all stay commands: a bare "#", a comment, and
    // a number one past i64. The last is why the parse result is checked
    // rather than the digits alone. The two real timestamps open entries
    // that never get a command, and empty entries are dropped.
    #[test]
    fn bash_rejects_timestamp_lookalikes_and_drops_empty_entries() {
        let rows = parse_bash("#\n#nope\n#9223372036854775808\n#100\n#200\npwd\n", 999);
        assert_eq!(
            commands(&rows),
            [
                ("#", 999),
                ("#nope", 999),
                ("#9223372036854775808", 999),
                ("pwd", 200)
            ]
        );
    }

    // The trailing backslash is zsh's own escaped newline, so it stays in
    // the command text along with the newline it escapes.
    #[test]
    fn zsh_extended_history_keeps_metadata_and_continuations() {
        let rows = parse_zsh(": 100:2;printf foo\\\nbar\n: 200:0;pwd\n").expect("parse");
        assert_eq!(commands(&rows), [("printf foo\\\nbar", 100), ("pwd", 200)]);
    }

    #[test]
    fn zsh_rejects_mixed_or_malformed_records() {
        // A plain line before any header: the file is not what detection
        // said it was, and the rest cannot be trusted either.
        assert!(parse_zsh("plain\n: 100:0;ls\n").is_err());
        // Header shape with a non-numeric timestamp.
        assert!(parse_zsh(": nope:0;ls\n").is_err());
        // Non-numeric duration.
        assert!(parse_zsh(": 100:nope;ls\n").is_err());
        // No semicolon, so there is no command field at all.
        assert!(parse_zsh(": 100:0ls\n").is_err());
        // A blank leading line is not a mixed-format file. Zsh writes one
        // after a partial write, and it must not fail the import.
        assert_eq!(
            commands(&parse_zsh("\n: 100:0;ls\n").expect("blank prefix")),
            [("ls", 100)]
        );
    }

    // Both escapes in one command, followed by the `paths` block that has
    // to be skipped without ending the entry.
    #[test]
    fn fish_history_decodes_commands_and_ignores_metadata() {
        let rows = parse_fish(
            "- cmd: printf foo\\nbar\\\\baz\n  when: 100\n  paths:\n    - /tmp\n",
            999,
        )
        .expect("parse");
        assert_eq!(commands(&rows), [("printf foo\nbar\\baz", 100)]);
    }

    #[test]
    fn fish_rejects_malformed_records() {
        // Metadata with no command to attach it to.
        assert!(parse_fish("  when: 100\n", 999).is_err());
        // An escape fish does not write. Treating it as a literal would
        // import a command that differs from the one that ran.
        assert!(parse_fish("- cmd: bad\\q\n", 999).is_err());
        // A backslash at end of line, so the escape has nothing to escape.
        assert!(parse_fish("- cmd: bad\\\n", 999).is_err());
        // A `when` that is not a number.
        assert!(parse_fish("- cmd: ls\n  when: nope\n", 999).is_err());
        // Not fish history at all.
        assert!(parse_fish("plain\n", 999).is_err());
    }

    // A record whose `when` is missing still imports, dated now. Losing the
    // command would be worse than dating it wrong.
    #[test]
    fn fish_uses_the_fallback_time_and_keeps_multiple_records() {
        let rows = parse_fish("- cmd: first\n- cmd: second\n  when: 200\n", 999).expect("parse");
        assert_eq!(commands(&rows), [("first", 999), ("second", 200)]);
    }

    // Import failures point at a file the user can go look at, so every
    // variant has to carry its path into the message. Db defers to DbError
    // rather than adding a second prefix.
    #[test]
    fn import_errors_keep_their_source() {
        assert_eq!(
            ImportError::Read {
                path: "/tmp/hist".into(),
                message: "denied".into(),
            }
            .to_string(),
            "cannot read /tmp/hist: denied"
        );
        assert_eq!(
            ImportError::Format {
                path: "/tmp/hist".into(),
                message: "bad record".into(),
            }
            .to_string(),
            "cannot parse /tmp/hist: bad record"
        );
        assert_eq!(
            ImportError::Atuin {
                path: "/tmp/atuin.db".into(),
                message: "bad schema".into(),
            }
            .to_string(),
            "cannot read the atuin db at /tmp/atuin.db: bad schema"
        );
        assert_eq!(
            ImportError::Db(DbError::Query("stopped".into())).to_string(),
            "database error: stopped"
        );
    }

    // The two ways reading a history file fails, told apart by variant so
    // the message says which. The 0xff byte is not valid UTF-8: history
    // files hold arbitrary bytes, and the lossy decode has to get past it
    // and leave the parser to reject the record on its own terms.
    #[test]
    fn text_reader_reports_read_and_format_errors() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("missing");
        assert!(matches!(
            read_text(&missing, Format::Detect),
            Err(ImportError::Read { path, .. }) if path == missing
        ));

        let malformed = dir.path().join("fish_history");
        std::fs::write(&malformed, b"- cmd: bad\\q\xff\n").expect("write history");
        assert!(matches!(
            read_text(&malformed, Format::Fish),
            Err(ImportError::Format { path, .. }) if path == malformed
        ));
    }

    // Rows are inserted out of order to prove the query sorts by timestamp,
    // and the nanosecond timestamps come back as 1 and 2 seconds. The empty
    // db at the end has no history table: a file that is not an atuin db
    // has to fail, not import as nothing.
    #[test]
    fn atuin_reader_preserves_metadata_and_reports_bad_databases() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("atuin.db");
        let conn = Connection::open(&path).expect("open atuin");
        conn.execute_batch(
            "create table history (
               id text primary key, timestamp integer, exit integer,
               command text, cwd text, session text, deleted_at integer
             );
             insert into history values
               ('b', 2000000000, 2, 'second', '/b', 's2', null),
               ('a', 1000000000, 1, 'first', '/a', 's1', null),
               ('x', 3000000000, 0, 'deleted', '/x', 'sx', 1);",
        )
        .expect("seed atuin");
        drop(conn);

        let rows = read_atuin(&path).expect("read atuin");
        assert_eq!(commands(&rows), [("first", 1), ("second", 2)]);
        assert_eq!(rows[0].cwd.as_deref(), Some("/a"));
        assert_eq!(rows[0].exit, Some(1));
        assert_eq!(rows[0].session.as_deref(), Some("s1"));

        let malformed = dir.path().join("malformed.db");
        Connection::open(&malformed).expect("open malformed");
        assert!(matches!(
            read_atuin(&malformed),
            Err(ImportError::Atuin { path, .. }) if path == malformed
        ));
    }
}
