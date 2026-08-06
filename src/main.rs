//! padloper records shell history into sqlite and searches it.
//!
//! The pipeline: cli parses the subcommand, app dispatches it, db holds the
//! history. search owns the picker logic, tui draws it, import fills the db
//! from atuin or a history file.

mod app;
mod cli;
mod db;
mod import;
mod search;
mod shell;
mod timefmt;
mod tui;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    app::run(cli::Cli::parse())
}
