//! The history database. One row per distinct command; running a command
//! again refreshes its row instead of adding one.
//!
//! So the command text is the identity, and cwd, exit, ts, and session all
//! describe its most recent run. There is no execution log. That is what
//! keeps the file small and the search fast, and it is why history cannot
//! answer "how often" or "where else did I run this".

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, params};

// `unique` on cmd is what makes the row a projection rather than a log; the
// upsert in OBSERVE depends on it. The index covers the recency ordering
// both reads use, so neither sorts.
const SCHEMA: &str = "
create table if not exists history (
  id integer primary key autoincrement,
  cmd text not null unique,
  cwd text,
  exit integer,
  ts integer,
  session text
);
create index if not exists history_ts_id_cmd on history(ts desc, id desc, cmd);
";

/// An open history database. Cheap to create, so every subcommand opens its
/// own and drops it on exit.
pub struct Db {
    conn: Connection,
}

/// A row as the picker needs it: the text to match, the age to show, and
/// the directory its last run happened in for the dir-only scope.
pub struct HistoryRow {
    pub cmd: String,
    /// Unix seconds.
    pub ts: i64,
    /// None for imported rows, which no dir-only search can match.
    pub cwd: Option<String>,
}

/// A row as `padloper list` prints it.
pub struct ListRow {
    /// Unix seconds.
    pub ts: i64,
    /// None for imported rows, which carry no status.
    pub exit: Option<i64>,
    pub cmd: String,
}

/// One sighting of a command. Everything but `cmd` and `ts` is optional
/// because imported history rarely records it.
pub struct Observation {
    pub cmd: String,
    pub cwd: Option<String>,
    pub exit: Option<i64>,
    /// Unix seconds.
    pub ts: i64,
    pub session: Option<String>,
}

#[derive(Debug)]
pub enum DbError {
    /// No data directory to put the db in.
    NoHome,
    /// The file could not be created, opened, or brought up to schema.
    Open { path: PathBuf, message: String },
    /// A statement failed against an open db.
    Query(String),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::NoHome => {
                write!(
                    f,
                    "cannot locate the data directory: XDG_DATA_HOME and HOME are unset"
                )
            }
            DbError::Open { path, message } => {
                write!(f, "cannot open {}: {message}", path.display())
            }
            DbError::Query(message) => write!(f, "database error: {message}"),
        }
    }
}

/// The XDG data root, `~/.local/share` by default. The import source search
/// uses this too, so atuin and fish history are found the same way.
pub(crate) fn data_dir() -> Result<PathBuf, DbError> {
    data_dir_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

/// Takes the two variables as arguments so the precedence is testable.
/// Setting env vars in a test would race every other test in the process.
///
/// An empty variable counts as unset, which is what an exported but
/// never-assigned `HISTFILE` or `XDG_DATA_HOME` looks like.
fn data_dir_from(xdg: Option<OsString>, home: Option<OsString>) -> Result<PathBuf, DbError> {
    if let Some(dir) = xdg.filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    match home {
        Some(home) if !home.is_empty() => Ok(PathBuf::from(home).join(".local").join("share")),
        _ => Err(DbError::NoHome),
    }
}

/// Open the real history db, creating it on first use.
pub fn open() -> Result<Db, DbError> {
    let path = data_dir()?.join("padloper").join("history.db");
    Db::open_at(&path)
}

impl Db {
    /// Open or create a db at `path`, then apply the schema. Safe to call on
    /// an existing file: every statement in SCHEMA is `if not exists`.
    ///
    /// WAL and the busy timeout are what let a prompt hook write while a
    /// picker in another terminal reads. Without them a concurrent write
    /// fails instead of waiting.
    pub(crate) fn open_at(path: &Path) -> Result<Db, DbError> {
        let open_err = |message: String| DbError::Open {
            path: path.to_path_buf(),
            message,
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| open_err(e.to_string()))?;
        }
        let conn = Connection::open(path).map_err(|e| open_err(e.to_string()))?;
        conn.busy_timeout(Duration::from_millis(5000))
            .map_err(|e| open_err(e.to_string()))?;
        conn.pragma_update(None, "journal_mode", "wal")
            .map_err(|e| open_err(e.to_string()))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| open_err(e.to_string()))?;
        Ok(Db { conn })
    }

    /// Record one command. Inserts, or refreshes the existing row under the
    /// rules in [`OBSERVE`].
    pub fn observe(&self, row: Observation) -> Result<(), DbError> {
        self.conn
            .execute(
                OBSERVE,
                params![row.cmd, row.cwd, row.exit, row.ts, row.session],
            )
            .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(())
    }

    /// The newest `limit` commands, newest first. The picker relies on that
    /// order for its tiebreak, so do not reorder here.
    pub fn recent(&self, limit: usize) -> Result<Vec<HistoryRow>, DbError> {
        let query_err = |e: rusqlite::Error| DbError::Query(e.to_string());
        let mut stmt = self
            .conn
            .prepare("select cmd, ts, cwd from history order by ts desc, id desc limit ?1")
            .map_err(query_err)?;
        let rows = stmt
            .query_map([limit as i64], |r| {
                Ok(HistoryRow {
                    cmd: r.get(0)?,
                    ts: r.get(1)?,
                    cwd: r.get(2)?,
                })
            })
            .map_err(query_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(query_err)
    }

    /// The 50 newest commands, newest first, for `padloper list`.
    pub fn list(&self) -> Result<Vec<ListRow>, DbError> {
        let query_err = |e: rusqlite::Error| DbError::Query(e.to_string());
        let mut stmt = self
            .conn
            .prepare("select ts, exit, cmd from history order by ts desc, id desc limit 50")
            .map_err(query_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ListRow {
                    ts: r.get(0)?,
                    exit: r.get(1)?,
                    cmd: r.get(2)?,
                })
            })
            .map_err(query_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(query_err)
    }

    /// Record many commands in one transaction, the shape import needs: a
    /// failure part way through leaves the db untouched.
    ///
    /// Returns how many rows were written, which is not the row count of the
    /// db. Blank commands are skipped, and repeats collapse onto one row.
    pub fn observe_all<I>(&mut self, rows: I) -> Result<u64, DbError>
    where
        I: IntoIterator<Item = Observation>,
    {
        let query_err = |e: rusqlite::Error| DbError::Query(e.to_string());
        let tx = self.conn.transaction().map_err(query_err)?;
        let mut count = 0u64;
        {
            let mut stmt = tx.prepare(OBSERVE).map_err(query_err)?;
            for row in rows {
                let cmd = row.cmd.trim();
                if cmd.is_empty() {
                    continue;
                }
                stmt.execute(params![cmd, row.cwd, row.exit, row.ts, row.session])
                    .map_err(query_err)?;
                count += 1;
            }
        }
        tx.commit().map_err(query_err)?;
        Ok(count)
    }
}

/// The one write in padloper. Insert the command, or replace the whole
/// metadata set if this sighting is at least as new as the stored one.
///
/// The `where` clause is what makes import safe to run at any time and more
/// than once: an older sighting cannot overwrite what a live shell recorded.
/// Refreshing id makes same-second observations sort by arrival, since ts
/// alone cannot separate them.
const OBSERVE: &str = "
insert into history (cmd, cwd, exit, ts, session)
values (?1, ?2, ?3, ?4, ?5)
on conflict(cmd) do update set
  id = excluded.id, cwd = excluded.cwd, exit = excluded.exit,
  ts = excluded.ts, session = excluded.session
where history.ts is null or excluded.ts >= history.ts
";

// Most of these pin down OBSERVE. Its conflict rules decide what history
// survives an import, and none of them are visible from a row count.
#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use tempfile::TempDir;

    // The TempDir comes back with the Db because dropping it deletes the
    // file out from under the open connection.
    fn temp_db() -> (TempDir, Db) {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = Db::open_at(&dir.path().join("history.db")).expect("open");
        (dir, db)
    }

    #[test]
    fn data_directory_prefers_xdg_then_falls_back_to_home() {
        assert_eq!(
            data_dir_from(Some(OsString::from("/xdg")), Some(OsString::from("/home")))
                .expect("xdg"),
            PathBuf::from("/xdg")
        );
        assert_eq!(
            data_dir_from(Some(OsString::new()), Some(OsString::from("/home"))).expect("home"),
            PathBuf::from("/home/.local/share")
        );
        assert!(matches!(data_dir_from(None, None), Err(DbError::NoHome)));
        assert!(matches!(
            data_dir_from(None, Some(OsString::new())),
            Err(DbError::NoHome)
        ));
    }

    // These strings are the user-facing surface: app prints them verbatim
    // after "padloper: ". Each has to say what failed and where.
    #[test]
    fn database_errors_name_the_operation_and_path() {
        assert_eq!(
            DbError::NoHome.to_string(),
            "cannot locate the data directory: XDG_DATA_HOME and HOME are unset"
        );
        assert_eq!(
            DbError::Open {
                path: PathBuf::from("/tmp/history.db"),
                message: "bad file".into(),
            }
            .to_string(),
            "cannot open /tmp/history.db: bad file"
        );
        assert_eq!(
            DbError::Query("bad query".into()).to_string(),
            "database error: bad query"
        );
    }

    // A file where a directory should be, so create_dir_all fails. The
    // error has to name the db path, not the parent it tripped on.
    #[test]
    fn opening_below_a_file_reports_an_open_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let parent = dir.path().join("not-a-directory");
        std::fs::write(&parent, "file").expect("write parent");
        let path = parent.join("history.db");

        let error = Db::open_at(&path).err().expect("open must fail");

        assert!(matches!(error, DbError::Open { path: p, .. } if p == path));
    }

    // `create table if not exists` accepts any existing table by that name,
    // whatever its columns. A file holding a foreign or older `history` has
    // to fail at open, not on the first query.
    #[test]
    fn malformed_existing_schema_reports_an_open_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("history.db");
        let conn = Connection::open(&path).expect("open seed db");
        conn.execute_batch("create table history (cmd text);")
            .expect("seed schema");
        drop(conn);

        assert!(matches!(Db::open_at(&path), Err(DbError::Open { .. })));
    }

    // Every subcommand opens the db, so the schema has to apply to an
    // existing file as cleanly as to a new one.
    #[test]
    fn opening_twice_is_idempotent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("history.db");
        let db = Db::open_at(&path).expect("open");
        drop(db);
        Db::open_at(&path).expect("reopen");
    }

    #[test]
    fn inserting_the_same_command_updates_the_existing_row() {
        let (_dir, db) = temp_db();
        db.observe(observation("ls", Some("/a"), Some(0), 100, None))
            .expect("insert");
        db.observe(observation("ls", Some("/b"), Some(1), 200, Some("s")))
            .expect("insert");
        let rows = db.list().expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ts, 200);
        assert_eq!(rows[0].exit, Some(1));
    }

    #[test]
    fn recent_orders_by_timestamp_descending_and_honors_the_limit() {
        let (_dir, db) = temp_db();
        db.observe(observation("first", None, Some(0), 100, None))
            .expect("insert");
        db.observe(observation("third", Some("/t"), Some(0), 300, None))
            .expect("insert");
        db.observe(observation("second", None, Some(0), 200, None))
            .expect("insert");
        let rows = db.recent(2).expect("recent");
        let cmds: Vec<&str> = rows.iter().map(|r| r.cmd.as_str()).collect();
        assert_eq!(cmds, ["third", "second"]);
        assert_eq!(rows[0].cwd.as_deref(), Some("/t"));
        assert_eq!(rows[1].cwd, None);
    }

    // Import must not walk history backwards: the stored `ls` is newer than
    // the imported one, so it keeps its cwd, status, and time.
    #[test]
    fn batch_keeps_the_newer_observation_and_skips_empty_commands() {
        let (_dir, mut db) = temp_db();
        db.observe(observation("ls", Some("/new"), Some(0), 500, Some("new")))
            .expect("insert");
        let imported = db
            .observe_all(vec![
                observation("ls", Some("/old"), Some(1), 100, Some("old")),
                observation("  ", None, None, 100, None),
                observation("pwd", None, None, 200, None),
            ])
            .expect("import");
        assert_eq!(imported, 2);
        let rows = db.list().expect("list");
        assert_eq!(rows[0].cmd, "ls");
        assert_eq!(rows[0].ts, 500);
        assert_eq!(rows[0].exit, Some(0));
        assert_eq!(rows[1].cmd, "pwd");
    }

    // A partial update would leave a row describing two different runs, so
    // this reads the columns no public accessor exposes.
    #[test]
    fn a_newer_observation_replaces_the_complete_record() {
        let (_dir, db) = temp_db();
        db.observe(observation("ls", Some("/old"), Some(1), 100, Some("old")))
            .expect("insert");
        db.observe(observation("ls", Some("/new"), Some(0), 200, Some("new")))
            .expect("insert");

        let row: (String, Option<i64>, i64, Option<String>) = db
            .conn
            .query_row(
                "select cwd, exit, ts, session from history where cmd = 'ls'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("query");
        assert_eq!(row, ("/new".into(), Some(0), 200, Some("new".into())));
    }

    // Commands run inside one second share a timestamp. The `>=` in OBSERVE
    // and the refreshed id are what order them, and a plain `>` would leave
    // the repeat of `first` behind `second`.
    #[test]
    fn equal_timestamps_prefer_the_later_observation() {
        let (_dir, db) = temp_db();
        db.observe(observation("first", None, Some(0), 100, None))
            .expect("insert");
        db.observe(observation("second", None, Some(0), 100, None))
            .expect("insert");
        db.observe(observation("first", None, Some(2), 100, None))
            .expect("insert");

        let rows = db.list().expect("list");
        assert_eq!(rows[0].cmd, "first");
        assert_eq!(rows[0].exit, Some(2));
        assert_eq!(rows[1].cmd, "second");
    }

    // The trigger is the only way to fail a write part way through a batch
    // that padloper itself would accept. What matters is that "good", which
    // inserted cleanly, is gone too: a half-applied import is worse than
    // none, because it looks complete.
    #[test]
    fn batch_failure_rolls_back_earlier_rows() {
        let (_dir, mut db) = temp_db();
        db.conn
            .execute_batch(
                "create trigger reject_bad before insert on history
                 when new.cmd = 'bad'
                 begin select raise(abort, 'bad command'); end;",
            )
            .expect("trigger");

        let error = db
            .observe_all(vec![
                observation("good", None, None, 100, None),
                observation("bad", None, None, 200, None),
            ])
            .expect_err("batch must fail");

        assert!(matches!(error, DbError::Query(_)));
        assert!(db.list().expect("list").is_empty());
    }

    // Dropping the table is a stand-in for a db that went bad after opening.
    // Every read and write has to surface that as an error rather than an
    // empty result, which would read as "no history".
    #[test]
    fn query_methods_report_a_removed_schema() {
        let (_dir, mut db) = temp_db();
        db.conn
            .execute_batch("drop table history;")
            .expect("drop schema");

        assert!(matches!(
            db.observe(observation("ls", None, None, 100, None)),
            Err(DbError::Query(_))
        ));
        assert!(matches!(db.recent(10), Err(DbError::Query(_))));
        assert!(matches!(db.list(), Err(DbError::Query(_))));
        assert!(matches!(
            db.observe_all([observation("ls", None, None, 100, None)]),
            Err(DbError::Query(_))
        ));
    }

    // 51 rows for a 50 row cap, so the test also shows which end is
    // dropped: the newest 50 stay and the oldest falls off.
    #[test]
    fn list_returns_only_the_fifty_newest_rows() {
        let (_dir, db) = temp_db();
        for ts in 0..51 {
            db.observe(observation(&format!("cmd-{ts:02}"), None, None, ts, None))
                .expect("insert");
        }

        let rows = db.list().expect("list");

        assert_eq!(rows.len(), 50);
        assert_eq!(rows.first().map(|row| row.cmd.as_str()), Some("cmd-50"));
        assert_eq!(rows.last().map(|row| row.cmd.as_str()), Some("cmd-01"));
    }

    fn observation(
        cmd: &str,
        cwd: Option<&str>,
        exit: Option<i64>,
        ts: i64,
        session: Option<&str>,
    ) -> Observation {
        Observation {
            cmd: cmd.to_string(),
            cwd: cwd.map(str::to_string),
            exit,
            ts,
            session: session.map(str::to_string),
        }
    }
}
