//! Byte offsets to editor positions and back.
//!
//! `Span` carries a `line` and `col` already, but neither is usable here: `col`
//! counts *bytes* and is only recorded for a span's start, so a span cannot be
//! turned into a range and any line holding a non-ASCII character reports the
//! wrong column. The byte offsets in `Span.start`/`Span.end` are always right,
//! so everything is derived from those instead.
//!
//! Positions are 0-based and count UTF-16 code units, which is what the Language
//! Server Protocol means by a character. Identifiers are ASCII, but string
//! literals and comments are not — `"héllo→"` appears in the lexer's own tests —
//! so the conversion has to be real rather than an offset subtraction.

/// A 0-based position, with `character` counted in UTF-16 code units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

impl Position {
    pub fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

/// The line structure of one source file.
pub struct LineIndex<'src> {
    src: &'src str,
    /// Byte offset of the first character of each line. Always starts with 0,
    /// so it is never empty and `line_starts[n]` is line `n`.
    line_starts: Vec<usize>,
}

impl<'src> LineIndex<'src> {
    pub fn new(src: &'src str) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(
            src.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i + 1),
        );
        Self { src, line_starts }
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// The position of a byte offset. Offsets past the end clamp to the end, and
    /// an offset landing inside a multi-byte character snaps back to its start,
    /// so this never panics on a stale or truncated span.
    pub fn position(&self, offset: usize) -> Position {
        let offset = self.floor_char_boundary(offset.min(self.src.len()));
        // The last line whose start is at or before the offset.
        let line = self.line_starts.partition_point(|start| *start <= offset) - 1;
        let character: usize =
            self.src[self.line_starts[line]..offset].chars().map(char::len_utf16).sum();
        Position { line: line as u32, character: character as u32 }
    }

    /// The byte offset of a position. Out-of-range lines and characters clamp to
    /// the end of the line, and then of the file — an editor can legitimately
    /// ask about a position that no longer exists.
    pub fn offset(&self, position: Position) -> usize {
        let Some(&line_start) = self.line_starts.get(position.line as usize) else {
            return self.src.len();
        };
        let line_end = self
            .line_starts
            .get(position.line as usize + 1)
            .map(|next| self.line_end_excluding_newline(line_start, *next))
            .unwrap_or(self.src.len());

        let mut remaining = position.character as usize;
        let mut offset = line_start;
        for ch in self.src[line_start..line_end].chars() {
            if remaining == 0 {
                return offset;
            }
            let units = ch.len_utf16();
            if remaining < units {
                // Mid-surrogate-pair. Snap to the character's start rather than
                // splitting it.
                return offset;
            }
            remaining -= units;
            offset += ch.len_utf8();
        }
        offset
    }

    /// Trims the line terminator, so a position past the last visible character
    /// does not land after a `\n` (or inside a `\r\n`).
    fn line_end_excluding_newline(&self, start: usize, next_line_start: usize) -> usize {
        let mut end = next_line_start.saturating_sub(1); // the '\n'
        if end > start && self.src.as_bytes().get(end - 1) == Some(&b'\r') {
            end -= 1;
        }
        end.max(start)
    }

    fn floor_char_boundary(&self, offset: usize) -> usize {
        let mut offset = offset;
        while offset > 0 && !self.src.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_are_zero_based() {
        let index = LineIndex::new("abc\ndef\n");
        assert_eq!(index.position(0), Position::new(0, 0));
        assert_eq!(index.position(2), Position::new(0, 2));
        assert_eq!(index.position(4), Position::new(1, 0));
        assert_eq!(index.position(6), Position::new(1, 2));
    }

    #[test]
    fn a_newline_belongs_to_the_line_it_ends() {
        let index = LineIndex::new("ab\ncd");
        assert_eq!(index.position(2), Position::new(0, 2), "the \\n itself");
        assert_eq!(index.position(3), Position::new(1, 0), "the byte after it");
    }

    #[test]
    fn empty_source_has_one_line() {
        let index = LineIndex::new("");
        assert_eq!(index.line_count(), 1);
        assert_eq!(index.position(0), Position::new(0, 0));
    }

    #[test]
    fn offsets_past_the_end_clamp() {
        let index = LineIndex::new("ab");
        assert_eq!(index.position(999), Position::new(0, 2));
    }

    #[test]
    fn multibyte_characters_count_as_utf16_units() {
        // é is 2 bytes / 1 UTF-16 unit; → is 3 bytes / 1 UTF-16 unit.
        let src = "\"héllo→\"";
        let index = LineIndex::new(src);
        assert_eq!(index.position(src.len()), Position::new(0, 8), "8 characters, 11 bytes");
        assert_eq!(src.len(), 11);
    }

    #[test]
    fn astral_characters_are_two_utf16_units() {
        // A character outside the BMP is a surrogate pair: 4 bytes, 2 units.
        let src = "a😀b";
        let index = LineIndex::new(src);
        assert_eq!(index.position(1), Position::new(0, 1));
        assert_eq!(index.position(5), Position::new(0, 3), "after the pair");
        assert_eq!(index.position(6), Position::new(0, 4));
    }

    #[test]
    fn an_offset_inside_a_character_snaps_back() {
        let index = LineIndex::new("a😀b");
        // Bytes 2..4 are interior to the emoji.
        assert_eq!(index.position(3), index.position(1), "must not panic or split");
    }

    #[test]
    fn offset_round_trips_with_position() {
        let src = "fn a(): int {\n  let x = \"héllo→\"\n  1\n}\n";
        let index = LineIndex::new(src);
        for offset in 0..=src.len() {
            if !src.is_char_boundary(offset) {
                continue;
            }
            let position = index.position(offset);
            assert_eq!(index.offset(position), offset, "round trip failed at byte {offset}");
        }
    }

    #[test]
    fn a_character_past_the_line_end_clamps_to_it() {
        let index = LineIndex::new("ab\ncdef\n");
        assert_eq!(index.offset(Position::new(0, 99)), 2, "end of line 0, before the \\n");
        assert_eq!(index.offset(Position::new(1, 99)), 7, "end of line 1");
    }

    #[test]
    fn a_line_past_the_end_clamps_to_the_file_end() {
        let src = "ab\n";
        let index = LineIndex::new(src);
        assert_eq!(index.offset(Position::new(99, 0)), src.len());
    }

    #[test]
    fn carriage_returns_are_not_part_of_the_visible_line() {
        let src = "ab\r\ncd\r\n";
        let index = LineIndex::new(src);
        assert_eq!(index.offset(Position::new(0, 99)), 2, "before the \\r\\n");
        assert_eq!(index.position(5), Position::new(1, 1));
    }
}
