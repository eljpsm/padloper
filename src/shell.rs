//! Shell integration snippets printed by `padloper init`.
//!
//! Each snippet does the same three things in its own dialect: set a session
//! id, record every command from a prompt hook, and bind ctrl+r and the up
//! arrow to the picker. There is no shared abstraction because the three
//! shells agree on none of the mechanics, and hiding the ordering rules
//! behind a common shape would not remove them.
//!
//! Every widget reads the same protocol from `padloper search`: stdout is
//! the command, and the exit status is the action. 0 runs it, 10 puts it on
//! the prompt, anything else leaves the prompt alone. See
//! `app::emit_search`.
//!
//! These strings only ever get eval'd, so a syntax error surfaces as a
//! broken interactive shell. The tests at the bottom are the only automated
//! check that each one still says what it must.

use clap::ValueEnum;

/// The shells `padloper init` knows how to write for.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

impl Shell {
    /// The snippet to eval from a shell config file.
    pub fn snippet(self) -> &'static str {
        match self {
            Shell::Bash => BASH_INIT,
            Shell::Zsh => ZSH_INIT,
            Shell::Fish => FISH_INIT,
        }
    }
}

// Recording keys off the history entry number, not the command text, so a
// repeated command still refreshes its timestamp while an empty prompt Enter
// records nothing.
// Prompt and editor contracts:
// https://www.gnu.org/software/bash/manual/html_node/Bash-Variables.html
// https://www.gnu.org/software/bash/manual/html_node/Bash-Builtins.html
const BASH_INIT: &str = r#"if [ -z "${PADLOPER_SESSION:-}" ]; then
    export PADLOPER_SESSION=$(printf '%04x%04x%04x%04x' $RANDOM $RANDOM $RANDOM $RANDOM)
fi

# Skip whatever the history file already held when the shell started.
read -r __padloper_last _ <<< "$(HISTTIMEFORMAT='' builtin history 1)"

__padloper_record() {
    local exit=$?
    local entry num cmd
    entry=$(HISTTIMEFORMAT='' builtin history 1) || return $exit
    read -r num _ <<< "$entry"
    if [ -z "$num" ] || [ "$num" = "$__padloper_last" ]; then
        return $exit
    fi
    __padloper_last=$num
    cmd=$(sed '1s/^ *[0-9]* *//' <<< "$entry")
    # Errors are swallowed and the status is passed through untouched. A
    # broken db must not print at every prompt or change what $? reports.
    padloper add --exit "$exit" -- "$cmd" 2>/dev/null
    return $exit
}
PROMPT_COMMAND="__padloper_record${PROMPT_COMMAND:+;$PROMPT_COMMAND}"

# bind -x widgets cannot call accept-line, so ctrl+r runs a two-key macro.
# The widget decides what the second key does by rebinding it first.
__padloper_noop() { :; }

__padloper_search() {
    local out action
    bind -x '"\C-x\C-p": __padloper_noop'
    out=$(padloper search -- "$READLINE_LINE")
    action=$?
    case $action in
        0)
            if [ -n "$out" ]; then
                READLINE_LINE=$out
                READLINE_POINT=${#READLINE_LINE}
                bind '"\C-x\C-p": accept-line'
            fi
            ;;
        10)
            if [ -n "$out" ]; then
                READLINE_LINE=$out
                READLINE_POINT=${#READLINE_LINE}
            fi
            ;;
    esac
}
bind -x '"\C-x\C-o": __padloper_search'
bind '"\C-r": "\C-x\C-o\C-x\C-p"'
# Up arrow too, in both cursor modes.
bind '"\e[A": "\C-x\C-o\C-x\C-p"'
bind '"\eOA": "\C-x\C-o\C-x\C-p"'
"#;

// preexec sees the command, precmd sees the exit status. Recording spans
// both so each run lands with the right status, and an empty prompt Enter
// never fires preexec.
// Hook and editor contracts:
// https://zsh.sourceforge.io/Doc/Release/Functions.html#Hook-Functions
// https://zsh.sourceforge.io/Doc/Release/User-Contributions.html#Manipulating-Hook-Functions
// https://zsh.sourceforge.io/Doc/Release/Zsh-Line-Editor.html
const ZSH_INIT: &str = r#"if [[ -z "${PADLOPER_SESSION:-}" ]]; then
    export PADLOPER_SESSION=$(printf '%04x%04x%04x%04x' $RANDOM $RANDOM $RANDOM $RANDOM)
fi

autoload -Uz add-zsh-hook

__padloper_preexec() {
    __padloper_cmd=$1
}
add-zsh-hook preexec __padloper_preexec

__padloper_record() {
    local exit=$?
    if [[ -n "$__padloper_cmd" ]]; then
        padloper add --exit "$exit" -- "$__padloper_cmd" 2>/dev/null
        __padloper_cmd=""
    fi
}
add-zsh-hook precmd __padloper_record

__padloper_search() {
    local out action
    out=$(padloper search -- "$BUFFER")
    action=$?
    case $action in
        0)
            if [[ -n "$out" ]]; then
                BUFFER=$out
                CURSOR=${#BUFFER}
                zle accept-line
                return
            fi
            ;;
        10)
            if [[ -n "$out" ]]; then
                BUFFER=$out
                CURSOR=${#BUFFER}
            fi
            ;;
    esac
    # The picker left the screen; redraw the prompt under it.
    zle reset-prompt
}
zle -N __padloper_search
bindkey '^r' __padloper_search
bindkey '^[[A' __padloper_search
bindkey '^[OA' __padloper_search
"#;

// Event, editor, and binding contracts:
// https://fishshell.com/docs/current/language.html#event-handlers
// https://fishshell.com/docs/current/cmds/commandline.html
// https://fishshell.com/docs/current/cmds/bind.html
const FISH_INIT: &str = r#"if not set -q PADLOPER_SESSION
    set -gx PADLOPER_SESSION (printf '%04x%04x%04x%04x' (random) (random) (random) (random))
end

function __padloper_record --on-event fish_postexec
    set -l exit_code $status
    if test -n "$argv[1]"
        padloper add --exit $exit_code -- $argv[1] 2>/dev/null
    end
end

function __padloper_search
    # Fish splits command output on newlines, so a multiline command comes
    # back as several elements and has to be joined again.
    set -l lines (padloper search -- (commandline))
    set -l action $status
    set -l out (string join \n -- $lines)
    switch $action
        case 0
            if test -n "$out"
                commandline -r -- $out
                commandline -f execute
                return
            end
        case 10
            if test -n "$out"
                commandline -r -- $out
            end
    end
    commandline -f repaint
end
bind \cr __padloper_search
bind up __padloper_search
"#;

// Nothing here can run a shell, so these assert on the text. They catch a
// snippet that lost a binding or drifted from the others, not one that no
// longer parses.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_snippet_records_and_binds_the_searcher() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let snippet = shell.snippet();
            assert!(
                snippet.contains("padloper add"),
                "{shell:?} lacks the record call"
            );
            assert!(
                snippet.contains("padloper search"),
                "{shell:?} lacks the search call"
            );
            assert!(
                snippet.contains("PADLOPER_SESSION"),
                "{shell:?} lacks the session id"
            );
        }
    }

    #[test]
    fn every_snippet_binds_ctrl_r() {
        assert!(BASH_INIT.contains(r#"bind '"\C-r": "\C-x\C-o\C-x\C-p"'"#));
        assert!(ZSH_INIT.contains("bindkey '^r' __padloper_search"));
        assert!(FISH_INIT.contains(r"bind \cr __padloper_search"));
    }

    // Terminals send the up arrow as either escape sequence depending on
    // cursor key mode, so bash and zsh have to bind both.
    #[test]
    fn every_snippet_binds_the_up_arrow() {
        assert!(BASH_INIT.contains(r#"bind '"\e[A": "\C-x\C-o\C-x\C-p"'"#));
        assert!(BASH_INIT.contains(r#"bind '"\eOA": "\C-x\C-o\C-x\C-p"'"#));
        assert!(ZSH_INIT.contains("bindkey '^[[A' __padloper_search"));
        assert!(ZSH_INIT.contains("bindkey '^[OA' __padloper_search"));
        assert!(FISH_INIT.contains("bind up __padloper_search"));
    }

    // An earlier revision prefixed the command with an action marker. The
    // status carries it now, so no snippet may look for the old one.
    #[test]
    fn every_snippet_handles_search_statuses_without_a_marker() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let snippet = shell.snippet();
            assert!(snippet.contains("action"), "{shell:?} lacks the status");
            assert!(snippet.contains("10"), "{shell:?} lacks insert");
            assert!(!snippet.contains("__padloper_run__:"));
        }
    }

    // A snippet is eval'd under whatever locale the user has. Non-ascii in
    // one could be mangled before the shell parses it.
    #[test]
    fn every_snippet_is_plain_ascii() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            assert!(shell.snippet().is_ascii(), "{shell:?} contains non-ascii");
        }
    }
}
