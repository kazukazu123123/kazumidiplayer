//! Multi-line plain text editor with optional syntax highlighting.
//!
//! Keep a [`TextEditorState`] somewhere, feed it keys with
//! [`TextEditorState::handle_keys`] once per frame, and draw it with
//! [`draw_text_editor`]. Colors come from an optional [`SyntaxHighlighter`].

use super::scrollbar::draw_scrollbar;
use raylib::prelude::*;
use std::ops::Range;

const FONT_SIZE: f32 = 16.0;
const SPACING: f32 = 1.0;
const LINE_HEIGHT: f32 = 20.0;
const PADDING: f32 = 8.0;
const BLINK_INTERVAL: f32 = 0.5;

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
    pub enter: bool,
    pub left: bool,
    pub right: bool,
    pub up: bool,
    pub down: bool,
    pub escape: bool,
}

impl TextEditorKeys {
    /// Read this frame's keys. Consumes raylib's typed-character queue.
    pub fn poll(rl: &mut RaylibHandle) -> Self {
        let mut typed = Vec::new();
        while let Some(ch) = rl.get_char_pressed() {
            typed.push(ch);
        }
        Self {
            typed,
            backspace: rl.is_key_pressed(KeyboardKey::KEY_BACKSPACE),
            enter: rl.is_key_pressed(KeyboardKey::KEY_ENTER),
            left: rl.is_key_pressed(KeyboardKey::KEY_LEFT),
            right: rl.is_key_pressed(KeyboardKey::KEY_RIGHT),
            up: rl.is_key_pressed(KeyboardKey::KEY_UP),
            down: rl.is_key_pressed(KeyboardKey::KEY_DOWN),
            escape: rl.is_key_pressed(KeyboardKey::KEY_ESCAPE),
        }
    }
}

/// Mouse input for one frame, in the same coordinates as the editor's rect.
pub struct TextEditorMouse {
    pub pos: Vector2,
    pub click: Option<Vector2>,
    pub down: bool,
    pub wheel: f32,
}

pub struct TextEditorState {
    text: String,
    /// In chars, `0..=text.chars().count()`.
    cursor: usize,
    pub focused: bool,
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
            cursor: 0,
            focused: false,
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
        self.cursor = self.cursor.min(self.char_count());
    }

    /// Unfocus, put the cursor at the end and restart blinking, e.g. when the
    /// editor is shown again.
    pub fn reset_interaction(&mut self) {
        self.focused = false;
        self.cursor = self.char_count();
        self.reset_blink();
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

    /// Row and column (in chars) of the cursor.
    fn cursor_row_col(&self) -> (usize, usize) {
        let mut start = 0;
        for (row, line) in self.text.split('\n').enumerate() {
            let len = line.chars().count();
            if self.cursor <= start + len {
                return (row, self.cursor - start);
            }
            start += len + 1;
        }
        (0, 0)
    }

    /// Char position of `col` on `row`, clamped to that line.
    fn pos_at(&self, row: usize, col: usize) -> usize {
        let mut start = 0;
        for (i, line) in self.text.split('\n').enumerate() {
            let len = line.chars().count();
            if i == row {
                return start + col.min(len);
            }
            start += len + 1;
        }
        self.char_count()
    }

    /// Apply this frame's keys. Ignored (and blinking paused) while unfocused.
    /// Escape only drops focus; returns true when it did, so the caller can
    /// leave a second Escape for closing whatever contains the editor.
    pub fn handle_keys(&mut self, keys: &TextEditorKeys, dt: f32) -> bool {
        if !self.focused {
            self.reset_blink();
            return false;
        }
        if keys.escape {
            self.focused = false;
            self.reset_blink();
            return true;
        }
        for &ch in &keys.typed {
            self.insert(ch);
        }
        if keys.backspace && self.cursor > 0 {
            let start = self.byte_at(self.cursor - 1);
            let end = self.byte_at(self.cursor);
            self.text.drain(start..end);
            self.cursor -= 1;
        }
        if keys.enter {
            self.insert('\n');
        }
        if keys.left {
            self.cursor = self.cursor.saturating_sub(1);
        }
        if keys.right {
            self.cursor = (self.cursor + 1).min(self.char_count());
        }
        if keys.up {
            let (row, col) = self.cursor_row_col();
            self.cursor = if row == 0 {
                0
            } else {
                self.pos_at(row - 1, col)
            };
        }
        if keys.down {
            let (row, col) = self.cursor_row_col();
            let last_row = self.text.split('\n').count() - 1;
            self.cursor = if row >= last_row {
                self.char_count()
            } else {
                self.pos_at(row + 1, col)
            };
        }

        self.blink_timer += dt;
        if self.blink_timer >= BLINK_INTERVAL {
            self.blink_timer -= BLINK_INTERVAL;
            self.cursor_visible = !self.cursor_visible;
        }
        false
    }

    fn insert(&mut self, ch: char) {
        let at = self.byte_at(self.cursor);
        self.text.insert(at, ch);
        self.cursor += 1;
    }
}

/// Draw the editor in `rect` and handle mouse focus, scrolling and the
/// scrollbar. `camera` maps `rect`'s coordinates to the screen (rotation is
/// not supported) and is used to clip the text. Returns true when the mouse
/// wheel was consumed.
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

    let line_count = state.text.split('\n').count();
    let content_height = line_count as f32 * LINE_HEIGHT + PADDING * 2.0;
    let max_scroll = f32::max(content_height - rect.height, 0.0);

    d.draw_rectangle_rec(rect, Color::new(30, 30, 30, a(255.0)));
    let border = if state.focused {
        Color::new(0, 170, 255, a(255.0))
    } else {
        Color::new(80, 80, 80, a(255.0))
    };
    d.draw_rectangle_lines_ex(rect, 1.0, border);

    let wheel_consumed = mouse.wheel != 0.0 && rect.check_collision_point_rec(mouse.pos);
    if wheel_consumed {
        state.scroll_y -= mouse.wheel * 30.0;
    }

    let (cursor_row, cursor_col) = state.cursor_row_col();
    if state.focused {
        // Keep the cursor line inside the view.
        let top = PADDING + cursor_row as f32 * LINE_HEIGHT;
        if top - state.scroll_y < 0.0 {
            state.scroll_y = top;
        } else if top + LINE_HEIGHT - state.scroll_y > rect.height {
            state.scroll_y = top + LINE_HEIGHT - rect.height;
        }
    }
    state.scroll_y = state.scroll_y.clamp(0.0, max_scroll);

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

        let mut spans = Vec::new();
        for (i, line) in state.text.split('\n').enumerate() {
            let y = rect.y + PADDING + i as f32 * LINE_HEIGHT - state.scroll_y;
            if y + LINE_HEIGHT < rect.y || y > rect.y + rect.height {
                continue;
            }
            let x = rect.x + PADDING;
            spans.clear();
            if let Some(h) = highlighter {
                h.highlight_line(i, line, &mut spans);
            }
            draw_highlighted_line(
                &mut d,
                font,
                line,
                &spans,
                Vector2::new(x, y),
                text_color,
                alpha,
            );

            if state.focused && state.cursor_visible && i == cursor_row {
                let prefix_end = line
                    .char_indices()
                    .nth(cursor_col)
                    .map_or(line.len(), |(b, _)| b);
                let cx = x + font.measure_text(&line[..prefix_end], FONT_SIZE, SPACING).x;
                d.draw_line_v(
                    Vector2::new(cx, y),
                    Vector2::new(cx, y + LINE_HEIGHT),
                    text_color,
                );
            }
        }
    }

    // Drawn after the clipped text so it stays on top.
    let track = Rectangle::new(
        rect.x + rect.width - 9.0,
        rect.y + 1.0,
        8.0,
        rect.height - 2.0,
    );
    let scrollbar_used = draw_scrollbar(
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

    if let Some(click) = mouse.click
        && !scrollbar_used
    {
        state.focused = rect.check_collision_point_rec(click);
        state.reset_blink();
    }

    wheel_consumed
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
        assert!(s.handle_keys(&esc(), 0.0));
        assert!(!s.focused);
        assert!(!s.handle_keys(&esc(), 0.0));
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
