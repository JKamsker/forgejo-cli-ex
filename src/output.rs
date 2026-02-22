use std::io::IsTerminal;

pub fn should_print_header(force_header: bool, no_header: bool) -> bool {
    if no_header {
        return false;
    }
    if force_header {
        return true;
    }
    std::io::stdout().is_terminal()
}

pub fn print_table(headers: &[&str], rows: &[Vec<String>], show_header: bool) {
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
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    if show_header && !headers.is_empty() {
        println!("{}", format_row(headers.iter().copied(), &widths));
    }

    for row in rows {
        let cells = (0..col_count).map(|i| row.get(i).map(|s| s.as_str()).unwrap_or(""));
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
