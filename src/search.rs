//! Picker state: query editing, fuzzy filtering, selection, scrolling.
//! No terminal code, so all of it is unit-testable.
//!
//! One index convention runs through the whole module. Position 0 is the
//! best match and the newest, and it draws nearest the prompt at the bottom
//! of the screen. Moving up the screen therefore moves toward older commands
//! and a higher index.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use unicode_segmentation::UnicodeSegmentation;

use crate::db::HistoryRow;

pub struct SearchState {
    /// Every candidate, newest first, as [`crate::db::Db::recent`] returned
    /// them. Never reordered; ranking moves indices in `matches` instead.
    rows: Vec<HistoryRow>,
    query: String,
    /// Indices into `rows`, best match first.
    matches: Vec<usize>,
    /// Index into `matches`, not into `rows`.
    selected: usize,
    /// First visible position in `matches`.
    offset: usize,
    matcher: Matcher,
    /// Scratch the matcher borrows per candidate. Held here so scoring a
    /// full history does not allocate once per row.
    buf: Vec<char>,
    /// Where the picker was opened. None when the directory is unknown,
    /// which also disables the dir-only toggle.
    cwd: Option<String>,
    /// When set, only rows whose last run happened in `cwd` match.
    dir_only: bool,
}

impl SearchState {
    /// `rows` must be newest first: the ranking tiebreak reads that order.
    pub fn new(rows: Vec<HistoryRow>, initial_query: &str, cwd: Option<String>) -> Self {
        let mut state = SearchState {
            rows,
            query: initial_query.to_string(),
            matches: Vec::new(),
            selected: 0,
            offset: 0,
            matcher: Matcher::new(Config::DEFAULT),
            buf: Vec::new(),
            cwd,
            dir_only: false,
        };
        state.refilter();
        state
    }

    /// Narrow matches to the current directory, or widen back out. Does
    /// nothing when the directory is unknown, so dir-only always has a
    /// directory to compare against.
    pub fn toggle_dir_only(&mut self) {
        if self.cwd.is_none() {
            return;
        }
        self.dir_only = !self.dir_only;
        self.refilter();
    }

    pub fn dir_only(&self) -> bool {
        self.dir_only
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// The highlighted command, or None when nothing matches.
    pub fn selection(&self) -> Option<&str> {
        self.matches
            .get(self.selected)
            .map(|&i| self.rows[i].cmd.as_str())
    }

    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.refilter();
    }

    /// Delete one visible character, which can be several chars: an accented
    /// letter or an emoji with modifiers goes in a single press.
    pub fn backspace(&mut self) {
        self.pop_grapheme();
        self.refilter();
    }

    pub fn clear_line(&mut self) {
        self.query.clear();
        self.refilter();
    }

    /// Delete trailing whitespace, then the word before it. Whitespace is
    /// judged per grapheme, so an ideographic space separates words the same
    /// way an ascii one does.
    pub fn delete_word(&mut self) {
        while self
            .query
            .graphemes(true)
            .next_back()
            .is_some_and(|g| g.chars().all(char::is_whitespace))
        {
            self.pop_grapheme();
        }
        while self
            .query
            .graphemes(true)
            .next_back()
            .is_some_and(|g| !g.chars().all(char::is_whitespace))
        {
            self.pop_grapheme();
        }
        self.refilter();
    }

    fn pop_grapheme(&mut self) {
        if let Some((start, _)) = self.query.grapheme_indices(true).next_back() {
            self.query.truncate(start);
        }
    }

    /// Up on screen means older: matches render bottom-up with index 0,
    /// the best and newest, next to the input line.
    ///
    /// `visible` is the row count on screen now, which a resize can change
    /// between calls.
    pub fn move_up(&mut self, visible: usize) {
        if self.selected + 1 < self.matches.len() {
            self.selected += 1;
        }
        self.ensure_visible(visible);
    }

    pub fn move_down(&mut self, visible: usize) {
        self.selected = self.selected.saturating_sub(1);
        self.ensure_visible(visible);
    }

    /// Keep the selection inside the window. Call before drawing so a
    /// resize cannot leave it off screen.
    pub fn ensure_visible(&mut self, visible: usize) {
        if visible == 0 || self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + visible {
            self.offset = self.selected + 1 - visible;
        }
    }

    /// Rows in the window, best match first, with a selected flag. The
    /// caller draws them bottom-up.
    pub fn visible_rows(&self, visible: usize) -> impl Iterator<Item = (bool, &HistoryRow)> {
        self.matches
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(visible)
            .map(|(i, &row)| (i == self.selected, &self.rows[row]))
    }

    /// Rescore every row against the query. Runs on each keystroke, and
    /// returns the selection to the top so the best match is always the one
    /// Enter takes.
    fn refilter(&mut self) {
        self.selected = 0;
        self.offset = 0;
        // The scope narrows the candidates before the query ranks them. A
        // row with no recorded directory never matches in dir-only mode.
        let dir_only = self.dir_only;
        let cwd = self.cwd.as_deref();
        let in_scope =
            |row: &HistoryRow| !dir_only || (row.cwd.is_some() && row.cwd.as_deref() == cwd);
        // Scoring an empty pattern would rank by nothing, so short-circuit
        // to plain recency order.
        if self.query.is_empty() {
            self.matches = self
                .rows
                .iter()
                .enumerate()
                .filter(|(_, row)| in_scope(row))
                .map(|(i, _)| i)
                .collect();
            return;
        }
        // Smart casing: an all-lowercase query ignores case, and one capital
        // makes the whole query case sensitive.
        let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let matcher = &mut self.matcher;
        let buf = &mut self.buf;
        let mut scored: Vec<(u32, usize)> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| in_scope(row))
            .filter_map(|(i, row)| {
                pattern
                    .score(Utf32Str::new(&row.cmd, buf), matcher)
                    .map(|s| (s, i))
            })
            .collect();
        // Rows arrive newest first, so the index tiebreak prefers newer
        // commands on equal scores.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.matches = scored.into_iter().map(|(_, i)| i).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(cmds: &[&str]) -> Vec<HistoryRow> {
        rows_in(&cmds.iter().map(|&cmd| (cmd, None)).collect::<Vec<_>>())
    }

    // Newest first, like Db::recent.
    fn rows_in(cmds: &[(&str, Option<&str>)]) -> Vec<HistoryRow> {
        cmds.iter()
            .enumerate()
            .map(|(i, (cmd, cwd))| HistoryRow {
                cmd: cmd.to_string(),
                ts: 1000 - i as i64,
                cwd: cwd.map(str::to_string),
            })
            .collect()
    }

    // A window big enough to hold everything, for tests about ranking
    // rather than scrolling.
    fn matched(state: &SearchState) -> Vec<&str> {
        state
            .visible_rows(usize::MAX)
            .map(|(_, r)| r.cmd.as_str())
            .collect()
    }

    #[test]
    fn an_empty_query_keeps_recency_order() {
        let state = SearchState::new(rows(&["newest", "older", "oldest"]), "", None);
        assert_eq!(matched(&state), ["newest", "older", "oldest"]);
        assert_eq!(state.selection(), Some("newest"));
    }

    #[test]
    fn typing_filters_to_fuzzy_matches() {
        let mut state =
            SearchState::new(rows(&["git status", "cargo build", "git push"]), "", None);
        for c in "git".chars() {
            state.push_char(c);
        }
        assert_eq!(state.match_count(), 2);
        assert!(matched(&state).iter().all(|cmd| cmd.starts_with("git")));
    }

    // Both contain "make", and nucleo scores the shorter one at least as
    // well, so this fixes the tiebreak direction rather than the score.
    #[test]
    fn equal_scores_prefer_the_newer_command() {
        let mut state = SearchState::new(rows(&["make test", "make test x"]), "", None);
        for c in "make".chars() {
            state.push_char(c);
        }
        assert_eq!(matched(&state)[0], "make test");
    }

    #[test]
    fn no_match_yields_no_selection() {
        let state = SearchState::new(rows(&["ls"]), "zzzz", None);
        assert_eq!(state.match_count(), 0);
        assert_eq!(state.selection(), None);
    }

    // Holding an arrow key past either end must not wrap or panic.
    #[test]
    fn selection_clamps_at_both_ends() {
        let mut state = SearchState::new(rows(&["a", "b"]), "", None);
        state.move_down(10);
        assert_eq!(state.selection(), Some("a"));
        state.move_up(10);
        state.move_up(10);
        state.move_up(10);
        assert_eq!(state.selection(), Some("b"));
    }

    // Four matches in a two-row window: the offset has to move at the far
    // edge and come back on the way down.
    #[test]
    fn the_window_follows_the_selection() {
        let mut state = SearchState::new(rows(&["a", "b", "c", "d"]), "", None);
        state.move_up(2);
        state.move_up(2);
        let visible: Vec<&str> = state.visible_rows(2).map(|(_, r)| r.cmd.as_str()).collect();
        assert_eq!(visible, ["b", "c"]);
        state.move_down(2);
        state.move_down(2);
        let visible: Vec<&str> = state.visible_rows(2).map(|(_, r)| r.cmd.as_str()).collect();
        assert_eq!(visible, ["a", "b"]);
    }

    // Otherwise a stale index would leave Enter pointing at whatever row
    // landed in that slot after the new query reordered things.
    #[test]
    fn refilter_resets_the_selection_to_the_top() {
        let mut state = SearchState::new(rows(&["aa", "ab", "ac"]), "", None);
        state.move_down(10);
        state.push_char('a');
        assert_eq!(state.selection(), Some("aa"));
    }

    #[test]
    fn ctrl_w_deletes_the_last_word_and_ctrl_u_clears() {
        let mut state = SearchState::new(rows(&["git status"]), "", None);
        for c in "git sta".chars() {
            state.push_char(c);
        }
        state.delete_word();
        assert_eq!(state.query(), "git ");
        state.delete_word();
        assert_eq!(state.query(), "");
        for c in "xy".chars() {
            state.push_char(c);
        }
        state.clear_line();
        assert_eq!(state.query(), "");
        assert_eq!(state.match_count(), 1);
    }

    // "e" plus a combining acute is one grapheme and two chars. Popping a
    // char would leave a bare "e" on screen and take two presses.
    #[test]
    fn backspace_removes_one_visible_character() {
        let mut state = SearchState::new(rows(&["else"]), "e\u{301}", None);
        state.backspace();
        assert_eq!(state.query(), "");
    }

    // The row with no cwd drops out along with the one from another
    // directory. Unknown is not treated as a match, so dir-only never shows
    // a command it cannot place. Toggling back restores all three.
    #[test]
    fn dir_only_narrows_to_the_current_directory_and_back() {
        let mut state = SearchState::new(
            rows_in(&[
                ("make test", Some("/here")),
                ("cargo build", Some("/elsewhere")),
                ("ls", None),
            ]),
            "",
            Some("/here".to_string()),
        );
        assert_eq!(state.match_count(), 3);
        state.toggle_dir_only();
        assert!(state.dir_only());
        assert_eq!(matched(&state), ["make test"]);
        state.toggle_dir_only();
        assert!(!state.dir_only());
        assert_eq!(state.match_count(), 3);
    }

    // Scope and query are separate filters, both applied. Narrowing does
    // not clear the query the user already typed.
    #[test]
    fn dir_only_composes_with_a_query() {
        let mut state = SearchState::new(
            rows_in(&[
                ("git status", Some("/elsewhere")),
                ("git log", Some("/here")),
            ]),
            "git",
            Some("/here".to_string()),
        );
        assert_eq!(state.match_count(), 2);
        state.toggle_dir_only();
        assert_eq!(matched(&state), ["git log"]);
    }

    // Narrowing can shrink the list under the selection, so it returns to
    // the top like any other refilter. A kept index could point past the
    // end or at a row the user never chose.
    #[test]
    fn dir_only_resets_the_selection() {
        let mut state = SearchState::new(
            rows_in(&[("a", Some("/here")), ("b", Some("/here"))]),
            "",
            Some("/here".to_string()),
        );
        state.move_up(10);
        state.toggle_dir_only();
        assert_eq!(state.selection(), Some("a"));
    }

    // With no directory to compare against the toggle must stay off, or
    // dir-only mode would silently match nothing.
    #[test]
    fn the_toggle_does_nothing_without_a_directory() {
        let mut state = SearchState::new(rows_in(&[("ls", Some("/here"))]), "", None);
        state.toggle_dir_only();
        assert!(!state.dir_only());
        assert_eq!(state.match_count(), 1);
    }

    // U+2003 is an em space. char::is_whitespace covers it, so ctrl+w stops
    // there instead of eating the line back to the start.
    #[test]
    fn ctrl_w_treats_unicode_whitespace_as_a_separator() {
        let mut state = SearchState::new(rows(&["git status"]), "git\u{2003}sta", None);
        state.delete_word();
        assert_eq!(state.query(), "git\u{2003}");
        state.delete_word();
        assert_eq!(state.query(), "");
    }
}
