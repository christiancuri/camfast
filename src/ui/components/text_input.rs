//! Single-line text input with IME support (via `EntityInputHandler`), cursor, selection,
//! clipboard, optional masking (passwords) and a digits-only filter.
//!
//! Adapted from gpui's `examples/input.rs`; cursor movement uses `char` boundaries instead of
//! grapheme clusters to avoid an extra dependency.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, ContentMask, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, KeyBinding, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ShapedLine, SharedString, Style, TextRun,
    UTF16Selection, UnderlineStyle, Window, actions, div, fill, point, prelude::*, px, relative, size,
};

use crate::ui::theme;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        WordLeft,
        WordRight,
        SelectLeft,
        SelectRight,
        SelectWordLeft,
        SelectWordRight,
        SelectAll,
        Home,
        End,
        SelectToHome,
        SelectToEnd,
        DeleteWordLeft,
        DeleteWordRight,
        DeleteToStart,
        ShowCharacterPalette,
        Paste,
        Cut,
        Copy,
        Submit,
    ]
);

const KEY_CONTEXT: &str = "TextInput";
const MASK_CHAR: &str = "•";
const HEIGHT: f32 = 30.;
const PADDING_X: f32 = 8.;

pub(super) fn init(cx: &mut App) {
    let ctx = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("alt-left", WordLeft, ctx),
        KeyBinding::new("alt-right", WordRight, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("shift-alt-left", SelectWordLeft, ctx),
        KeyBinding::new("shift-alt-right", SelectWordRight, ctx),
        KeyBinding::new("cmd-a", SelectAll, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("cmd-left", Home, ctx),
        KeyBinding::new("cmd-right", End, ctx),
        KeyBinding::new("ctrl-a", Home, ctx),
        KeyBinding::new("ctrl-e", End, ctx),
        KeyBinding::new("shift-home", SelectToHome, ctx),
        KeyBinding::new("shift-end", SelectToEnd, ctx),
        KeyBinding::new("shift-cmd-left", SelectToHome, ctx),
        KeyBinding::new("shift-cmd-right", SelectToEnd, ctx),
        KeyBinding::new("alt-backspace", DeleteWordLeft, ctx),
        KeyBinding::new("alt-delete", DeleteWordRight, ctx),
        KeyBinding::new("cmd-backspace", DeleteToStart, ctx),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, ctx),
        KeyBinding::new("cmd-v", Paste, ctx),
        KeyBinding::new("cmd-c", Copy, ctx),
        KeyBinding::new("cmd-x", Cut, ctx),
        KeyBinding::new("enter", Submit, ctx),
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextInputEvent {
    /// The text changed because of user input (not `set_text`).
    Change,
    /// Enter was pressed.
    Submit,
}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    scroll_x: Pixels,
    is_selecting: bool,
    masked: bool,
    digits_only: bool,
    max_len: Option<usize>,
}

impl EventEmitter<TextInputEvent> for TextInput {}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TextInput {
    pub fn new(cx: &mut Context<Self>, placeholder: impl Into<SharedString>, tab_index: isize) -> Self {
        Self {
            focus_handle: cx.focus_handle().tab_index(tab_index).tab_stop(true),
            content: SharedString::default(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            scroll_x: px(0.),
            is_selecting: false,
            masked: false,
            digits_only: false,
            max_len: None,
        }
    }

    /// Render bullets instead of the text (passwords).
    pub fn masked(mut self, masked: bool) -> Self {
        self.masked = masked;
        self
    }

    /// Only ASCII digits can be typed or pasted.
    pub fn digits_only(mut self, digits_only: bool) -> Self {
        self.digits_only = digits_only;
        self
    }

    /// Maximum number of characters.
    pub fn max_len(mut self, max_len: usize) -> Self {
        self.max_len = Some(max_len);
        self
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn is_masked(&self) -> bool {
        self.masked
    }

    pub fn set_masked(&mut self, masked: bool, cx: &mut Context<Self>) {
        self.masked = masked;
        cx.notify();
    }

    /// Replaces the content without emitting `Change`; cursor goes to the end.
    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        let text = text.into();
        if text == self.content {
            return;
        }
        let len = text.len();
        self.content = text;
        self.selected_range = len..len;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    // --- editing --------------------------------------------------------------------------

    fn sanitize(&self, text: &str, replaced: &Range<usize>) -> String {
        let mut out: String = text
            .chars()
            .filter(|c| !matches!(c, '\n' | '\r'))
            .filter(|c| !self.digits_only || c.is_ascii_digit())
            .collect();
        if let Some(max) = self.max_len {
            let remaining_chars = self.content.chars().count().saturating_sub(self.content[replaced.clone()].chars().count());
            let room = max.saturating_sub(remaining_chars);
            if out.chars().count() > room {
                out = out.chars().take(room).collect();
            }
        }
        out
    }

    fn splice(&mut self, range: Range<usize>, new_text: &str) {
        let range = self.clamp_range(range);
        self.content = format!("{}{}{}", &self.content[..range.start], new_text, &self.content[range.end..]).into();
    }

    fn clamp_range(&self, range: Range<usize>) -> Range<usize> {
        let start = self.clamp_offset(range.start);
        let end = self.clamp_offset(range.end).max(start);
        start..end
    }

    fn clamp_offset(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.content.len());
        while !self.content.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            if self.cursor_offset() == prev {
                window.play_system_bell();
                return;
            }
            self.select_to(prev, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            if self.cursor_offset() == next {
                window.play_system_bell();
                return;
            }
            self.select_to(next, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_word_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete_word_right(&mut self, _: &DeleteWordRight, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_word_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete_to_start(&mut self, _: &DeleteToStart, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(0, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TextInputEvent::Submit);
    }

    // --- cursor / selection ------------------------------------------------------------------

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.previous_word_boundary(self.cursor_offset()), cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.next_word_boundary(self.cursor_offset()), cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_word_boundary(self.cursor_offset()), cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_word_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn select_to_home(&mut self, _: &SelectToHome, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_to_end(&mut self, _: &SelectToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    fn show_character_palette(&mut self, _: &ShowCharacterPalette, window: &mut Window, _: &mut Context<Self>) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text.replace('\n', " "), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        // Never expose a masked value through the clipboard.
        if !self.selected_range.is_empty() && !self.masked {
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[self.selected_range.clone()].to_string()));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            return;
        }
        if !self.masked {
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[self.selected_range.clone()].to_string()));
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = true;
        let index = self.index_for_mouse_position(event.position);
        if event.click_count >= 2 {
            self.is_selecting = false;
            self.move_to(0, cx);
            self.select_to(self.content.len(), cx);
        } else if event.modifiers.shift {
            self.select_to(index, cx);
        } else {
            self.move_to(index, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.clamp_offset(offset);
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.clamp_offset(offset);
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed { self.selected_range.start } else { self.selected_range.end }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref()) else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        let display_index = line.closest_index_for_x(position.x - bounds.left() + self.scroll_x);
        self.content_offset(display_index)
    }

    // --- boundaries -----------------------------------------------------------------------

    fn previous_boundary(&self, offset: usize) -> usize {
        let offset = self.clamp_offset(offset);
        self.content[..offset].char_indices().next_back().map(|(i, _)| i).unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        let offset = self.clamp_offset(offset);
        offset + self.content[offset..].chars().next().map(char::len_utf8).unwrap_or(0)
    }

    fn previous_word_boundary(&self, offset: usize) -> usize {
        let offset = self.clamp_offset(offset);
        let chars: Vec<(usize, char)> = self.content[..offset].char_indices().collect();
        let mut i = chars.len();
        while i > 0 && !chars[i - 1].1.is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && chars[i - 1].1.is_alphanumeric() {
            i -= 1;
        }
        chars.get(i).map(|(b, _)| *b).unwrap_or(offset)
    }

    fn next_word_boundary(&self, offset: usize) -> usize {
        let offset = self.clamp_offset(offset);
        let mut iter = self.content[offset..].char_indices().peekable();
        while iter.peek().is_some_and(|(_, c)| !c.is_alphanumeric()) {
            iter.next();
        }
        while iter.peek().is_some_and(|(_, c)| c.is_alphanumeric()) {
            iter.next();
        }
        iter.peek().map(|(i, _)| offset + i).unwrap_or(self.content.len())
    }

    // --- offset conversions -----------------------------------------------------------------

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.clamp_range(self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end))
    }

    /// Byte offset in the rendered string for a byte offset in `content`.
    fn display_offset(&self, offset: usize) -> usize {
        if self.masked {
            self.content[..self.clamp_offset(offset)].chars().count() * MASK_CHAR.len()
        } else {
            offset
        }
    }

    /// Byte offset in `content` for a byte offset in the rendered string.
    fn content_offset(&self, display_offset: usize) -> usize {
        if self.masked {
            let n = display_offset / MASK_CHAR.len();
            self.content.char_indices().nth(n).map(|(i, _)| i).unwrap_or(self.content.len())
        } else {
            self.clamp_offset(display_offset)
        }
    }

    fn display_text(&self) -> SharedString {
        if self.masked { MASK_CHAR.repeat(self.content.chars().count()).into() } else { self.content.clone() }
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(&mut self, _: bool, _window: &mut Window, _cx: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: self.range_to_utf16(&self.selected_range), reversed: self.selection_reversed })
    }

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(&mut self, range_utf16: Option<Range<usize>>, new_text: &str, _: &mut Window, cx: &mut Context<Self>) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let range = self.clamp_range(range);
        let new_text = self.sanitize(new_text, &range);
        let before = self.content.clone();
        self.splice(range.clone(), &new_text);
        let cursor = range.start + new_text.len();
        self.selected_range = cursor..cursor;
        self.selection_reversed = false;
        self.marked_range.take();
        cx.notify();
        if before != self.content {
            cx.emit(TextInputEvent::Change);
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let range = self.clamp_range(range);
        let new_text = self.sanitize(new_text, &range);
        let before = self.content.clone();
        self.splice(range.clone(), &new_text);
        self.marked_range = (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        // The new selection is in UTF-16 units relative to `new_text`, not to the whole content.
        let utf8_in_new_text = |utf16: usize| {
            let mut units = 0;
            for (i, c) in new_text.char_indices() {
                if units >= utf16 {
                    return i;
                }
                units += c.len_utf16();
            }
            new_text.len()
        };
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| range.start + utf8_in_new_text(r.start)..range.start + utf8_in_new_text(r.end))
            .map(|r| self.clamp_range(r))
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        cx.notify();
        if before != self.content {
            cx.emit(TextInputEvent::Change);
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let left = bounds.left() - self.scroll_x;
        Some(Bounds::from_corners(
            point(left + last_layout.x_for_index(self.display_offset(range.start)), bounds.top()),
            point(left + last_layout.x_for_index(self.display_offset(range.end)), bounds.bottom()),
        ))
    }

    fn character_index_for_point(&mut self, point: Point<Pixels>, _window: &mut Window, _cx: &mut Context<Self>) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;
        let display_index = last_layout.index_for_x(line_point.x + self.scroll_x)?;
        Some(self.offset_to_utf16(self.content_offset(display_index)))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    scroll_x: Pixels,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let style = window.text_style();
        let empty = input.content.is_empty();
        let (display_text, text_color) = if empty {
            (input.placeholder.clone(), theme::text_muted().into())
        } else {
            (input.display_text(), style.color)
        };
        let selected = input.display_offset(input.selected_range.start)..input.display_offset(input.selected_range.end);
        let cursor = input.display_offset(input.cursor_offset());
        let marked = input.marked_range.as_ref().map(|r| input.display_offset(r.start)..input.display_offset(r.end));

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = match marked.filter(|_| !empty) {
            Some(marked) => vec![
                TextRun { len: marked.start, ..run.clone() },
                TextRun {
                    len: marked.end - marked.start,
                    underline: Some(UnderlineStyle { color: Some(run.color), thickness: px(1.0), wavy: false }),
                    ..run.clone()
                },
                TextRun { len: display_text.len().saturating_sub(marked.end), ..run },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect(),
            None => vec![run],
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window.text_system().shape_line(display_text, font_size, &runs, None);

        // Keep the cursor visible when the text is wider than the field.
        let cursor_x = if empty { px(0.) } else { line.x_for_index(cursor) };
        let width = bounds.size.width;
        let mut scroll_x = input.scroll_x;
        if cursor_x - scroll_x > width - px(2.) {
            scroll_x = cursor_x - width + px(2.);
        }
        if cursor_x - scroll_x < px(0.) {
            scroll_x = cursor_x;
        }
        scroll_x = scroll_x.min((line.width() - width + px(2.)).max(px(0.)));
        let left = bounds.left() - scroll_x;

        let (selection, cursor) = if selected.is_empty() || empty {
            (
                None,
                Some(fill(
                    Bounds::new(point(left + cursor_x, bounds.top()), size(px(1.5), bounds.size.height)),
                    theme::accent(),
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(left + line.x_for_index(selected.start), bounds.top()),
                        point(left + line.x_for_index(selected.end), bounds.bottom()),
                    ),
                    theme::accent().opacity(0.35),
                )),
                None,
            )
        };
        PrepaintState { line: Some(line), cursor, selection, scroll_x }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.input.clone()), cx);
        let Some(line) = prepaint.line.take() else {
            return;
        };
        let scroll_x = prepaint.scroll_x;
        let focused = focus_handle.is_focused(window);
        let selection = prepaint.selection.take();
        let cursor = prepaint.cursor.take();
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            if let Some(selection) = selection {
                window.paint_quad(selection);
            }
            let origin = point(bounds.left() - scroll_x, bounds.top());
            if let Err(err) = line.paint(origin, window.line_height(), gpui::TextAlign::Left, None, window, cx) {
                tracing::warn!(%err, "text input paint failed");
            }
            if focused && let Some(cursor) = cursor {
                window.paint_quad(cursor);
            }
        });
        self.input.update(cx, |input, _| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
            input.scroll_x = scroll_x;
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        div()
            .flex()
            .items_center()
            .w_full()
            .h(px(HEIGHT))
            .px(px(PADDING_X))
            .rounded_md()
            .border_1()
            .border_color(if focused { theme::accent() } else { theme::border() })
            .bg(theme::background())
            .text_color(theme::text())
            .text_size(px(13.))
            .line_height(px(18.))
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::delete_to_start))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_to_home))
            .on_action(cx.listener(Self::select_to_end))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::submit))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(TextElement { input: cx.entity() })
    }
}
