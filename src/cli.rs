//! The command line surface. Doc comments here are user-facing: clap prints
//! them as help text.

use clap::{Parser, Subcommand};

use crate::shell::Shell;

/// A minimalist sqlite-backed shell-history recorder and searcher.
#[derive(Debug, Parser)]
#[command(name = "padloper", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// The subcommands. `add` and `search` are called by the shell snippets in
/// [`crate::shell`]; the rest are typed by hand.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the shell integration snippet for bash, zsh, or fish.
    Init {
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Record one executed command. Called by the shell hook.
    Add {
        /// Exit status of the command.
        #[arg(long)]
        exit: i32,
        /// The command line, after --.
        // `last` puts everything past `--` here, so a recorded command that
        // looks like a flag is not parsed as one.
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Search history interactively. Prints the selection to stdout.
    Search {
        /// Initial query.
        // The widgets pass the current prompt line, which can be empty, can
        // start with a hyphen, and can hold anything else the user typed.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        query: Vec<String>,
    },
    /// Print the last 50 commands as time, exit, and cmd, tab separated.
    List,
    /// Import from Atuin, $HISTFILE, or the current shell's history.
    Import,
}

// These cover the argument shapes the shell snippets produce. A parsing
// change that breaks one of them breaks recording or searching in a live
// shell, where the failure is silent.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_requires_an_exit_status_and_a_command() {
        assert!(Cli::try_parse_from(["padloper", "add", "--", "ls"]).is_err());
        assert!(Cli::try_parse_from(["padloper", "add", "--exit", "0", "--"]).is_err());
        assert!(Cli::try_parse_from(["padloper", "add", "--exit", "0", "--", "ls"]).is_ok());
    }

    #[test]
    fn add_keeps_the_command_after_the_separator_verbatim() {
        let cli = Cli::try_parse_from(["padloper", "add", "--exit", "1", "--", "grep", "-r", "x"])
            .expect("parse");
        match cli.command {
            Command::Add { exit, cmd } => {
                assert_eq!(exit, 1);
                assert_eq!(cmd, ["grep", "-r", "x"]);
            }
            other => panic!("expected add, got {other:?}"),
        }
    }

    // A prompt line left mid-edit can begin with a hyphen. Without
    // allow_hyphen_values clap would read it as an unknown flag and exit 2.
    #[test]
    fn search_accepts_query_words_starting_with_hyphens() {
        let cli = Cli::try_parse_from(["padloper", "search", "-n", "foo"]).expect("parse");
        match cli.command {
            Command::Search { query } => assert_eq!(query, ["-n", "foo"]),
            other => panic!("expected search, got {other:?}"),
        }
    }

    // The widgets all call `padloper search -- "$BUFFER"`, so the separator
    // must not survive into the query.
    #[test]
    fn search_drops_the_separator_the_shell_widgets_pass() {
        let cli = Cli::try_parse_from(["padloper", "search", "--", "git"]).expect("parse");
        match cli.command {
            Command::Search { query } => assert_eq!(query, ["git"]),
            other => panic!("expected search, got {other:?}"),
        }
    }

    #[test]
    fn init_rejects_unknown_shells() {
        assert!(Cli::try_parse_from(["padloper", "init", "tcsh"]).is_err());
        assert!(Cli::try_parse_from(["padloper", "init", "zsh"]).is_ok());
    }
}
