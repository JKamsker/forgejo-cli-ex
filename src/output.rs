use std::borrow::Cow;
use std::io::IsTerminal;

/// Neutralizes control characters in server-controlled text before it is
/// printed for a human, so a crafted PR title, comment, job or artifact name
/// cannot inject terminal escape sequences.
///
/// C0 controls, DEL and C1 controls become U+FFFD, and so do the invisible
/// formatting characters of [`is_display_spoofing_char`]. With `multiline`
/// (for bodies), `\n` and `\t` are kept and `\r\n` becomes `\n`; otherwise,
/// and for a lone `\r`, tabs and line breaks become spaces. JSON output must
/// not be passed through this function.
pub fn sanitize_terminal_text(text: &str, multiline: bool) -> Cow<'_, str> {
    if !text
        .chars()
        .any(|ch| ch.is_control() || is_display_spoofing_char(ch))
    {
        return Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\n' | '\t' if multiline => out.push(ch),
            '\r' if multiline && chars.peek() == Some(&'\n') => {}
            '\n' | '\t' | '\r' => out.push(' '),
            ch if ch.is_control() || is_display_spoofing_char(ch) => out.push('\u{FFFD}'),
            ch => out.push(ch),
        }
    }
    Cow::Owned(out)
}

/// Invisible characters that reorder or disguise the text around them on a
/// bidi-aware terminal: bidi embeddings, overrides and isolates (which carry
/// to the end of the line), the zero-width space and the byte-order mark.
/// Directional marks and zero-width (non-)joiners only affect neighbouring
/// characters and are common in RTL scripts and emoji, so they are kept.
fn is_display_spoofing_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{200B}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// [`sanitize_terminal_text`] for single-line values such as names and titles.
pub fn sanitize_line(text: &str) -> Cow<'_, str> {
    sanitize_terminal_text(text, false)
}

pub fn should_print_header(force_header: bool, no_header: bool) -> bool {
    if no_header {
        return false;
    }
    if force_header {
        return true;
    }
    std::io::stdout().is_terminal()
}

/// Prints an aligned table. Cells usually hold server-controlled values, so
/// they are sanitized with [`sanitize_line`].
pub fn print_table(headers: &[&str], rows: &[Vec<String>], show_header: bool) {
    let rows: Vec<Vec<Cow<'_, str>>> = rows
        .iter()
        .map(|row| row.iter().map(|cell| sanitize_line(cell)).collect())
        .collect();
    let col_count = std::cmp::max(
        headers.len(),
        rows.iter().map(|r| r.len()).max().unwrap_or(0),
    );
    if col_count == 0 {
        return;
    }

    let mut widths = vec![0usize; col_count];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = widths[i].max(h.chars().count());
    }
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    if show_header && !headers.is_empty() {
        println!("{}", format_row(headers.iter().copied(), &widths));
    }

    for row in &rows {
        let cells = (0..col_count).map(|i| row.get(i).map(|s| s.as_ref()).unwrap_or(""));
        println!("{}", format_row(cells, &widths));
    }
}

fn format_row<'a>(cells: impl Iterator<Item = &'a str>, widths: &[usize]) -> String {
    let mut out = String::new();
    for (i, cell) in cells.enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        let width = widths.get(i).copied().unwrap_or(0);
        out.push_str(cell);
        let padding = width.saturating_sub(cell.chars().count());
        if padding > 0 {
            out.extend(std::iter::repeat_n(' ', padding));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{sanitize_line, sanitize_terminal_text};

    #[test]
    fn sanitizer_replaces_c0_c1_and_del_controls() {
        assert_eq!(
            sanitize_line("evil\u{1b}]52;c;cHduZWQ=\u{07}title"),
            "evil\u{fffd}]52;c;cHduZWQ=\u{fffd}title"
        );
        assert_eq!(
            sanitize_line("csi\u{9b}2Jdel\u{7f}"),
            "csi\u{fffd}2Jdel\u{fffd}"
        );
        assert_eq!(sanitize_line("a\tb\r\nc"), "a b  c");
    }

    #[test]
    fn sanitizer_keeps_newlines_and_tabs_in_bodies() {
        assert_eq!(
            sanitize_terminal_text("line 1\r\n\tline 2\r\u{1b}[31m", true),
            "line 1\n\tline 2 \u{fffd}[31m"
        );
    }

    #[test]
    fn sanitizer_replaces_bidi_overrides_and_invisible_marks() {
        assert_eq!(
            sanitize_line("feat\u{202E}gnp.exe\u{2066}x\u{2069}\u{200B}y\u{FEFF}"),
            "feat\u{fffd}gnp.exe\u{fffd}x\u{fffd}\u{fffd}y\u{fffd}"
        );
        assert_eq!(
            sanitize_terminal_text("a\u{202D}\nb\u{2067}", true),
            "a\u{fffd}\nb\u{fffd}"
        );
        // Joiners and marks are part of emoji sequences and RTL text.
        assert!(matches!(
            sanitize_line("\u{1F469}\u{200D}\u{1F4BB} \u{0645}\u{06CC}\u{200C}\u{062E}\u{200F}"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn sanitizer_leaves_plain_text_untouched() {
        assert!(matches!(
            sanitize_line("fix: ünïcode ✓"),
            std::borrow::Cow::Borrowed("fix: ünïcode ✓")
        ));
    }
}
