//! End-to-end tests of the binary: exit codes, db handling, import sources.
//! Each test points XDG_DATA_HOME at its own temp tree so the real db is
//! never touched.
//!
//! These cover what the shell snippets depend on and the unit tests cannot
//! reach: the exit status of a real process, and the source that import
//! picks given a particular environment.
//!
//! `search` is only tested for its no-terminal failure. Driving the picker
//! would need a pty.

use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

/// An isolated HOME-shaped directory: the db, any history file the test
/// writes, and the process cwd all live under it.
struct TempTree {
    root: TempDir,
}

impl TempTree {
    /// `name` only labels the temp directory, to make a leaked one on a
    /// failing run traceable back to its test.
    fn new(name: &str) -> Self {
        let root = tempfile::Builder::new()
            .prefix(&format!("padloper-cli-{name}-"))
            .tempdir()
            .unwrap();
        TempTree { root }
    }

    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let path = self.root.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// The binary under test, with the environment stripped to nothing the
    /// developer's machine can influence. Without the removals, import would
    /// find the real HISTFILE and the tests would depend on whose shell ran
    /// them. Returns the Command so a test can add back one variable.
    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_padloper"));
        cmd.args(args)
            .current_dir(self.root.path())
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env_remove("HOME")
            .env_remove("HISTFILE")
            .env_remove("PADLOPER_SESSION");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// A fresh db, one add, one list. Also the check that the db is created on
// first use rather than needing a setup step.
#[test]
fn add_then_list_round_trips_a_command() {
    let tree = TempTree::new("round-trip");
    assert_eq!(
        code(&tree.run(&["add", "--exit", "2", "--", "cargo build"])),
        0
    );

    let list = tree.run(&["list"]);

    assert_eq!(code(&list), 0);
    let line = stdout(&list);
    let fields: Vec<&str> = line.trim_end().split('\t').collect();
    assert_eq!(fields[0], "now");
    assert_eq!(fields[1], "2");
    assert_eq!(fields[2], "cargo build");
}

#[test]
fn adding_the_same_command_twice_keeps_one_row() {
    let tree = TempTree::new("dedup");
    tree.run(&["add", "--exit", "1", "--", "ls"]);
    tree.run(&["add", "--exit", "0", "--", "ls"]);

    let list = tree.run(&["list"]);

    let out = stdout(&list);
    assert_eq!(out.lines().count(), 1);
    assert!(out.contains("\t0\tls"));
}

// A bare Enter at the prompt reaches the hook as whitespace. It has to
// record nothing and still exit 0, since the hook passes $? through.
#[test]
fn an_empty_command_records_nothing() {
    let tree = TempTree::new("empty-add");
    assert_eq!(code(&tree.run(&["add", "--exit", "0", "--", "  "])), 0);
    assert_eq!(stdout(&tree.run(&["list"])), "");
}

#[test]
fn list_on_a_fresh_db_prints_nothing_and_exits_zero() {
    let tree = TempTree::new("fresh-list");
    let list = tree.run(&["list"]);
    assert_eq!(code(&list), 0);
    assert_eq!(stdout(&list), "");
}

#[test]
fn init_prints_the_snippet_for_each_shell() {
    let tree = TempTree::new("init");
    for shell in ["bash", "zsh", "fish"] {
        let init = tree.run(&["init", shell]);
        assert_eq!(code(&init), 0, "{shell}");
        assert!(stdout(&init).contains("padloper add"), "{shell}");
        assert!(stdout(&init).contains("padloper search"), "{shell}");
    }
}

// 2 is clap's usage error, not padloper's failure exit of 1.
#[test]
fn init_rejects_an_unknown_shell_with_a_usage_error() {
    let tree = TempTree::new("init-unknown");
    assert_eq!(code(&tree.run(&["init", "tcsh"])), 2);
}

#[test]
fn import_reads_the_histfile_and_is_idempotent() {
    let tree = TempTree::new("import-histfile");
    let histfile = tree.write("hist", "#100\nls -la\n#200\npwd\n");

    let mut cmd = tree.cmd(&["import"]);
    cmd.env("HISTFILE", &histfile);
    let import = cmd.output().unwrap();

    assert_eq!(code(&import), 0);
    assert_eq!(stdout(&import), "imported 2 commands\n");

    // Importing the same file again must merge onto the same two rows.
    // Nothing stops a user from running import twice.
    let mut cmd = tree.cmd(&["import"]);
    cmd.env("HISTFILE", &histfile);
    cmd.output().unwrap();

    assert_eq!(stdout(&tree.run(&["list"])).lines().count(), 2);
    // Newest first, so the file's own order survives the import and the
    // second pass does not shuffle it.
    let list = stdout(&tree.run(&["list"]));
    let commands: Vec<&str> = list
        .lines()
        .map(|line| line.rsplit('\t').next().unwrap())
        .collect();
    assert_eq!(commands, ["pwd", "ls -la"]);
}

// HISTFILE says nothing about which shell wrote the file, so the format has
// to come from the contents.
#[test]
fn import_detects_zsh_extended_history() {
    let tree = TempTree::new("import-zsh");
    let histfile = tree.write("hist", ": 100:2;printf foo\\\nbar\n: 200:0;pwd\n");
    let mut cmd = tree.cmd(&["import"]);
    cmd.env("HISTFILE", &histfile);

    let import = cmd.output().unwrap();

    assert_eq!(code(&import), 0, "{}", stderr(&import));
    let list = stdout(&tree.run(&["list"]));
    assert!(list.contains("printf foo\\"));
    assert!(list.contains("pwd"));
}

// With no HISTFILE, $SHELL picks the source. Fish is the only shell with a
// default path padloper will guess.
#[test]
fn import_discovers_fish_history_for_the_current_shell() {
    let tree = TempTree::new("import-fish");
    tree.write(
        "data/fish/fish_history",
        "- cmd: printf foo\\nbar\\\\baz\n  when: 100\n",
    );
    let mut cmd = tree.cmd(&["import"]);
    cmd.env("SHELL", "/bin/fish");

    let import = cmd.output().unwrap();

    assert_eq!(code(&import), 0, "{}", stderr(&import));
    assert_eq!(stdout(&import), "imported 1 commands\n");
    let list = stdout(&tree.run(&["list"]));
    assert!(list.contains("printf foo"));
}

// All or nothing. A partial import would look like a complete one, so the
// db must be untouched after the failure.
#[test]
fn malformed_history_imports_nothing() {
    let tree = TempTree::new("import-malformed");
    let histfile = tree.write("hist", "- cmd: bad\\q\n  when: 100\n");
    let mut cmd = tree.cmd(&["import"]);
    cmd.env("HISTFILE", &histfile);

    let import = cmd.output().unwrap();

    assert_eq!(code(&import), 1);
    assert!(stderr(&import).contains("cannot parse"));
    assert_eq!(stdout(&tree.run(&["list"])), "");
}

// Builds a real atuin db, since padloper reads its schema directly. The
// timestamps are nanoseconds, and row 'b' is a tombstone: atuin keeps
// deleted commands with deleted_at set, and importing one would resurrect
// something the user deleted on purpose.
#[test]
fn import_prefers_the_atuin_db_and_skips_tombstones() {
    let tree = TempTree::new("import-atuin");
    // Present only to be ignored: atuin outranks HISTFILE.
    let histfile = tree.write("hist", "from histfile\n");
    let atuin = tree
        .root
        .path()
        .join("data")
        .join("atuin")
        .join("history.db");
    std::fs::create_dir_all(atuin.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&atuin).unwrap();
    conn.execute_batch(
        "create table history (
           id text primary key, timestamp integer not null,
           duration integer not null, exit integer not null,
           command text not null, cwd text not null, session text not null,
           hostname text not null, deleted_at integer
         );
         insert into history values
           ('a', 1700000000000000000, 0, 0, 'cargo test', '/x', 's', 'h', null),
           ('b', 1700000001000000000, 0, 1, 'gone', '/x', 's', 'h', 1),
           ('c', 1700000002000000000, 0, 0, 'git log', '/x', 's', 'h', null);",
    )
    .unwrap();
    drop(conn);

    let mut command = tree.cmd(&["import"]);
    command.env("HISTFILE", histfile);
    let import = command.output().unwrap();

    assert_eq!(code(&import), 0);
    assert_eq!(stdout(&import), "imported 2 commands\n");
    let list = stdout(&tree.run(&["list"]));
    assert!(list.contains("git log"));
    assert!(list.contains("cargo test"));
    assert!(!list.contains("gone"));
    assert!(!list.contains("from histfile"));
}

// The next rung down: with no atuin db, an explicit HISTFILE wins over the
// history the shell would have been asked for.
#[test]
fn histfile_beats_the_shell_default() {
    let tree = TempTree::new("histfile-precedence");
    let histfile = tree.write("chosen", "from histfile\n");
    tree.write("data/fish/fish_history", "- cmd: from fish\n");
    let mut command = tree.cmd(&["import"]);
    command.env("HISTFILE", histfile).env("SHELL", "/bin/fish");

    let import = command.output().unwrap();

    assert_eq!(code(&import), 0, "{}", stderr(&import));
    let list = stdout(&tree.run(&["list"]));
    assert!(list.contains("from histfile"));
    assert!(!list.contains("from fish"));
}

// HISTFILE set to "" counts as unset, which is how an exported but empty
// variable behaves. The bash default takes over from there.
#[test]
fn bash_uses_its_default_history_when_histfile_is_empty() {
    let tree = TempTree::new("bash-default");
    let home = tree.root.path().join("home");
    tree.write("home/.bash_history", "from bash\n");
    let mut command = tree.cmd(&["import"]);
    command
        .env("HOME", &home)
        .env("HISTFILE", "")
        .env("SHELL", "/bin/bash");

    let import = command.output().unwrap();

    assert_eq!(code(&import), 0, "{}", stderr(&import));
    assert!(stdout(&tree.run(&["list"])).contains("from bash"));
}

// Zsh has no default history path. A ~/.bash_history sitting there is not
// this user's zsh history, so import asks for HISTFILE instead of taking it.
#[test]
fn zsh_does_not_guess_a_default_history_file() {
    let tree = TempTree::new("zsh-no-default");
    let home = tree.root.path().join("home");
    tree.write("home/.bash_history", "not zsh\n");
    let mut command = tree.cmd(&["import"]);
    command.env("HOME", home).env("SHELL", "/bin/zsh");

    let import = command.output().unwrap();

    assert_eq!(code(&import), 1);
    assert!(stderr(&import).contains("nothing to import"));
}

// Precedence picks a source once. After that a failure is reported, not
// worked around: silently importing ~/.bash_history because the file you
// named was missing would put the wrong history in the db.
#[test]
fn a_selected_missing_histfile_does_not_fall_back() {
    let tree = TempTree::new("missing-histfile");
    let home = tree.root.path().join("home");
    tree.write("home/.bash_history", "fallback\n");
    let mut command = tree.cmd(&["import"]);
    command
        .env("HOME", home)
        .env("SHELL", "/bin/bash")
        .env("HISTFILE", tree.root.path().join("missing"));

    let import = command.output().unwrap();

    assert_eq!(code(&import), 1);
    assert!(stderr(&import).contains("cannot read"));
    assert_eq!(stdout(&tree.run(&["list"])), "");
}

// The same rule one rung up. The atuin file exists but holds no history
// table, and HISTFILE is set: the error wins over the alternative.
#[test]
fn a_selected_broken_atuin_db_does_not_fall_back() {
    let tree = TempTree::new("broken-atuin");
    let histfile = tree.write("hist", "fallback\n");
    let atuin = tree
        .root
        .path()
        .join("data")
        .join("atuin")
        .join("history.db");
    std::fs::create_dir_all(atuin.parent().unwrap()).unwrap();
    rusqlite::Connection::open(&atuin).unwrap();
    let mut command = tree.cmd(&["import"]);
    command.env("HISTFILE", histfile);

    let import = command.output().unwrap();

    assert_eq!(code(&import), 1);
    assert!(stderr(&import).contains("cannot read the atuin db"));
    assert_eq!(stdout(&tree.run(&["list"])), "");
}

#[test]
fn import_with_no_source_fails_with_a_message() {
    let tree = TempTree::new("import-none");
    let import = tree.run(&["import"]);
    assert_eq!(code(&import), 1);
    assert!(stderr(&import).contains("padloper: nothing to import"));
}

// Under a test harness or a pipe there is no terminal. Failing beats
// writing escape sequences into whatever is reading.
#[test]
fn search_without_a_terminal_fails_with_a_message() {
    let tree = TempTree::new("search-notty");
    let search = tree.run(&["search"]);
    assert_eq!(code(&search), 1);
    assert!(stderr(&search).contains("padloper: search needs a terminal"));
}
