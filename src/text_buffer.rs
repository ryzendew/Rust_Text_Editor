use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use xi_unicode::LineBreakIterator;

#[derive(Debug, Clone)]
pub struct TextBuffer {
    content: String,
    line_breaks: Vec<usize>,
    cursor_position: usize,
    selection: Option<Range<usize>>,
    preferred_column: Option<usize>,  // For maintaining cursor column during vertical movement
}

impl TextBuffer {
    pub fn new() -> Self {
        Self {
            content: String::new(),
            line_breaks: vec![0],
            cursor_position: 0,
            selection: None,
            preferred_column: None,
        }
    }

    pub fn from_str(text: &str) -> Self {
        let mut buffer = Self::new();
        buffer.set_text(text);
        buffer
    }

    pub fn set_text(&mut self, text: &str) {
        self.content = text.to_string();
        self.update_line_breaks();
        self.cursor_position = 0;
        self.selection = None;
        self.preferred_column = None;
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn insert(&mut self, text: &str) {
        if let Some(range) = self.selection.take() {
            self.delete_range(range);
        }
        self.content.insert_str(self.cursor_position, text);
        self.cursor_position += text.len();
        self.update_line_breaks();
        self.preferred_column = None;
    }

    pub fn delete_backward(&mut self) {
        if let Some(range) = self.selection.take() {
            self.delete_range(range);
        } else if self.cursor_position > 0 {
            let prev_char_boundary = self.content
                .grapheme_indices(true)
                .take_while(|(i, _)| *i < self.cursor_position)
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.delete_range(prev_char_boundary..self.cursor_position);
            self.cursor_position = prev_char_boundary;
        }
        self.preferred_column = None;
    }

    pub fn delete_forward(&mut self) {
        if let Some(range) = self.selection.take() {
            self.delete_range(range);
        } else if self.cursor_position < self.content.len() {
            let next_char_boundary = self.content
                .grapheme_indices(true)
                .find(|(i, _)| *i > self.cursor_position)
                .map(|(i, _)| i)
                .unwrap_or(self.content.len());
            self.delete_range(self.cursor_position..next_char_boundary);
        }
        self.preferred_column = None;
    }

    pub fn move_cursor(&mut self, offset: isize, extend_selection: bool) {
        let new_position = if offset < 0 {
            self.cursor_position.saturating_sub(offset.unsigned_abs())
        } else {
            self.cursor_position.saturating_add(offset as usize)
        }.min(self.content.len());

        if extend_selection {
            let current_selection = self.selection.clone();
            self.selection = Some(match current_selection {
                Some(range) if range.start == self.cursor_position => new_position..range.end,
                Some(range) => range.start..new_position,
                None => self.cursor_position..new_position,
            });
        } else {
            self.selection = None;
        }
        self.cursor_position = new_position;
        self.preferred_column = None;
    }

    pub fn move_cursor_vertically(&mut self, lines: isize, extend_selection: bool) {
        let current_line = self.line_at_offset(self.cursor_position);
        let target_line = (current_line as isize + lines).max(0) as usize;
        
        // Get or calculate preferred column
        let preferred_column = self.preferred_column.unwrap_or_else(|| {
            self.column_at_offset(self.cursor_position)
        });
        self.preferred_column = Some(preferred_column);

        // Find target position
        let new_position = if let Some(line_range) = self.line_range(target_line) {
            let line_text = &self.content[line_range.clone()];
            let mut column = 0;
            let mut target_pos = line_range.start;

            for (idx, _) in line_text.grapheme_indices(true) {
                if column >= preferred_column {
                    break;
                }
                target_pos = line_range.start + idx;
                column += 1;
            }
            target_pos
        } else {
            if lines < 0 {
                0
            } else {
                self.content.len()
            }
        };

        // Update selection if needed
        if extend_selection {
            let current_selection = self.selection.clone();
            self.selection = Some(match current_selection {
                Some(range) if range.start == self.cursor_position => new_position..range.end,
                Some(range) => range.start..new_position,
                None => self.cursor_position..new_position,
            });
        } else {
            self.selection = None;
        }
        self.cursor_position = new_position;
    }

    fn delete_range(&mut self, range: Range<usize>) {
        self.content.drain(range.clone());
        self.update_line_breaks();
    }

    fn update_line_breaks(&mut self) {
        self.line_breaks = vec![0];
        let mut iter = LineBreakIterator::new(&self.content);
        while let Some((idx, _)) = iter.next() {
            if idx > 0 {
                self.line_breaks.push(idx);
            }
        }
        if !self.content.is_empty() && *self.line_breaks.last().unwrap() != self.content.len() {
            self.line_breaks.push(self.content.len());
        }
    }

    pub fn cursor_position(&self) -> usize {
        self.cursor_position
    }

    pub fn selection(&self) -> Option<Range<usize>> {
        self.selection.clone()
    }

    pub fn line_count(&self) -> usize {
        self.line_breaks.len()
    }

    pub fn line_range(&self, line_index: usize) -> Option<Range<usize>> {
        if line_index >= self.line_breaks.len() {
            return None;
        }
        let start = self.line_breaks[line_index];
        let end = self.line_breaks.get(line_index + 1).copied().unwrap_or(self.content.len());
        Some(start..end)
    }

    pub fn line_at_offset(&self, offset: usize) -> usize {
        match self.line_breaks.binary_search(&offset) {
            Ok(idx) => idx,
            Err(idx) => idx.saturating_sub(1),
        }
    }

    pub fn column_at_offset(&self, offset: usize) -> usize {
        let line_start = self.line_breaks[self.line_at_offset(offset)];
        self.content[line_start..offset].graphemes(true).count()
    }

    pub fn get_word_boundary_at_offset(&self, offset: usize) -> Range<usize> {
        let mut start = offset;
        let mut end = offset;

        // Find word start
        for (idx, _) in self.content[..offset].grapheme_indices(true).rev() {
            if !self.is_word_char(self.content[idx..].chars().next().unwrap()) {
                break;
            }
            start = idx;
        }

        // Find word end
        for (idx, _) in self.content[offset..].grapheme_indices(true) {
            let abs_idx = offset + idx;
            if !self.is_word_char(self.content[abs_idx..].chars().next().unwrap()) {
                break;
            }
            end = abs_idx + 1;
        }

        start..end
    }

    fn is_word_char(&self, c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    pub fn set_selection(&mut self, range: Option<Range<usize>>) {
        self.selection = range;
    }

    pub fn get_selection(&self) -> Option<Range<usize>> {
        self.selection.clone()
    }
} 