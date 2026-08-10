//! Subcommand dispatch. The one place errors are rendered.
//!
//! Every fallible step returns `anyhow::Result` and its message is printed
//! here, prefixed once, so the modules below never write to stderr and never
//! decide an exit code.

use std::io::Write;
use std::process::ExitCode;

use crate::cli::{Cli, Command};
use crate::timefmt::{relative_time, unix_now};
use crate::{db, import, tui};

/// Run a parsed command line and report how it went. The only exit code
/// other than success and failure comes from `search`.
pub fn run(cli: Cli) -> ExitCode {
    match execute(cli.command) {
        Ok(status) => status,
        Err(err) => {
            eprintln!("padloper: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn execute(command: Command) -> anyhow::Result<ExitCode> {
    match command {
        Command::Init { shell } => {
            print!("{}", shell.snippet());
            Ok(ExitCode::SUCCESS)
        }
        Command::Add { exit, cmd } => success(add(exit, &cmd.join(" "))),
        Command::Search { query } => search(&query.join(" ")),
        Command::List => success(list()),
        Command::Import => success(import()),
    }
}

fn success(result: anyhow::Result<()>) -> anyhow::Result<ExitCode> {
    result.map(|()| ExitCode::SUCCESS)
}

/// Record one command. Runs on every prompt, so it must stay quiet and
/// cheap; a bare Enter reaches here as an empty command and records nothing.
fn add(exit: i32, cmd: &str) -> anyhow::Result<()> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return Ok(());
    }
    let db = db::open()?;
    let cwd = current_dir_string();
    let session = std::env::var("PADLOPER_SESSION").ok();
    db.observe(db::Observation {
        cmd: cmd.to_string(),
        cwd,
        exit: Some(exit.into()),
        ts: unix_now(),
        session,
    })
}

/// Load history, run the picker, and hand the outcome to the shell widget.
fn search(initial_query: &str) -> anyhow::Result<ExitCode> {
    let db = db::open()?;
    // A bounded read keeps startup flat on a large db. Fuzzy matching runs
    // over every row on each keystroke, so this is also the latency budget.
    let rows = db.recent(10000)?;
    let outcome = tui::run(rows, initial_query, current_dir_string())?;
    Ok(emit_search(outcome, std::io::stdout().lock())?)
}

/// The recorder's cwd and the picker's dir-only scope must agree, so both
/// come from here.
fn current_dir_string() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// The protocol the shell widgets decode. Stdout is command data. The status
/// is the action: 0 runs, 10 inserts, and 130 cancels.
///
/// Keeping the action out of stdout means a command that looks like a
/// marker stays ordinary text. The three snippets in [`crate::shell`]
/// switch on these numbers, so changing one means changing all of them.
fn emit_search(outcome: tui::Outcome, mut out: impl Write) -> std::io::Result<ExitCode> {
    match outcome {
        tui::Outcome::Run(cmd) => {
            writeln!(out, "{cmd}")?;
            Ok(ExitCode::SUCCESS)
        }
        tui::Outcome::Insert(cmd) => {
            writeln!(out, "{cmd}")?;
            Ok(ExitCode::from(10))
        }
        tui::Outcome::Cancelled => Ok(ExitCode::from(130)),
    }
}

/// Print recent history as time, exit, and cmd, tab separated. Multiline
/// commands print their newlines, so a consumer that splits on lines sees
/// more rows than it asked for.
fn list() -> anyhow::Result<()> {
    let db = db::open()?;
    let now = unix_now();
    let mut out = std::io::stdout().lock();
    write_list(db.list()?, now, &mut out);
    Ok(())
}

/// Split from [`list`] so the closed-pipe path can be tested against a
/// writer that fails on demand, rather than a real pipe.
fn write_list(rows: impl IntoIterator<Item = db::ListRow>, now: i64, mut out: impl Write) {
    for row in rows {
        let exit = row.exit.map(|e| e.to_string()).unwrap_or_default();
        // A closed pipe (list | head) ends the listing. Not a failure.
        if writeln!(out, "{}\t{}\t{}", relative_time(now, row.ts), exit, row.cmd).is_err() {
            break;
        }
    }
}

fn import() -> anyhow::Result<()> {
    let mut db = db::open()?;
    let count = import::run(&mut db)?;
    println!("imported {count} commands");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for `list | head -1`: takes one line, then reports the
    /// broken pipe every later write would get.
    struct OneLineWriter {
        bytes: Vec<u8>,
        closed: bool,
    }

    impl Write for OneLineWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.closed {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed",
                ));
            }
            self.bytes.extend_from_slice(buf);
            if buf.contains(&b'\n') {
                self.closed = true;
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // The insert case carries what an older revision used as an inline
    // action marker. It has to survive as ordinary text now.
    #[test]
    fn search_actions_keep_control_out_of_command_text() {
        let mut out = Vec::new();
        let status = emit_search(tui::Outcome::Run("echo run".into()), &mut out).expect("emit");
        assert_eq!(status, ExitCode::SUCCESS);
        assert_eq!(out, b"echo run\n");

        out.clear();
        let status = emit_search(
            tui::Outcome::Insert("__padloper_run__:echo edit".into()),
            &mut out,
        )
        .expect("emit");
        assert_eq!(status, ExitCode::from(10));
        assert_eq!(out, b"__padloper_run__:echo edit\n");

        out.clear();
        let status = emit_search(tui::Outcome::Cancelled, &mut out).expect("emit");
        assert_eq!(status, ExitCode::from(130));
        assert!(out.is_empty());
    }

    // Two things at once. An imported row has no exit status, so that field
    // prints empty and the line keeps all three columns (hence the two tabs
    // in a row). And "second" never appears, because the broken pipe ends
    // the listing instead of failing the command.
    #[test]
    fn list_formats_missing_status_and_stops_on_a_closed_pipe() {
        let rows = vec![
            db::ListRow {
                ts: 100,
                exit: None,
                cmd: "first".into(),
            },
            db::ListRow {
                ts: 50,
                exit: Some(1),
                cmd: "second".into(),
            },
        ];
        let mut out = OneLineWriter {
            bytes: Vec::new(),
            closed: false,
        };

        write_list(rows, 100, &mut out);

        assert_eq!(out.bytes, b"now\t\tfirst\n");
    }
}
