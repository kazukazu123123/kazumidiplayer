//! Multi-line plain text editor with optional syntax highlighting.
//!
//! Keep a [`TextEditorState`] somewhere, feed it keys with
//! [`TextEditorState::update`] once per frame, and draw it with
//! [`draw_text_editor`]. Colors come from an optional [`SyntaxHighlighter`].

use super::scrollbar::draw_scrollbar;
use raylib::prelude::*;
use std::ops::Range;

const FONT_SIZE: f32 = 16.0;
const SPACING: f32 = 1.0;
const LINE_HEIGHT: f32 = 20.0;
const PADDING: f32 = 8.0;
const BLINK_INTERVAL: f32 = 0.5;
const SCROLLBAR_WIDTH: f32 = 10.0;

/// A colored part of one line. `range` is in bytes of that line and must lie
/// on char boundaries; text outside every span uses the default color.
pub struct Span {
    pub range: Range<usize>,
    pub color: Color,
}

/// Colors lines for [`draw_text_editor`]. Called only for visible lines.
pub trait SyntaxHighlighter {
    /// Push spans for `line` (without its `'\n'`) into `out`, in order and
    /// without overlap. `index` is the 0-based line number.
    fn highlight_line(&self, index: usize, line: &str, out: &mut Vec<Span>);
}

/// Example highlighter: dims lines whose first non-blank text is a marker.
pub struct LineCommentHighlighter {
    pub markers: &'static [&'static str],
    pub color: Color,
}

impl SyntaxHighlighter for LineCommentHighlighter {
    fn highlight_line(&self, _index: usize, line: &str, out: &mut Vec<Span>) {
        let trimmed = line.trim_start();
        if self.markers.iter().any(|m| trimmed.starts_with(m)) {
            out.push(Span {
                range: line.len() - trimmed.len()..line.len(),
                color: self.color,
            });
        }
    }
}

/// Keyboard input for one frame.
#[derive(Default)]
pub struct TextEditorKeys {
    pub typed: Vec<char>,
    pub backspace: bool,
    pub delete: bool,
    pub enter: bool,
    pub left: bool,
    pub right: bool,
    pub up: bool,
    pub down: bool,
    pub home: bool,
    pub end: bool,
    /// Extend the selection with cursor moves.
    pub shift: bool,
    pub select_all: bool,
    pub copy: bool,
    pub cut: bool,
    /// Clipboard contents when paste was requested.
    pub paste: Option<String>,
    pub escape: bool,
}

impl TextEditorKeys {
    /// Read this frame's keys. Consumes raylib's typed-character queue.
    /// Editing and movement keys repeat while held.
    pub fn poll(rl: &mut RaylibHandle) -> Self {
        let mut typed = Vec::new();
        while let Some(ch) = rl.get_char_pressed() {
            typed.push(ch);
        }
        let held = |rl: &RaylibHandle, key| rl.is_key_pressed(key) || rl.is_key_pressed_repeat(key);
        let either = |rl: &RaylibHandle, a, b| rl.is_key_down(a) || rl.is_key_down(b);
        let ctrl = either(
            rl,
            KeyboardKey::KEY_LEFT_CONTROL,
            KeyboardKey::KEY_RIGHT_CONTROL,
        );
        let shift = either(
            rl,
            KeyboardKey::KEY_LEFT_SHIFT,
            KeyboardKey::KEY_RIGHT_SHIFT,
        );
        let paste = if ctrl && held(rl, KeyboardKey::KEY_V) {
            rl.get_clipboard_text().ok()
        } else {
            None
        };
        Self {
            typed,
            backspace: held(rl, KeyboardKey::KEY_BACKSPACE),
            delete: held(rl, KeyboardKey::KEY_DELETE),
            enter: held(rl, KeyboardKey::KEY_ENTER),
            left: held(rl, KeyboardKey::KEY_LEFT),
            right: held(rl, KeyboardKey::KEY_RIGHT),
            up: held(rl, KeyboardKey::KEY_UP),
            down: held(rl, KeyboardKey::KEY_DOWN),
            home: held(rl, KeyboardKey::KEY_HOME),
            end: held(rl, KeyboardKey::KEY_END),
            shift,
            select_all: ctrl && rl.is_key_pressed(KeyboardKey::KEY_A),
            copy: ctrl && rl.is_key_pressed(KeyboardKey::KEY_C),
            cut: ctrl && rl.is_key_pressed(KeyboardKey::KEY_X),
            paste,
            escape: rl.is_key_pressed(KeyboardKey::KEY_ESCAPE),
        }
    }
}

/// What [`TextEditorState::handle_keys`] wants the caller to do.
#[derive(Default, Debug, PartialEq)]
pub struct KeyOutcome {
    /// Escape dropped focus; leave a second Escape for closing the container.
    pub focus_dropped: bool,
    /// Text to put on the clipboard (copy or cut).
    pub copied: Option<String>,
}

/// Mouse input for one frame, in the same coordinates as the editor's rect.
pub struct TextEditorMouse {
    pub pos: Vector2,
    pub click: Option<Vector2>,
    pub down: bool,
    pub wheel: f32,
    /// Shift-click extends the selection.
    pub shift: bool,
}

/// One displayed row: a whole line, or a part of it when wrapped.
/// Positions are in chars of the whole text; `end` excludes the line break.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Row {
    start: usize,
    end: usize,
    /// Index of the logical line and the char position where it starts.
    line: usize,
    line_start: usize,
    /// The line continues on the next row (`end` is that row's `start`).
    wraps: bool,
}

/// One row per logical line, without wrapping.
fn logical_rows(text: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut start = 0;
    for (line, s) in text.split('\n').enumerate() {
        let end = start + s.chars().count();
        rows.push(Row {
            start,
            end,
            line,
            line_start: start,
            wraps: false,
        });
        start = end + 1;
    }
    rows
}

pub struct TextEditorState {
    text: String,
    /// Wrap long lines at the editor width; otherwise scroll horizontally.
    pub wrap: bool,
    /// Rows from the last draw; stale after an edit until the next draw.
    rows: Vec<Row>,
    rows_valid: bool,
    scroll_x: f32,
    /// In chars, `0..=text.chars().count()`.
    cursor: usize,
    /// Other end of the selection, in chars. No selection when equal to cursor.
    anchor: usize,
    pub focused: bool,
    /// A click in the text started a drag that moves the cursor.
    mouse_selecting: bool,
    /// A key moved the cursor or edited; scroll it into view on the next draw.
    /// Only then, so the wheel and scrollbar can move away from the cursor.
    follow_cursor: bool,
    scroll_y: f32,
    scrollbar_dragging: bool,
    scrollbar_drag_start_y: f32,
    scrollbar_drag_start_scroll: f32,
    blink_timer: f32,
    cursor_visible: bool,
}

impl Default for TextEditorState {
    fn default() -> Self {
        Self {
            text: String::new(),
            wrap: true,
            rows: Vec::new(),
            rows_valid: false,
            scroll_x: 0.0,
            cursor: 0,
            anchor: 0,
            focused: false,
            mouse_selecting: false,
            follow_cursor: false,
            scroll_y: 0.0,
            scrollbar_dragging: false,
            scrollbar_drag_start_y: 0.0,
            scrollbar_drag_start_scroll: 0.0,
            blink_timer: 0.0,
            cursor_visible: true,
        }
    }
}

impl TextEditorState {
    pub fn text(&self) -> &str {
        &self.text
    }

    #[allow(dead_code)] // API for future users; the debug editor starts empty.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.rows_valid = false;
        self.cursor = self.cursor.min(self.char_count());
        self.anchor = self.cursor;
    }

    /// Unfocus, put the cursor at the end and restart blinking, e.g. when the
    /// editor is shown again.
    pub fn reset_interaction(&mut self) {
        self.focused = false;
        self.mouse_selecting = false;
        self.cursor = self.char_count();
        self.anchor = self.cursor;
        self.reset_blink();
    }

    /// Read keys from raylib, apply them and update the clipboard. Returns
    /// true when Escape only dropped focus (see [`KeyOutcome`]).
    pub fn update(&mut self, rl: &mut RaylibHandle, dt: f32) -> bool {
        let keys = TextEditorKeys::poll(rl);
        let outcome = self.handle_keys(&keys, dt);
        if let Some(text) = outcome.copied {
            let _ = rl.set_clipboard_text(&text);
        }
        outcome.focus_dropped
    }

    fn reset_blink(&mut self) {
        self.blink_timer = 0.0;
        self.cursor_visible = true;
    }

    fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    fn byte_at(&self, char_pos: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_pos)
            .map_or(self.text.len(), |(i, _)| i)
    }

    /// Selected char range, empty when nothing is selected.
    fn selection(&self) -> Range<usize> {
        self.cursor.min(self.anchor)..self.cursor.max(self.anchor)
    }

    fn selected_text(&self) -> &str {
        let sel = self.selection();
        &self.text[self.byte_at(sel.start)..self.byte_at(sel.end)]
    }

    /// Remove the selection; false when there was none.
    fn delete_selection(&mut self) -> bool {
        let sel = self.selection();
        if sel.is_empty() {
            return false;
        }
        let (start, end) = (self.byte_at(sel.start), self.byte_at(sel.end));
        self.text.drain(start..end);
        self.rows_valid = false;
        self.cursor = sel.start;
        self.anchor = sel.start;
        true
    }

    /// Move the cursor; without `extend` the selection collapses.
    fn move_to(&mut self, pos: usize, extend: bool) {
        self.cursor = pos.min(self.char_count());
        if !extend {
            self.anchor = self.cursor;
        }
    }

    /// Rows of the last draw, or unwrapped rows when the text changed since.
    fn rows(&self) -> std::borrow::Cow<'_, [Row]> {
        if self.rows_valid {
            std::borrow::Cow::Borrowed(&self.rows)
        } else {
            std::borrow::Cow::Owned(logical_rows(&self.text))
        }
    }

    /// Row and column (in chars) of the cursor. At a wrap point the cursor
    /// belongs to the next row.
    fn cursor_row_col(&self) -> (usize, usize) {
        let rows = self.rows();
        let row = row_of(&rows, self.cursor);
        (row, self.cursor - rows[row].start)
    }

    /// Char position of `col` on `row`, clamped to that row.
    fn pos_at(&self, row: usize, col: usize) -> usize {
        let rows = self.rows();
        rows.get(row)
            .map_or(self.char_count(), |r| pos_in_row(r, col))
    }

    /// Apply this frame's keys. Ignored (and blinking paused) while unfocused.
    pub fn handle_keys(&mut self, keys: &TextEditorKeys, dt: f32) -> KeyOutcome {
        let mut outcome = KeyOutcome::default();
        if !self.focused {
            self.reset_blink();
            return outcome;
        }
        if keys.escape {
            self.focused = false;
            self.mouse_selecting = false;
            self.reset_blink();
            outcome.focus_dropped = true;
            return outcome;
        }
        let before = (self.cursor, self.anchor, self.text.len());

        if keys.select_all {
            self.anchor = 0;
            self.cursor = self.char_count();
        }
        if keys.copy || keys.cut {
            let selected = self.selected_text();
            if !selected.is_empty() {
                outcome.copied = Some(selected.to_owned());
                if keys.cut {
                    self.delete_selection();
                }
            }
        }
        if let Some(text) = &keys.paste {
            // Normalize Windows line endings; the editor only knows '\n'.
            self.insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
        }
        for &ch in &keys.typed {
            self.insert_str(ch.encode_utf8(&mut [0; 4]));
        }
        if keys.enter {
            self.insert_str("\n");
        }
        if keys.backspace && !self.delete_selection() && self.cursor > 0 {
            self.anchor = self.cursor - 1;
            self.delete_selection();
        }
        if keys.delete && !self.delete_selection() && self.cursor < self.char_count() {
            self.anchor = self.cursor + 1;
            self.delete_selection();
        }

        let sel = self.selection();
        if keys.left {
            let pos = if !keys.shift && !sel.is_empty() {
                sel.start
            } else {
                self.cursor.saturating_sub(1)
            };
            self.move_to(pos, keys.shift);
        }
        if keys.right {
            let pos = if !keys.shift && !sel.is_empty() {
                sel.end
            } else {
                self.cursor + 1
            };
            self.move_to(pos, keys.shift);
        }
        if keys.up {
            let (row, col) = self.cursor_row_col();
            let pos = if row == 0 {
                0
            } else {
                self.pos_at(row - 1, col)
            };
            self.move_to(pos, keys.shift);
        }
        if keys.down {
            let (row, col) = self.cursor_row_col();
            let last_row = self.rows().len() - 1;
            let pos = if row >= last_row {
                self.char_count()
            } else {
                self.pos_at(row + 1, col)
            };
            self.move_to(pos, keys.shift);
        }
        if keys.home {
            let (row, _) = self.cursor_row_col();
            self.move_to(self.pos_at(row, 0), keys.shift);
        }
        if keys.end {
            let (row, _) = self.cursor_row_col();
            self.move_to(self.pos_at(row, usize::MAX), keys.shift);
        }

        if (self.cursor, self.anchor, self.text.len()) != before {
            // Keep the cursor solid while it moves or the text changes.
            self.reset_blink();
            self.follow_cursor = true;
        } else {
            self.blink_timer += dt;
            if self.blink_timer >= BLINK_INTERVAL {
                self.blink_timer -= BLINK_INTERVAL;
                self.cursor_visible = !self.cursor_visible;
            }
        }
        outcome
    }

    /// Replace the selection (if any) with `s`.
    fn insert_str(&mut self, s: &str) {
        self.delete_selection();
        let at = self.byte_at(self.cursor);
        self.text.insert_str(at, s);
        self.rows_valid = false;
        self.cursor += s.chars().count();
        self.anchor = self.cursor;
    }
}

/// Row index holding char position `pos`.
fn row_of(rows: &[Row], pos: usize) -> usize {
    rows.iter()
        .position(|r| pos < r.end || (pos == r.end && !r.wraps))
        .unwrap_or(rows.len() - 1)
}

/// Char position `col` chars into `row`. A wrapped row stops before its last
/// char, since its end already belongs to the next row.
fn pos_in_row(row: &Row, col: usize) -> usize {
    let last = if row.wraps { row.end - 1 } else { row.end };
    row.start + col.min(last - row.start)
}

/// Rows for `text`, wrapping lines wider than `width` (preferably after a space).
fn wrapped_rows(font: &impl RaylibFont, text: &str, width: f32) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut line_start = 0;
    for (line, s) in text.split('\n').enumerate() {
        let chars: Vec<char> = s.chars().collect();
        // measure_text adds spacing between glyphs; count it after each one.
        let widths: Vec<f32> = chars
            .iter()
            .map(|c| {
                font.measure_text(c.encode_utf8(&mut [0; 4]), FONT_SIZE, SPACING)
                    .x
                    + SPACING
            })
            .collect();
        let mut start = 0;
        loop {
            // Take chars while they fit; always at least one.
            let mut end = start;
            let mut w = 0.0;
            while end < chars.len() && (end == start || w + widths[end] - SPACING <= width) {
                w += widths[end];
                end += 1;
            }
            if end < chars.len()
                && let Some(after_space) = (start + 1..=end).rev().find(|&i| chars[i - 1] == ' ')
            {
                end = after_space;
            }
            let wraps = end < chars.len();
            rows.push(Row {
                start: line_start + start,
                end: line_start + end,
                line,
                line_start,
                wraps,
            });
            if !wraps {
                break;
            }
            start = end;
        }
        line_start += chars.len() + 1;
    }
    rows
}

/// Char boundary in `line` nearest to `x` (relative to the line start).
fn column_at(font: &impl RaylibFont, line: &str, x: f32) -> usize {
    let mut prev_width = 0.0;
    for (col, (byte, ch)) in line.char_indices().enumerate() {
        let width = font
            .measure_text(&line[..byte + ch.len_utf8()], FONT_SIZE, SPACING)
            .x;
        if x < (prev_width + width) / 2.0 {
            return col;
        }
        prev_width = width;
    }
    line.chars().count()
}

/// Width of the first `col` chars of `line`.
fn prefix_width(font: &impl RaylibFont, line: &str, col: usize) -> f32 {
    let end = line.char_indices().nth(col).map_or(line.len(), |(b, _)| b);
    font.measure_text(&line[..end], FONT_SIZE, SPACING).x
}

/// Draw the editor in `rect` and handle the mouse: focus, placing the cursor,
/// drag and shift-click selection, scrolling and the scrollbar. Long lines wrap
/// when `state.wrap` is set; otherwise they scroll sideways (Shift+wheel).
/// `camera` maps `rect`'s coordinates to the screen (rotation is not
/// supported) and is used to clip the text. Returns true when the mouse wheel
/// was consumed.
pub fn draw_text_editor<D: RaylibDraw + RaylibScissorModeExt>(
    d: &mut D,
    camera: &Camera2D,
    font: &impl RaylibFont,
    rect: Rectangle,
    state: &mut TextEditorState,
    mouse: &TextEditorMouse,
    alpha: f32,
    highlighter: Option<&dyn SyntaxHighlighter>,
) -> bool {
    let a = |v: f32| (v * alpha) as u8;
    let text_color = Color::new(240, 240, 240, a(255.0));
    let selection_color = Color::new(38, 79, 120, a(255.0));
    let text_x = rect.x + PADDING;
    // Leave room for the vertical scrollbar on the right.
    let text_width = rect.width - PADDING * 2.0 - SCROLLBAR_WIDTH;

    state.rows = if state.wrap {
        wrapped_rows(font, &state.text, text_width)
    } else {
        logical_rows(&state.text)
    };
    state.rows_valid = true;

    let content_height = state.rows.len() as f32 * LINE_HEIGHT + PADDING * 2.0;
    let max_scroll = f32::max(content_height - rect.height, 0.0);
    let max_scroll_x = if state.wrap {
        0.0
    } else {
        let widest = state
            .text
            .split('\n')
            .map(|l| font.measure_text(l, FONT_SIZE, SPACING).x)
            .fold(0.0, f32::max);
        // A little extra so the cursor after the last char stays visible.
        f32::max(widest + FONT_SIZE - text_width, 0.0)
    };

    d.draw_rectangle_rec(rect, Color::new(30, 30, 30, a(255.0)));
    let border = if state.focused {
        Color::new(0, 170, 255, a(255.0))
    } else {
        Color::new(80, 80, 80, a(255.0))
    };
    d.draw_rectangle_lines_ex(rect, 1.0, border);

    let wheel_consumed = mouse.wheel != 0.0 && rect.check_collision_point_rec(mouse.pos);
    if wheel_consumed {
        if mouse.shift && !state.wrap {
            state.scroll_x -= mouse.wheel * 30.0;
        } else {
            state.scroll_y -= mouse.wheel * 30.0;
        }
    }

    // A click on the scrollbar track is not also a text click.
    let track = Rectangle::new(
        rect.x + rect.width - SCROLLBAR_WIDTH + 1.0,
        rect.y + 1.0,
        SCROLLBAR_WIDTH - 2.0,
        rect.height - 2.0,
    );
    let scrollbar_hit = mouse
        .click
        .is_some_and(|c| track.check_collision_point_rec(c));

    // Char position under a point; points above or below the text clamp to
    // its ends.
    let pos_at_point = |state: &TextEditorState, p: Vector2| {
        let row = ((p.y - rect.y - PADDING + state.scroll_y) / LINE_HEIGHT).floor();
        if row < 0.0 {
            return 0;
        }
        match state.rows.get(row as usize) {
            Some(r) => {
                let col = column_at(
                    font,
                    row_text(&state.text, r),
                    p.x - text_x + state.scroll_x,
                );
                pos_in_row(r, col)
            }
            None => state.char_count(),
        }
    };

    if let Some(click) = mouse.click
        && !scrollbar_hit
    {
        let inside = rect.check_collision_point_rec(click);
        if inside {
            let pos = pos_at_point(state, click);
            let extend = mouse.shift && state.focused;
            state.move_to(pos, extend);
            state.mouse_selecting = true;
        }
        state.focused = inside;
        state.reset_blink();
    }
    if state.mouse_selecting {
        if mouse.down {
            // Dragging past an edge scrolls toward it.
            if mouse.pos.y < rect.y {
                state.scroll_y -= LINE_HEIGHT / 2.0;
            } else if mouse.pos.y > rect.y + rect.height {
                state.scroll_y += LINE_HEIGHT / 2.0;
            }
            if mouse.pos.x < rect.x {
                state.scroll_x -= FONT_SIZE;
            } else if mouse.pos.x > rect.x + rect.width {
                state.scroll_x += FONT_SIZE;
            }
            state.scroll_y = state.scroll_y.clamp(0.0, max_scroll);
            state.scroll_x = state.scroll_x.clamp(0.0, max_scroll_x);
            let pos = pos_at_point(state, mouse.pos);
            if pos != state.cursor {
                state.move_to(pos, true);
                state.reset_blink();
            }
        } else {
            state.mouse_selecting = false;
        }
    }

    let (cursor_row, cursor_col) = state.cursor_row_col();
    if std::mem::take(&mut state.follow_cursor) && state.focused && !state.mouse_selecting {
        // Bring the cursor into view after a key moved it.
        let top = PADDING + cursor_row as f32 * LINE_HEIGHT;
        if top - state.scroll_y < 0.0 {
            state.scroll_y = top;
        } else if top + LINE_HEIGHT - state.scroll_y > rect.height {
            state.scroll_y = top + LINE_HEIGHT - rect.height;
        }
        if !state.wrap {
            let row = &state.rows[cursor_row];
            let cx = prefix_width(font, row_text(&state.text, row), cursor_col);
            if cx < state.scroll_x {
                state.scroll_x = cx;
            } else if cx > state.scroll_x + text_width {
                state.scroll_x = cx - text_width;
            }
        }
    }
    state.scroll_y = state.scroll_y.clamp(0.0, max_scroll);
    state.scroll_x = state.scroll_x.clamp(0.0, max_scroll_x);

    {
        let to_screen = |p: Vector2| (p - camera.target) * camera.zoom + camera.offset;
        let tl = to_screen(Vector2::new(rect.x, rect.y));
        let br = to_screen(Vector2::new(rect.x + rect.width, rect.y + rect.height));
        let mut d = d.begin_scissor_mode(
            tl.x as i32,
            tl.y as i32,
            (br.x - tl.x) as i32,
            (br.y - tl.y) as i32,
        );

        let x = text_x - state.scroll_x;
        let sel = state.selection();
        let mut line_spans = Vec::new();
        let mut spans_for_line = None;
        let mut row_spans = Vec::new();
        for (i, row) in state.rows.iter().enumerate() {
            let y = rect.y + PADDING + i as f32 * LINE_HEIGHT - state.scroll_y;
            if y + LINE_HEIGHT < rect.y || y > rect.y + rect.height {
                continue;
            }
            let text = row_text(&state.text, row);

            // Selection background, plus a sliver when the line break is selected.
            if !sel.is_empty() && sel.start <= row.end && sel.end > row.start {
                let from = sel.start.max(row.start) - row.start;
                let to = sel.end.min(row.end) - row.start;
                let x0 = x + prefix_width(font, text, from);
                let mut x1 = x + prefix_width(font, text, to);
                if sel.end > row.end && !row.wraps {
                    x1 += FONT_SIZE / 3.0;
                }
                d.draw_rectangle_rec(Rectangle::new(x0, y, x1 - x0, LINE_HEIGHT), selection_color);
            }

            // Highlight whole lines (once each), then cut the spans to this row.
            row_spans.clear();
            if let Some(h) = highlighter {
                if spans_for_line != Some(row.line) {
                    line_spans.clear();
                    let line = state.text.split('\n').nth(row.line).unwrap_or("");
                    h.highlight_line(row.line, line, &mut line_spans);
                    spans_for_line = Some(row.line);
                }
                let line = state.text.split('\n').nth(row.line).unwrap_or("");
                let row_bytes =
                    byte_range(line, row.start - row.line_start, row.end - row.line_start);
                for span in &line_spans {
                    let start = span.range.start.max(row_bytes.start);
                    let end = span.range.end.min(row_bytes.end);
                    if start < end {
                        row_spans.push(Span {
                            range: start - row_bytes.start..end - row_bytes.start,
                            color: span.color,
                        });
                    }
                }
            }
            draw_highlighted_line(
                &mut d,
                font,
                text,
                &row_spans,
                Vector2::new(x, y),
                text_color,
                alpha,
            );

            if state.focused && state.cursor_visible && i == cursor_row {
                let cx = x + prefix_width(font, text, cursor_col);
                d.draw_line_v(
                    Vector2::new(cx, y),
                    Vector2::new(cx, y + LINE_HEIGHT),
                    text_color,
                );
            }
        }
    }

    // Drawn after the clipped text so it stays on top.
    draw_scrollbar(
        d,
        track,
        content_height,
        rect.height,
        &mut state.scroll_y,
        &mut state.scrollbar_dragging,
        &mut state.scrollbar_drag_start_y,
        &mut state.scrollbar_drag_start_scroll,
        mouse.pos,
        mouse.click,
        mouse.down,
        alpha,
    );

    wheel_consumed
}

/// Byte range of chars `from..to` in `line`.
fn byte_range(line: &str, from: usize, to: usize) -> Range<usize> {
    let byte = |col: usize| line.char_indices().nth(col).map_or(line.len(), |(b, _)| b);
    byte(from)..byte(to)
}

/// The part of the text shown on `row`.
fn row_text<'a>(text: &'a str, row: &Row) -> &'a str {
    let line = text.split('\n').nth(row.line).unwrap_or("");
    &line[byte_range(line, row.start - row.line_start, row.end - row.line_start)]
}

/// Draw `line` piece by piece: spans in their colors, gaps in `default`.
fn draw_highlighted_line(
    d: &mut impl RaylibDraw,
    font: &impl RaylibFont,
    line: &str,
    spans: &[Span],
    origin: Vector2,
    default: Color,
    alpha: f32,
) {
    let mut pieces: Vec<(&str, Color)> = Vec::with_capacity(spans.len() * 2 + 1);
    let mut pos = 0;
    for span in spans {
        let start = span.range.start.clamp(pos, line.len());
        let end = span.range.end.clamp(start, line.len());
        if !line.is_char_boundary(start) || !line.is_char_boundary(end) {
            continue;
        }
        let c = span.color;
        pieces.push((&line[pos..start], default));
        pieces.push((
            &line[start..end],
            Color::new(c.r, c.g, c.b, (c.a as f32 * alpha) as u8),
        ));
        pos = end;
    }
    pieces.push((&line[pos..], default));

    let mut x = origin.x;
    for (text, color) in pieces {
        if text.is_empty() {
            continue;
        }
        d.draw_text_ex(
            font,
            text,
            Vector2::new(x, origin.y),
            FONT_SIZE,
            SPACING,
            color,
        );
        // measure_text leaves out the spacing that would follow the last glyph.
        x += font.measure_text(text, FONT_SIZE, SPACING).x + SPACING;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(text: &str, cursor: usize) -> TextEditorState {
        TextEditorState {
            text: text.into(),
            cursor,
            anchor: cursor,
            focused: true,
            ..Default::default()
        }
    }

    fn press(s: &mut TextEditorState, keys: TextEditorKeys) {
        s.handle_keys(&keys, 0.0);
    }

    #[test]
    fn edits_at_cursor_with_multibyte_text() {
        let mut s = state("あいう", 1);
        press(
            &mut s,
            TextEditorKeys {
                typed: vec!['x'],
                ..Default::default()
            },
        );
        assert_eq!(s.text(), "あxいう");
        press(
            &mut s,
            TextEditorKeys {
                backspace: true,
                ..Default::default()
            },
        );
        press(
            &mut s,
            TextEditorKeys {
                backspace: true,
                ..Default::default()
            },
        );
        assert_eq!(s.text(), "いう");
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn vertical_moves_keep_column_and_clamp() {
        let mut s = state("abcd\nx\nefgh", 3);
        press(
            &mut s,
            TextEditorKeys {
                down: true,
                ..Default::default()
            },
        );
        assert_eq!(s.cursor_row_col(), (1, 1));
        press(
            &mut s,
            TextEditorKeys {
                down: true,
                ..Default::default()
            },
        );
        assert_eq!(s.cursor_row_col(), (2, 1));
        press(
            &mut s,
            TextEditorKeys {
                down: true,
                ..Default::default()
            },
        );
        assert_eq!(s.cursor, s.char_count());
        press(
            &mut s,
            TextEditorKeys {
                up: true,
                ..Default::default()
            },
        );
        press(
            &mut s,
            TextEditorKeys {
                up: true,
                ..Default::default()
            },
        );
        press(
            &mut s,
            TextEditorKeys {
                up: true,
                ..Default::default()
            },
        );
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn escape_drops_focus_only_when_focused() {
        let mut s = state("a", 1);
        let esc = || TextEditorKeys {
            escape: true,
            ..Default::default()
        };
        assert!(s.handle_keys(&esc(), 0.0).focus_dropped);
        assert!(!s.focused);
        assert!(!s.handle_keys(&esc(), 0.0).focus_dropped);
    }

    #[test]
    fn shift_moves_select_and_typing_replaces_selection() {
        let mut s = state("hello world", 0);
        for _ in 0..5 {
            press(
                &mut s,
                TextEditorKeys {
                    right: true,
                    shift: true,
                    ..Default::default()
                },
            );
        }
        assert_eq!(s.selected_text(), "hello");
        press(
            &mut s,
            TextEditorKeys {
                typed: vec!['H', 'i'],
                ..Default::default()
            },
        );
        assert_eq!(s.text(), "Hi world");
        assert!(s.selection().is_empty());
    }

    #[test]
    fn arrow_without_shift_collapses_to_selection_edge() {
        let mut s = state("abcdef", 4);
        s.anchor = 1;
        press(
            &mut s,
            TextEditorKeys {
                left: true,
                ..Default::default()
            },
        );
        assert_eq!((s.cursor, s.anchor), (1, 1));
    }

    #[test]
    fn cut_copy_paste_and_select_all() {
        let mut s = state(
            "ab
cd", 0,
        );
        let out = s.handle_keys(
            &TextEditorKeys {
                select_all: true,
                copy: true,
                ..Default::default()
            },
            0.0,
        );
        assert_eq!(
            out.copied.as_deref(),
            Some(
                "ab
cd"
            )
        );
        s.anchor = 0;
        s.cursor = 2;
        let out = s.handle_keys(
            &TextEditorKeys {
                cut: true,
                ..Default::default()
            },
            0.0,
        );
        assert_eq!(out.copied.as_deref(), Some("ab"));
        assert_eq!(
            s.text(),
            "
cd"
        );
        press(
            &mut s,
            TextEditorKeys {
                paste: Some(
                    "x
y"
                    .into(),
                ),
                ..Default::default()
            },
        );
        assert_eq!(
            s.text(),
            "x
y
cd"
        );
    }

    #[test]
    fn backspace_and_delete_remove_selection_or_one_char() {
        let mut s = state("abcd", 2);
        press(
            &mut s,
            TextEditorKeys {
                delete: true,
                ..Default::default()
            },
        );
        assert_eq!(s.text(), "abd");
        s.anchor = 0;
        press(
            &mut s,
            TextEditorKeys {
                backspace: true,
                ..Default::default()
            },
        );
        assert_eq!(s.text(), "d");
        press(
            &mut s,
            TextEditorKeys {
                end: true,
                ..Default::default()
            },
        );
        press(
            &mut s,
            TextEditorKeys {
                backspace: true,
                ..Default::default()
            },
        );
        assert_eq!(s.text(), "");
    }

    /// "hello world" wrapped after the space, as a narrow editor would.
    fn wrapped_hello() -> TextEditorState {
        let mut s = state("hello world", 0);
        s.rows = vec![
            Row {
                start: 0,
                end: 6,
                line: 0,
                line_start: 0,
                wraps: true,
            },
            Row {
                start: 6,
                end: 11,
                line: 0,
                line_start: 0,
                wraps: false,
            },
        ];
        s.rows_valid = true;
        s
    }

    #[test]
    fn wrap_point_belongs_to_next_row() {
        let s = wrapped_hello();
        assert_eq!(row_of(&s.rows, 5), 0);
        assert_eq!(row_of(&s.rows, 6), 1);
        assert_eq!(row_of(&s.rows, 11), 1);
        assert_eq!(row_text(s.text(), &s.rows[0]), "hello ");
        assert_eq!(row_text(s.text(), &s.rows[1]), "world");
    }

    #[test]
    fn vertical_and_home_end_follow_wrapped_rows() {
        let mut s = wrapped_hello();
        s.cursor = 8;
        s.anchor = 8;
        press(
            &mut s,
            TextEditorKeys {
                up: true,
                ..Default::default()
            },
        );
        assert_eq!(s.cursor, 2);
        press(
            &mut s,
            TextEditorKeys {
                end: true,
                ..Default::default()
            },
        );
        // Stays on the first row instead of jumping to the wrap point.
        assert_eq!(s.cursor, 5);
        press(
            &mut s,
            TextEditorKeys {
                down: true,
                ..Default::default()
            },
        );
        assert_eq!(s.cursor, 11);
        press(
            &mut s,
            TextEditorKeys {
                home: true,
                ..Default::default()
            },
        );
        assert_eq!(s.cursor, 6);
    }

    #[test]
    fn unfocused_ignores_keys() {
        let mut s = state("a", 1);
        s.focused = false;
        press(
            &mut s,
            TextEditorKeys {
                typed: vec!['b'],
                ..Default::default()
            },
        );
        assert_eq!(s.text(), "a");
    }
}
