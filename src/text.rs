//! Measuring and cutting text in terminal cells rather than characters.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// The columns a terminal uses to draw the text. Counted per grapheme cluster,
/// not per scalar: an emoji assembled from several scalars, a skin tone or a
/// family joined with zero width joiners, is drawn in two columns however
/// many scalars went into it.
pub fn display_width(text: &str) -> usize {
    text.graphemes(true).map(UnicodeWidthStr::width).sum()
}

/// Cuts the text down to `width` columns, never inside a grapheme cluster: half
/// of an emoji is not a character the terminal can draw. A cluster that would
/// straddle the limit is dropped, so a row can come out one column short of the
/// width rather than one past it.
pub fn truncate_to_width(text: &str, width: usize) -> String {
    let mut kept = String::new();
    let mut used = 0;
    for cluster in text.graphemes(true) {
        let cluster_width = UnicodeWidthStr::width(cluster);
        if used + cluster_width > width {
            break;
        }
        kept.push_str(cluster);
        used += cluster_width;
    }
    kept
}

/// Fills the text out to `width` columns, and leaves it alone when it already
/// runs past them.
pub fn pad_to_width(text: &str, width: usize) -> String {
    let padding = width.saturating_sub(display_width(text));
    format!("{text}{}", " ".repeat(padding))
}

/// Cuts the text to `width` columns and fills what is left with spaces, so the
/// next column starts where it does on every other row. `{:<width$}` cannot do
/// this, because it counts scalars.
pub fn fit_to_width(text: &str, width: usize) -> String {
    pad_to_width(&truncate_to_width(text, width), width)
}

/// Columns a tab advances to on the screen.
const TAB_WIDTH: usize = 8;

/// Takes out of a line of command output what would break a pane: escape
/// sequences, which colour text or move the cursor and have no width of their
/// own, and control characters. Tabs become the spaces they would have taken,
/// so that a tab-aligned table still lines up.
pub fn plain_text(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    let mut column = 0;
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => skip_escape_sequence(&mut chars),
            '\t' => {
                let spaces = TAB_WIDTH - column % TAB_WIDTH;
                out.extend(std::iter::repeat_n(' ', spaces));
                column += spaces;
            }
            c if c.is_control() => {}
            c => {
                out.push(c);
                column += display_width(c.encode_utf8(&mut [0; 4]));
            }
        }
    }
    out
}

/// Consumes the rest of an escape sequence whose ESC has just been read. CSI
/// (`ESC [`) runs to a final byte in `@`..`~`; OSC (`ESC ]`) runs to BEL or
/// `ESC \`; a sequence with intermediate bytes in ` `..`/`, such as the
/// charset designation `ESC ( B`, runs to the first byte after them; any other
/// sequence is ESC and one character.
fn skip_escape_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        Some('[') => {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    return;
                }
            }
        }
        Some(']') => {
            while let Some(c) = chars.next() {
                if c == '\u{7}' {
                    return;
                }
                if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                    chars.next();
                    return;
                }
            }
        }
        Some(c) if (' '..='/').contains(&c) => {
            while chars.next_if(|c| (' '..='/').contains(c)).is_some() {}
            chars.next();
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_wide_characters_and_clusters_in_cells() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("日本語"), 6);
        assert_eq!(display_width("👍🏽"), 2);
    }

    #[test]
    fn truncates_at_cluster_boundaries_and_pads_to_the_width() {
        assert_eq!(truncate_to_width("日本語", 5), "日本");
        assert_eq!(fit_to_width("日本語", 5), "日本 ");
        assert_eq!(fit_to_width("ab", 4), "ab  ");
        assert_eq!(pad_to_width("toolong", 3), "toolong");
    }

    #[test]
    fn strips_colour_and_cursor_sequences_and_keeps_the_text() {
        assert_eq!(
            plain_text("\u{1b}[1;32mCompiling\u{1b}[0m parun v0.1.0"),
            "Compiling parun v0.1.0"
        );
        assert_eq!(plain_text("\u{1b}[2K\u{1b}[1Gprogress 50%"), "progress 50%");
        assert_eq!(plain_text("\u{1b}]0;title\u{7}after"), "after");
        assert_eq!(plain_text("\u{1b}]8;;http://x\u{1b}\\link"), "link");
        assert_eq!(plain_text("a\u{1b}(Bb"), "ab");
    }

    #[test]
    fn expands_tabs_to_the_next_stop_and_drops_other_controls() {
        assert_eq!(plain_text("a\tb"), "a       b");
        assert_eq!(plain_text("12345678\tb"), "12345678        b");
        assert_eq!(plain_text("日本\tb"), "日本    b");
        assert_eq!(plain_text("bell\u{7}x\u{8}"), "bellx");
    }
}
