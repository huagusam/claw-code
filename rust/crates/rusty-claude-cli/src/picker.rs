use std::io::{self, Write};
use std::time::Duration;

use crossterm::cursor::{MoveToColumn, MoveToNextLine, MoveToPreviousLine};
use crossterm::event::{poll, read, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Print, Stylize};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use crossterm::{execute, queue};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, PartialEq, Eq)]
enum PickerKind {
    SlashCommands,
    Mentions,
    Skills,
}

enum InputMode {
    Insert,
    Picker {
        kind: PickerKind,
        filter: String,
        items: Vec<String>,
        matched: Vec<usize>,
        selected: usize,
    },
}

pub enum PickerResult {
    Submit(String),
    Cancel,
    Exit,
}

struct PickerState {
    buffer: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    history_pos: Option<usize>,
    mode: InputMode,
    completions: Vec<String>,
    mention_names: Vec<String>,
    skill_names: Vec<String>,
}

impl PickerState {
    fn new(completions: Vec<String>, mention_names: Vec<String>, skill_names: Vec<String>) -> Self {
        Self {
            buffer: Vec::new(),
            cursor: 0,
            history: Vec::new(),
            history_pos: None,
            mode: InputMode::Insert,
            completions,
            mention_names,
            skill_names,
        }
    }

    fn buffer_str(&self) -> String {
        self.buffer.iter().collect()
    }

    fn insert_char(&mut self, c: char) {
        if self.cursor > self.buffer.len() {
            self.cursor = self.buffer.len();
        }
        self.buffer.insert(self.cursor, c);
        self.cursor += 1;
    }

    fn delete_before(&mut self) {
        if self.cursor > 0 && !self.buffer.is_empty() {
            self.cursor -= 1;
            self.buffer.remove(self.cursor);
        }
    }

    fn delete_after(&mut self) {
        if self.cursor < self.buffer.len() {
            self.buffer.remove(self.cursor);
        }
    }

    fn cursor_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    fn cursor_right(&mut self) {
        if self.cursor < self.buffer.len() {
            self.cursor += 1;
        }
    }

    fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    fn cursor_end(&mut self) {
        self.cursor = self.buffer.len();
    }

    fn word_left(&mut self) {
        let before = &self.buffer[..self.cursor];
        if let Some(pos) = before.iter().rposition(|&c| c == ' ') {
            self.cursor = pos + 1;
        } else {
            self.cursor = 0;
        }
    }

    fn word_right(&mut self) {
        let after = &self.buffer[self.cursor..];
        if let Some(pos) = after.iter().position(|&c| c == ' ') {
            self.cursor += pos + 1;
        } else {
            self.cursor = self.buffer.len();
        }
    }

    fn enter_history_older(&mut self) {
        let pos = match self.history_pos {
            Some(p) if p + 1 < self.history.len() => p + 1,
            _ => return,
        };
        self.history_pos = Some(pos);
        let line: Vec<char> = self.history[pos].chars().collect();
        self.buffer = line;
        self.cursor = self.buffer.len();
    }

    fn enter_history_newer(&mut self) {
        match self.history_pos {
            Some(0) => {
                self.history_pos = None;
                self.buffer.clear();
                self.cursor = 0;
            }
            Some(p) => {
                self.history_pos = Some(p - 1);
                let line: Vec<char> = self.history[p - 1].chars().collect();
                self.buffer = line;
                self.cursor = self.buffer.len();
            }
            None => {}
        }
    }

    fn enter_slash_picker(&mut self) {
        let filter = self.buffer_str();
        let items: Vec<String> = self.completions.clone();
        let mut matched: Vec<usize> = (0..items.len()).collect();
        if !filter.is_empty() && filter != "/" {
            let q = if filter.starts_with('/') { &filter[1..] } else { &filter };
            matched.retain(|&i| items[i].to_lowercase().contains(&q.to_lowercase()));
        }
        matched.sort_by(|&a, &b| items[a].cmp(&items[b]));
        self.mode = InputMode::Picker {
            kind: PickerKind::SlashCommands,
            filter,
            items,
            matched,
            selected: 0,
        };
    }

    fn enter_mention_picker(&mut self) {
        let before_cursor: String = self.buffer[..self.cursor].iter().collect();
        let at_pos = before_cursor.rfind('@');
        let filter: String = match at_pos {
            Some(pos) if pos + 1 < before_cursor.len() => before_cursor[pos + 1..].to_string(),
            _ => String::new(),
        };
        let items: Vec<String> = self.mention_names.clone();
        let mut matched: Vec<usize> = (0..items.len()).collect();
        if !filter.is_empty() {
            matched.retain(|&i| items[i].to_lowercase().starts_with(&filter.to_lowercase()));
        }
        matched.sort_by(|&a, &b| items[a].cmp(&items[b]));
        self.mode = InputMode::Picker {
            kind: PickerKind::Mentions,
            filter,
            items,
            matched,
            selected: 0,
        };
    }

    fn enter_skill_picker(&mut self) {
        let before_cursor: String = self.buffer[..self.cursor].iter().collect();
        let dollar_pos = before_cursor.rfind('$');
        let filter: String = match dollar_pos {
            Some(pos) if pos + 1 < before_cursor.len() => before_cursor[pos + 1..].to_string(),
            _ => String::new(),
        };
        let items: Vec<String> = self.skill_names.clone();
        let mut matched: Vec<usize> = (0..items.len()).collect();
        if !filter.is_empty() {
            matched.retain(|&i| items[i].to_lowercase().starts_with(&filter.to_lowercase()));
        }
        matched.sort_by(|&a, &b| items[a].cmp(&items[b]));
        self.mode = InputMode::Picker {
            kind: PickerKind::Skills,
            filter,
            items,
            matched,
            selected: 0,
        };
    }

    fn picker_selected_item(&self) -> Option<String> {
        if let InputMode::Picker { ref items, ref matched, selected, .. } = self.mode {
            if matched.is_empty() {
                return None;
            }
            let idx = selected.min(matched.len() - 1);
            return Some(items[matched[idx]].clone());
        }
        None
    }

    fn apply_picker_selection(&mut self) {
        let item = match self.picker_selected_item() {
            Some(i) => i,
            None => return,
        };
        match self.mode {
            InputMode::Picker { kind: PickerKind::SlashCommands, .. } => {
                if let Some(slash_pos) = self.buffer[..self.cursor].iter().rposition(|c| *c == '/') {
                    let prefix: Vec<char> = self.buffer[..slash_pos].to_vec();
                    self.buffer = prefix.into_iter().chain(item.chars()).collect();
                } else {
                    self.buffer = item.chars().collect();
                }
                self.cursor = self.buffer.len();
            }
            InputMode::Picker { kind: PickerKind::Mentions, .. } => {
                let before_cursor: String = self.buffer[..self.cursor].iter().collect();
                if let Some(at_pos) = before_cursor.rfind('@') {
                    let char_pos = before_cursor[..at_pos].chars().count();
                    self.buffer.truncate(char_pos);
                    self.buffer.push('@');
                    for c in item.chars() {
                        self.buffer.push(c);
                    }
                    self.buffer.push(' ');
                    self.cursor = self.buffer.len();
                }
            }
            InputMode::Picker { kind: PickerKind::Skills, .. } => {
                let before_cursor: String = self.buffer[..self.cursor].iter().collect();
                if let Some(dollar_pos) = before_cursor.rfind('$') {
                    let char_pos = before_cursor[..dollar_pos].chars().count();
                    self.buffer.truncate(char_pos);
                    self.buffer.push('$');
                    for c in item.chars() {
                        self.buffer.push(c);
                    }
                    self.buffer.push(' ');
                    self.cursor = self.buffer.len();
                }
            }
            _ => {}
        }
        self.mode = InputMode::Insert;
    }

    fn picker_up(&mut self) {
        if let InputMode::Picker { ref mut selected, ref matched, .. } = self.mode {
            if !matched.is_empty() {
                *selected = if *selected == 0 { matched.len() - 1 } else { *selected - 1 };
            }
        }
    }

    fn picker_down(&mut self) {
        if let InputMode::Picker { ref mut selected, ref matched, .. } = self.mode {
            if !matched.is_empty() {
                *selected = if *selected + 1 >= matched.len() { 0 } else { *selected + 1 };
            }
        }
    }

    fn picker_update_filter(&mut self, c: char) {
        if let InputMode::Picker { ref mut filter, ref items, ref mut matched, ref mut selected, .. } = self.mode {
            filter.push(c);
            let q = if filter.starts_with('/') { &filter[1..] } else { &filter };
            *matched = if q.is_empty() {
                (0..items.len()).collect()
            } else {
                (0..items.len()).filter(|&i| items[i].to_lowercase().contains(&q.to_lowercase())).collect()
            };
            matched.sort_by(|&a, &b| items[a].cmp(&items[b]));
            *selected = 0;
        }
    }

    fn picker_backspace_filter(&mut self) {
        if let InputMode::Picker { ref mut filter, ref items, ref mut matched, ref mut selected, .. } = self.mode {
            filter.pop();
            if filter.is_empty() || filter == "/" {
                *matched = (0..items.len()).collect();
            } else {
                let q = if filter.starts_with('/') { &filter[1..] } else { &filter };
                *matched = (0..items.len()).filter(|&i| items[i].to_lowercase().contains(&q.to_lowercase())).collect();
            }
            matched.sort_by(|&a, &b| items[a].cmp(&items[b]));
            *selected = 0;
        }
    }

    fn push_history(&mut self, line: String) {
        if !line.trim().is_empty() {
            self.history.push(line);
        }
        self.history_pos = None;
    }
}

fn build_display(state: &PickerState) -> (Vec<char>, usize) {
    match &state.mode {
        InputMode::Picker { kind: PickerKind::SlashCommands, filter, .. } => {
            let chars: Vec<char> = filter.chars().collect();
            let cursor = chars.len();
            (chars, cursor)
        }
        InputMode::Picker { kind: PickerKind::Mentions, filter, .. } => {
            let before_cursor: String = state.buffer[..state.cursor].iter().collect();
            if let Some(at_pos) = before_cursor.rfind('@') {
                let char_pos = before_cursor[..at_pos].chars().count();
                let mut display: Vec<char> = Vec::with_capacity(char_pos + 1 + filter.len());
                display.extend(state.buffer[..char_pos].iter());
                display.push('@');
                display.extend(filter.chars());
                let cursor = display.len();
                (display, cursor)
            } else {
                (state.buffer.clone(), state.cursor)
            }
        }
        InputMode::Picker { kind: PickerKind::Skills, filter, .. } => {
            let before_cursor: String = state.buffer[..state.cursor].iter().collect();
            if let Some(dollar_pos) = before_cursor.rfind('$') {
                let char_pos = before_cursor[..dollar_pos].chars().count();
                let mut display: Vec<char> = Vec::with_capacity(char_pos + 1 + filter.len());
                display.extend(state.buffer[..char_pos].iter());
                display.push('$');
                display.extend(filter.chars());
                let cursor = display.len();
                (display, cursor)
            } else {
                (state.buffer.clone(), state.cursor)
            }
        }
        _ => (state.buffer.clone(), state.cursor),
    }
}

fn render_input_line<W: Write>(stdout: &mut W, prompt: &str, buffer: &[char], cursor: usize) -> io::Result<u16> {
    let line: String = buffer.iter().collect();
    let term_cols = terminal_width() as usize;
    let prompt_width = UnicodeWidthStr::width(prompt);
    let max_text_cols = term_cols.saturating_sub(prompt_width + 1);

    let cursor_char_idx = cursor.min(line.chars().count());

    // Calculate cursor column relative to the LAST line in the buffer.
    // For multi-line input, the cursor is on whichever line the last \n
    // before it belongs to — the prompt only prefixes the first line.
    fn cursor_col_on_last_line(line: &str, cursor_char_idx: usize, prompt_width: usize) -> usize {
        let before_byte = line.char_indices()
            .nth(cursor_char_idx)
            .map(|(i, _)| i)
            .unwrap_or(line.len());
        let text_before = &line[..before_byte];
        if let Some(last_nl) = text_before.rfind('\n') {
            // Cursor is on a continuation line — no prompt offset
            UnicodeWidthStr::width(&text_before[last_nl + 1..])
        } else {
            // Single line — prompt is visible
            prompt_width + UnicodeWidthStr::width(text_before)
        }
    }

    let (display, cursor_col) = if UnicodeWidthStr::width(line.as_str()) > max_text_cols {
        // Walk chars from the right to find where to start so display fits max_text_cols
        let chars: Vec<char> = line.chars().collect();
        let mut remaining = max_text_cols;
        let mut split_char = chars.len();
        for (i, &c) in chars.iter().enumerate().rev() {
            let w = UnicodeWidthChar::width(c).unwrap_or(0);
            if w <= remaining {
                remaining -= w;
                split_char = i;
            } else {
                break;
            }
        }
        let start_byte: usize = chars[..split_char].iter().map(|c| c.len_utf8()).sum();
        let d = &line[start_byte..];
        let shifted_cursor = cursor.saturating_sub(split_char).min(d.chars().count());
        // After truncation, no prompt prefix is shown
        let vis = d.chars().take(shifted_cursor).map(|c| UnicodeWidthChar::width(c).unwrap_or(0)).sum::<usize>();
        (d.to_string(), vis)
    } else {
        let col = cursor_col_on_last_line(&line, cursor_char_idx, prompt_width);
        (line.clone(), col)
    };

    queue!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
    write!(stdout, "{}{}", prompt, display)?;
    queue!(stdout, MoveToColumn(cursor_col as u16))?;
    Ok(cursor_col as u16)
}

fn write_highlighted<W: Write>(
    stdout: &mut W,
    text: &str,
    query: &str,
    selected: bool,
) -> io::Result<()> {
    let q_lower: Vec<char> = query.to_lowercase().chars().collect();
    if q_lower.is_empty() {
        if selected {
            write!(stdout, " {}", text.on_dark_grey().white())?;
        } else {
            write!(stdout, " {}", text.dark_grey())?;
        }
        return Ok(());
    }

    // Use character-level matching to avoid byte-offset mismatch between
    // to_lowercase() output and original text (which can panic on CJK/Unicode text).
    let text_chars: Vec<char> = text.chars().collect();
    let match_char_pos = text_chars
        .windows(q_lower.len())
        .position(|w| {
            w.iter()
                .zip(q_lower.iter())
                .all(|(tc, qc)| {
                    tc.to_lowercase().collect::<String>() == qc.to_lowercase().collect::<String>()
                })
        });

    if let Some(char_pos) = match_char_pos {
        let byte_pos: usize = text_chars[..char_pos].iter().map(|c| c.len_utf8()).sum();
        let match_byte_len: usize = text_chars[char_pos..char_pos + q_lower.len()]
            .iter()
            .map(|c| c.len_utf8())
            .sum();
        let byte_end = byte_pos + match_byte_len;

        let before = &text[..byte_pos];
        let matched_part = &text[byte_pos..byte_end];
        let after = &text[byte_end..];

        if selected {
            write!(stdout, " {}", before.on_dark_grey().white())?;
            write!(stdout, "{}", matched_part.on_dark_grey().white().bold())?;
            write!(stdout, "{}", after.on_dark_grey().white())?;
        } else {
            write!(stdout, " {}", before.dark_grey())?;
            write!(stdout, "{}", matched_part.white().bold())?;
            write!(stdout, "{}", after.dark_grey())?;
        }
    } else {
        if selected {
            write!(stdout, " {}", text.on_dark_grey().white())?;
        } else {
            write!(stdout, " {}", text.dark_grey())?;
        }
    }
    Ok(())
}

fn render_picker_overlay<W: Write>(
    stdout: &mut W,
    items: &[String],
    matched: &[usize],
    selected: usize,
    filter: &str,
) -> io::Result<usize> {
    let term_height = terminal_height() as usize;
    let max_visible = 10.min(term_height.saturating_sub(3));

    let query = if filter.starts_with('/') { &filter[1..] } else { filter };

    if matched.is_empty() {
        queue!(stdout, MoveToNextLine(1))?;
        write!(stdout, "  {}", "(no matches)".dark_grey())?;
        queue!(stdout, Clear(ClearType::UntilNewLine))?;
        return Ok(1);
    }

    let visible_count = max_visible.min(matched.len());
    let scroll_offset = if selected >= visible_count {
        selected - visible_count + 1
    } else {
        0
    };

    let mut total: usize = 0;

    if scroll_offset > 0 {
        queue!(stdout, MoveToNextLine(1), Clear(ClearType::CurrentLine))?;
        write!(stdout, "  ... {} more", scroll_offset.to_string().dark_grey())?;
        queue!(stdout, Clear(ClearType::UntilNewLine))?;
        total += 1;
    }

    for i in 0..visible_count {
        let idx = scroll_offset + i;
        if idx >= matched.len() {
            break;
        }
        let item = &items[matched[idx]];
        queue!(stdout, MoveToNextLine(1), Clear(ClearType::CurrentLine))?;
        if idx == selected {
            write!(stdout, "{}", "▶".white())?;
        } else {
            write!(stdout, "  ")?;
        }
        write_highlighted(stdout, item, query, idx == selected)?;
        queue!(stdout, Clear(ClearType::UntilNewLine))?;
        total += 1;
    }

    let items_below = matched.len().saturating_sub(scroll_offset + visible_count);
    if items_below > 0 {
        queue!(stdout, MoveToNextLine(1), Clear(ClearType::CurrentLine))?;
        write!(stdout, "  ... {} more", items_below.to_string().dark_grey())?;
        queue!(stdout, Clear(ClearType::UntilNewLine))?;
        total += 1;
    }

    Ok(total)
}

fn terminal_width() -> u16 {
    crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80)
}

fn terminal_height() -> u16 {
    crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24)
}

pub fn run_picker(
    prompt: &str,
    completions: &[String],
    mention_names: &[String],
    skill_names: &[String],
) -> io::Result<PickerResult> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    let mut state = PickerState::new(completions.to_vec(), mention_names.to_vec(), skill_names.to_vec());
    let mut dirty = true;

    let result = loop {
        // Only re-render when state has changed — this eliminates the 50ms
        // flicker caused by full clear+redraw on every poll iteration.
        if dirty {
            execute!(stdout, MoveToColumn(0), Clear(ClearType::FromCursorDown))?;
            queue!(stdout, Clear(ClearType::CurrentLine))?;
            let (display_buffer, display_cursor) = build_display(&state);
            let cursor_col = render_input_line(&mut stdout, prompt, &display_buffer, display_cursor)?;
            let overlay_lines = if let InputMode::Picker { ref matched, ref items, selected, ref filter, .. } = state.mode {
                render_picker_overlay(&mut stdout, items, matched, selected, filter)?
            } else {
                0
            };
            // Reposition cursor back to the input line so the next render
            // starts at the correct row (prevents input-line drift).
            if overlay_lines > 0 {
                queue!(stdout, MoveToPreviousLine(overlay_lines as u16))?;
                queue!(stdout, MoveToColumn(cursor_col))?;
            }
            stdout.flush()?;
            dirty = false;
        }

        if !poll(Duration::from_millis(100))? {
            continue;
        }

        let event = read()?;

        // Every event that reaches here triggers a redraw — this is fine because
        // rendering only happens on actual user input, not on every poll tick.
        dirty = true;

        match event {
            // On Windows, crossterm emits both Press and Release for every
            // keystroke. Discard Release to prevent character doubling.
            Event::Key(KeyEvent { kind: KeyEventKind::Release, .. }) => {}
            Event::Key(KeyEvent { code, modifiers, .. }) => {
                match &state.mode {
                    InputMode::Insert => {
                        match code {
                            KeyCode::Enter => {
                                let line = state.buffer_str();
                                // Auto-trigger slash picker on bare "/"
                                if modifiers == KeyModifiers::NONE && line.trim() == "/" {
                                    state.enter_slash_picker();
                                    continue;
                                }
                                // Auto-trigger mention picker on trailing @word
                                if modifiers == KeyModifiers::NONE {
                                    let before_cursor: String = state.buffer[..state.cursor].iter().collect();
                                    if let Some(at_pos) = before_cursor.rfind('@') {
                                        let after_at = &before_cursor[at_pos + 1..];
                                        if !after_at.is_empty() && !after_at.contains(char::is_whitespace) {
                                            state.enter_mention_picker();
                                            continue;
                                        }
                                    }
                                }
                                // Auto-trigger skill picker on trailing $word
                                if modifiers == KeyModifiers::NONE {
                                    let before_cursor: String = state.buffer[..state.cursor].iter().collect();
                                    if let Some(dollar_pos) = before_cursor.rfind('$') {
                                        let after_dollar = &before_cursor[dollar_pos + 1..];
                                        if !after_dollar.is_empty() && !after_dollar.contains(char::is_whitespace) {
                                            state.enter_skill_picker();
                                            continue;
                                        }
                                    }
                                }
                                if modifiers == KeyModifiers::SHIFT {
                                    state.insert_char('\n');
                                    continue;
                                }
                                state.push_history(line.clone());
                                break PickerResult::Submit(line);
                            }
                            KeyCode::Tab => {
                                let before_cursor: String = state.buffer[..state.cursor].iter().collect();
                                if before_cursor.starts_with('/') {
                                    state.enter_slash_picker();
                                } else if before_cursor.contains('@') {
                                    state.enter_mention_picker();
                                } else if before_cursor.contains('$') {
                                    state.enter_skill_picker();
                                } else {
                                    state.enter_slash_picker();
                                }
                            }
                            KeyCode::Char(c) => {
                                if c == 'c' && modifiers == KeyModifiers::CONTROL {
                                    break PickerResult::Exit;
                                }
                                if c == 'd' && modifiers == KeyModifiers::CONTROL {
                                    break PickerResult::Exit;
                                }
                                state.insert_char(c);
                                // Auto-trigger on "/" or "@" or "$"
                                if c == '/' && state.buffer.len() == 1 {
                                    state.enter_slash_picker();
                                } else if c == '@' {
                                    let before_cursor: String = state.buffer[..state.cursor].iter().collect();
                                    if !before_cursor[..before_cursor.len().saturating_sub(1)].contains('@') {
                                        state.enter_mention_picker();
                                    }
                                } else if c == '$' {
                                    let before_cursor: String = state.buffer[..state.cursor].iter().collect();
                                    if !before_cursor[..before_cursor.len().saturating_sub(1)].contains('$') {
                                        state.enter_skill_picker();
                                    }
                                }
                            }
                            KeyCode::Backspace => state.delete_before(),
                            KeyCode::Delete => state.delete_after(),
                            KeyCode::Left => state.cursor_left(),
                            KeyCode::Right => state.cursor_right(),
                            KeyCode::Home => state.cursor_home(),
                            KeyCode::End => state.cursor_end(),
                            KeyCode::Up => state.enter_history_older(),
                            KeyCode::Down => state.enter_history_newer(),
                            KeyCode::Esc => {
                                break PickerResult::Cancel;
                            }
                            _ => {}
                        }
                    }
                    InputMode::Picker { .. } => {
                        match code {
                            KeyCode::Up => state.picker_up(),
                            KeyCode::Down => state.picker_down(),
                            KeyCode::Enter => {
                                state.apply_picker_selection();
                            }
                            KeyCode::Tab => {
                                state.apply_picker_selection();
                            }
                            KeyCode::Esc => {
                                // Dismiss picker, copy typed filter back to buffer
                                // so the user's typed characters aren't lost.
                                let (buf, cur) = build_display(&state);
                                state.buffer = buf;
                                state.cursor = cur;
                                state.mode = InputMode::Insert;
                            }
                            KeyCode::Backspace => state.picker_backspace_filter(),
                            KeyCode::Char(c) => {
                                if c == 'c' && modifiers == KeyModifiers::CONTROL {
                                    break PickerResult::Exit;
                                }
                                if c == 'd' && modifiers == KeyModifiers::CONTROL {
                                    break PickerResult::Exit;
                                }
                                state.picker_update_filter(c);
                            }
                            KeyCode::Left | KeyCode::Right => {
                                match code {
                                    KeyCode::Left => state.cursor_left(),
                                    KeyCode::Right => state.cursor_right(),
                                    _ => {}
                                }
                                // Re-filter mention/skill picker based on new cursor
                                if matches!(state.mode, InputMode::Picker { kind: PickerKind::Mentions, .. }) {
                                    let before_cursor: String = state.buffer[..state.cursor].iter().collect();
                                    if let Some(at_pos) = before_cursor.rfind('@') {
                                        let new_filter: String = before_cursor[at_pos + 1..].to_string();
                                        if let InputMode::Picker { ref mut filter, ref items, ref mut matched, ref mut selected, .. } = state.mode {
                                            *filter = new_filter;
                                            *matched = if filter.is_empty() {
                                                (0..items.len()).collect()
                                            } else {
                                                (0..items.len()).filter(|&i| items[i].to_lowercase().starts_with(&filter.to_lowercase())).collect()
                                            };
                                            matched.sort_by(|&a, &b| items[a].cmp(&items[b]));
                                            *selected = 0;
                                        }
                                    }
                                }
                                if matches!(state.mode, InputMode::Picker { kind: PickerKind::Skills, .. }) {
                                    let before_cursor: String = state.buffer[..state.cursor].iter().collect();
                                    if let Some(dollar_pos) = before_cursor.rfind('$') {
                                        let new_filter: String = before_cursor[dollar_pos + 1..].to_string();
                                        if let InputMode::Picker { ref mut filter, ref items, ref mut matched, ref mut selected, .. } = state.mode {
                                            *filter = new_filter;
                                            *matched = if filter.is_empty() {
                                                (0..items.len()).collect()
                                            } else {
                                                (0..items.len()).filter(|&i| items[i].to_lowercase().starts_with(&filter.to_lowercase())).collect()
                                            };
                                            matched.sort_by(|&a, &b| items[a].cmp(&items[b]));
                                            *selected = 0;
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            Event::Resize(_, _) => {}
            Event::Paste(text) => {
                match state.mode {
                    InputMode::Insert => {
                        for c in text.chars() {
                            state.insert_char(c);
                        }
                    }
                    InputMode::Picker { .. } => {
                        for c in text.chars() {
                            if c == '\n' || c == '\r' { continue; }
                            state.picker_update_filter(c);
                        }
                    }
                }
            }
            _ => {}
        }
    };

    // Clean up – clear any leftover picker overlay and restore terminal state.
    // On Submit, preserve the input line (like rustyline) so user sees their text.
    // On Cancel/Exit, clear the line.
    if matches!(result, PickerResult::Submit(_)) {
        execute!(stdout, MoveToColumn(0))?;
    } else {
        execute!(stdout, MoveToColumn(0), Clear(ClearType::FromCursorDown))?;
    }
    disable_raw_mode()?;
    writeln!(stdout)?;

    Ok(result)
}
