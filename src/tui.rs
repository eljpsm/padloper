//! The terminal loop around SearchState. Draws to stderr so stdout carries
//! only the accepted command.
//!
//! This module puts the terminal into a state the user cannot recover from
//! by hand, so every path back out is a guard. Raw mode, the alternate
//! screen, and the panic hook each restore themselves on drop, and a panic
//! inside the loop restores the terminal before its message prints.

use std::io::{self, BufWriter, IsTerminal, Write};
use std::sync::Arc;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode, size,
};
use crossterm::{execute, queue};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::db::HistoryRow;
use crate::search::SearchState;
use crate::timefmt::{relative_time, unix_now};

/// What the user asked for. `app::emit_search` turns this into the exit
/// status the shell widget reads.
pub enum Outcome {
    /// Put the command on the prompt and execute it.
    Run(String),
    /// Put the command on the prompt and leave the cursor there.
    Insert(String),
    /// Leave the prompt as it was.
    Cancelled,
}

/// What one event does to the loop. Key handling returns this instead of
/// acting on the terminal, which is what lets the bindings be tested with
/// synthesized events and no pty.
enum EventAction {
    Continue,
    Resize(u16, u16),
    Finish(Outcome),
}

struct RawModeGuard;

impl RawModeGuard {
    fn new() -> io::Result<RawModeGuard> {
        enable_raw_mode()?;
        Ok(RawModeGuard)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        leave_raw_mode();
    }
}

struct AlternateScreenGuard;

impl AlternateScreenGuard {
    fn new() -> io::Result<AlternateScreenGuard> {
        execute!(io::stderr(), EnterAlternateScreen)?;
        Ok(AlternateScreenGuard)
    }
}

impl Drop for AlternateScreenGuard {
    fn drop(&mut self) {
        leave_alternate_screen();
    }
}

type PanicHook = dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static;

/// Restores the terminal before the panic message reaches the screen.
/// Without it the message prints into the alternate screen, which the
/// unwind then tears down, and the user sees a bare prompt and no reason.
struct PanicHookGuard {
    previous: Option<Arc<PanicHook>>,
}

impl PanicHookGuard {
    fn new() -> PanicHookGuard {
        let previous: Arc<PanicHook> = std::panic::take_hook().into();
        let chained = Arc::clone(&previous);
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            chained(info);
        }));
        PanicHookGuard {
            previous: Some(previous),
        }
    }
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        let _ = std::panic::take_hook();
        let previous = self.previous.take().expect("previous panic hook");
        std::panic::set_hook(Box::new(move |info| previous(info)));
    }
}

fn leave_alternate_screen() {
    let _ = execute!(io::stderr(), LeaveAlternateScreen);
}

fn leave_raw_mode() {
    let _ = disable_raw_mode();
}

fn restore_terminal() {
    leave_alternate_screen();
    leave_raw_mode();
}

/// Draw the picker until the user picks or quits.
///
/// Fails rather than drawing when stderr is not a terminal, which is what a
/// pipe or a test harness gets.
pub fn run(rows: Vec<HistoryRow>, initial_query: &str, cwd: Option<String>) -> io::Result<Outcome> {
    if !io::stderr().is_terminal() {
        return Err(io::Error::other("search needs a terminal on stderr"));
    }
    let mut state = SearchState::new(rows, initial_query, cwd);
    // Declaration order sets teardown order: the alternate screen goes away
    // first, then raw mode, leaving the original screen and a live prompt.
    let _raw_mode = RawModeGuard::new()?;
    let _alternate_screen = AlternateScreenGuard::new()?;
    // The hook restores the terminal before the panic message is printed.
    let panic_hook = PanicHookGuard::new();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| event_loop(&mut state)));
    drop(panic_hook);
    match outcome {
        Ok(outcome) => outcome,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// Draw, wait for a key, repeat. Blocks on input, so an idle picker costs
/// nothing.
fn event_loop(state: &mut SearchState) -> io::Result<Outcome> {
    // One clock read for the whole session. Ages would otherwise shift under
    // the user mid-search, and the picker is open for seconds.
    let now = unix_now();
    let (mut width, mut height) = fallback(size()?);
    loop {
        let visible = window(height);
        state.ensure_visible(visible);
        draw(state, width, height, now)?;
        match handle_event(state, read()?, visible) {
            EventAction::Continue => {}
            EventAction::Resize(w, h) => {
                (width, height) = fallback((w, h));
            }
            EventAction::Finish(outcome) => return Ok(outcome),
        }
    }
}

/// Apply one event to the picker. The whole key map lives here, and it is
/// the only place a binding is defined.
///
/// Enter and Right on an empty match list cancel rather than doing nothing,
/// so a query that matches nothing still leaves on one keypress.
fn handle_event(state: &mut SearchState, event: Event, visible: usize) -> EventAction {
    match event {
        // Kitty protocol terminals also deliver Release and Repeat.
        Event::Key(k) if k.kind == KeyEventKind::Press => {
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
            match k.code {
                KeyCode::Esc => EventAction::Finish(Outcome::Cancelled),
                KeyCode::Char('c') if ctrl => EventAction::Finish(Outcome::Cancelled),
                KeyCode::Enter => EventAction::Finish(match state.selection() {
                    Some(cmd) => Outcome::Run(cmd.to_string()),
                    None => Outcome::Cancelled,
                }),
                KeyCode::Right | KeyCode::Tab => EventAction::Finish(match state.selection() {
                    Some(cmd) => Outcome::Insert(cmd.to_string()),
                    None => Outcome::Cancelled,
                }),
                KeyCode::Char('r') if ctrl => {
                    state.toggle_dir_only();
                    EventAction::Continue
                }
                KeyCode::Up => {
                    state.move_up(visible);
                    EventAction::Continue
                }
                KeyCode::Down => {
                    state.move_down(visible);
                    EventAction::Continue
                }
                KeyCode::Char('p') if ctrl => {
                    state.move_up(visible);
                    EventAction::Continue
                }
                KeyCode::Char('n') if ctrl => {
                    state.move_down(visible);
                    EventAction::Continue
                }
                KeyCode::Backspace => {
                    state.backspace();
                    EventAction::Continue
                }
                KeyCode::Char('u') if ctrl => {
                    state.clear_line();
                    EventAction::Continue
                }
                KeyCode::Char('w') if ctrl => {
                    state.delete_word();
                    EventAction::Continue
                }
                // Anything with ctrl or alt held is a binding this picker
                // does not have, not text to search for.
                KeyCode::Char(c) if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) => {
                    state.push_char(c);
                    EventAction::Continue
                }
                _ => EventAction::Continue,
            }
        }
        Event::Resize(w, h) => EventAction::Resize(w, h),
        _ => EventAction::Continue,
    }
}

/// Rows left for matches once the footer and the input line take theirs.
fn window(height: u16) -> usize {
    height.saturating_sub(2) as usize
}

// Some ptys report no size at all. Assume a classic terminal instead of
// drawing nothing.
fn fallback((width, height): (u16, u16)) -> (u16, u16) {
    (
        if width == 0 { 80 } else { width },
        if height == 0 { 24 } else { height },
    )
}

/// The input sits on the bottom row, the footer above it, and matches grow
/// upward from the footer, newest next to the input.
///
/// Everything goes through one BufWriter and a single flush, so the screen
/// changes in one write and never tears.
fn draw(state: &SearchState, width: u16, height: u16, now: i64) -> io::Result<()> {
    let mut w = BufWriter::new(io::stderr().lock());
    draw_to(&mut w, state, width, height, now)?;
    w.flush()
}

/// The drawing itself, against any sink. [`draw`] supplies the buffered
/// stderr; tests pass a Vec and read back what would have been printed.
fn draw_to(
    w: &mut impl Write,
    state: &SearchState,
    width: u16,
    height: u16,
    now: i64,
) -> io::Result<()> {
    let visible = window(height);
    let cols = width as usize;
    queue!(w, Hide, MoveTo(0, 0), Clear(ClearType::All))?;
    for (i, (selected, row)) in state.visible_rows(visible).enumerate() {
        let line = height - 3 - i as u16;
        queue!(w, MoveTo(0, line))?;
        let time = relative_time(now, row.ts);
        let time_cols = time.width();
        // Leave the marker, a gap, and the timestamp out of the command
        // budget so nothing collides.
        let budget = cols.saturating_sub(2 + time_cols + 1);
        let cmd = flatten(&row.cmd);
        let cmd = truncate(&cmd, budget);
        if selected {
            queue!(
                w,
                SetAttribute(Attribute::Bold),
                SetForegroundColor(Color::Cyan),
                Print("> "),
                Print(&cmd),
                ResetColor,
                SetAttribute(Attribute::Reset)
            )?;
        } else {
            queue!(w, Print("  "), Print(&cmd))?;
        }
        if time_cols < cols {
            queue!(
                w,
                MoveTo((cols - time_cols) as u16, line),
                SetAttribute(Attribute::Dim),
                Print(&time),
                SetAttribute(Attribute::Reset)
            )?;
        }
    }
    if height >= 2 {
        let footer = format!(
            "{} matches  up/down move  enter run  right insert  ctrl-r dir  esc cancel",
            state.match_count()
        );
        let footer = truncate(&footer, cols);
        queue!(
            w,
            MoveTo(0, height - 2),
            SetAttribute(Attribute::Dim),
            Print(footer),
            SetAttribute(Attribute::Reset)
        )?;
    }
    let input = height.saturating_sub(1);
    // The prompt is the scope indicator: `dir> ` while narrowed, `> ` while
    // searching everything. Held as a string even though the narrowed case
    // prints in pieces, so the cursor math below measures what was drawn.
    let prompt = if state.dir_only() { "dir> " } else { "> " };
    queue!(w, MoveTo(0, input))?;
    if state.dir_only() {
        queue!(
            w,
            SetAttribute(Attribute::Bold),
            SetForegroundColor(Color::Cyan),
            Print("dir"),
            ResetColor,
            SetAttribute(Attribute::Reset),
            Print("> ")
        )?;
    } else {
        queue!(w, Print(prompt))?;
    }
    queue!(w, Print(state.query()))?;
    // Past the last column the cursor would wrap to the next line, so a long
    // query parks it on the right edge instead.
    let cursor = (prompt.width() + state.query().width()).min(cols.saturating_sub(1)) as u16;
    queue!(w, MoveTo(cursor, input), Show)?;
    Ok(())
}

// Multiline commands render on one row; tabs would move the cursor on
// their own, so they become spaces too.
fn flatten(cmd: &str) -> String {
    cmd.replace('\n', "  ").replace(['\r', '\t'], " ")
}

/// The longest prefix that fits in `max_width` columns. Counts display
/// width, not bytes or chars, and never splits a grapheme: a cut inside one
/// would print a stray combining mark or half an emoji.
fn truncate(text: &str, max_width: usize) -> &str {
    let mut end = 0;
    let mut width = 0;
    for grapheme in text.graphemes(true) {
        let next = width + grapheme.width();
        if next > max_width {
            break;
        }
        width = next;
        end += grapheme.len();
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventState};

    // One row in the test cwd and one outside it, so dir-only filtering
    // shows up as a change in what matches. The second is multiline, which
    // drawing has to flatten.
    fn rows() -> Vec<HistoryRow> {
        vec![
            HistoryRow {
                cmd: "git status".into(),
                ts: 100,
                cwd: Some("/work".into()),
            },
            HistoryRow {
                cmd: "printf foo\nbar".into(),
                ts: 50,
                cwd: Some("/elsewhere".into()),
            },
        ]
    }

    fn test_state(query: &str) -> SearchState {
        SearchState::new(rows(), query, Some("/work".into()))
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    // Unwraps a finished action. Panics on Continue, which is the failure
    // this is checking for: a key that was meant to end the picker.
    fn outcome(action: EventAction) -> Outcome {
        match action {
            EventAction::Finish(outcome) => outcome,
            _ => panic!("expected a finished outcome"),
        }
    }

    // One case per way a byte count would go wrong: a wide character, a
    // multi-char grapheme, and an emoji joined by zero-width joiners.
    #[test]
    fn truncation_uses_terminal_columns_and_keeps_graphemes_whole() {
        assert_eq!(truncate("a界b", 3), "a界");
        assert_eq!(truncate("e\u{301}x", 1), "e\u{301}");
        assert_eq!(truncate("x👨‍👩‍👧‍👦y", 3), "x👨‍👩‍👧‍👦");
    }

    // A terminal too short for any match row must report 0 rather than
    // underflow, since draw indexes backwards from the bottom.
    #[test]
    fn terminal_dimensions_have_safe_minimums() {
        assert_eq!(window(0), 0);
        assert_eq!(window(2), 0);
        assert_eq!(window(5), 3);
        assert_eq!(fallback((0, 0)), (80, 24));
        assert_eq!(fallback((120, 40)), (120, 40));
        assert_eq!(fallback((0, 40)), (80, 40));
        assert_eq!(fallback((120, 0)), (120, 24));
    }

    // Right and Tab both insert. Enter runs. On a query matching nothing
    // all of them cancel, so the picker never traps the user.
    #[test]
    fn selection_keys_finish_with_the_expected_action() {
        let mut state = test_state("");
        assert!(matches!(
            outcome(handle_event(&mut state, key(KeyCode::Enter), 3)),
            Outcome::Run(cmd) if cmd == "git status"
        ));
        assert!(matches!(
            outcome(handle_event(&mut state, key(KeyCode::Right), 3)),
            Outcome::Insert(cmd) if cmd == "git status"
        ));
        assert!(matches!(
            outcome(handle_event(&mut state, key(KeyCode::Tab), 3)),
            Outcome::Insert(cmd) if cmd == "git status"
        ));

        let mut empty = test_state("no match");
        assert!(matches!(
            outcome(handle_event(&mut empty, key(KeyCode::Enter), 3)),
            Outcome::Cancelled
        ));
        assert!(matches!(
            outcome(handle_event(&mut empty, key(KeyCode::Right), 3)),
            Outcome::Cancelled
        ));
    }

    #[test]
    fn cancel_keys_finish_without_a_selection() {
        for event in [key(KeyCode::Esc), ctrl('c')] {
            let mut state = test_state("");
            assert!(matches!(
                outcome(handle_event(&mut state, event, 3)),
                Outcome::Cancelled
            ));
        }
    }

    // Walks the whole editing key map. The arrows and their ctrl aliases
    // must agree, and each editing key must reach the matching SearchState
    // call, since nothing else connects the two.
    #[test]
    fn editing_and_movement_keys_update_search_state() {
        let mut state = test_state("");
        handle_event(&mut state, key(KeyCode::Up), 2);
        assert_eq!(state.selection(), Some("printf foo\nbar"));
        handle_event(&mut state, key(KeyCode::Down), 2);
        assert_eq!(state.selection(), Some("git status"));
        handle_event(&mut state, ctrl('p'), 2);
        assert_eq!(state.selection(), Some("printf foo\nbar"));
        handle_event(&mut state, ctrl('n'), 2);
        assert_eq!(state.selection(), Some("git status"));

        handle_event(&mut state, key(KeyCode::Char('g')), 2);
        assert_eq!(state.query(), "g");
        handle_event(&mut state, key(KeyCode::Backspace), 2);
        assert_eq!(state.query(), "");
        for c in "git sta".chars() {
            handle_event(&mut state, key(KeyCode::Char(c)), 2);
        }
        handle_event(&mut state, ctrl('w'), 2);
        assert_eq!(state.query(), "git ");
        handle_event(&mut state, ctrl('u'), 2);
        assert_eq!(state.query(), "");

        assert!(!state.dir_only());
        handle_event(&mut state, ctrl('r'), 2);
        assert!(state.dir_only());
    }

    // Everything the picker must ignore. The release event is the important
    // one: terminals speaking the kitty protocol send a Release for every
    // Press, and handling both would type each character twice.
    #[test]
    fn resize_and_unbound_events_do_not_edit_the_query() {
        let mut state = test_state("");
        assert!(matches!(
            handle_event(&mut state, Event::Resize(90, 30), 2),
            EventAction::Resize(90, 30)
        ));
        handle_event(
            &mut state,
            Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)),
            2,
        );
        handle_event(&mut state, ctrl('x'), 2);
        handle_event(&mut state, key(KeyCode::Left), 2);
        handle_event(&mut state, Event::FocusGained, 2);

        let release = KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        handle_event(&mut state, Event::Key(release), 2);
        assert_eq!(state.query(), "");
    }

    // Drawing has no return value to assert on, so this checks the bytes
    // for what has to reach the screen, then runs the sizes where the
    // layout arithmetic could underflow. A 1x1 terminal leaves room for
    // nothing at all and still must not panic.
    #[test]
    fn rendering_covers_rows_scopes_and_small_terminals() {
        let mut state = test_state("");
        let mut output = Vec::new();
        // "printf foo\nbar" has to arrive flattened onto one row.
        draw_to(&mut output, &state, 80, 5, 200).expect("draw");
        let rendered = String::from_utf8_lossy(&output);
        assert!(rendered.contains("git status"));
        assert!(rendered.contains("printf foo  bar"));
        assert!(rendered.contains("2 matches"));

        state.toggle_dir_only();
        output.clear();
        draw_to(&mut output, &state, 8, 2, 200).expect("draw dir");
        assert!(String::from_utf8_lossy(&output).contains("dir"));

        let long = test_state("git status with a long query");
        output.clear();
        draw_to(&mut output, &long, 1, 1, 200).expect("draw small");
        assert!(!output.is_empty());
    }
}
