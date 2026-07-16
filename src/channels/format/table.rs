//! Render a markdown table as an aligned monospace ASCII grid, and a shared
//! inline→plain-text flattener used for table cells and plain output.

use super::ast::Inline;

/// Flatten inline runs to plain text for an `ascii_table` cell: styling and
/// link URLs are dropped, keeping only the label.
///
/// Dropping the URL is the ASCII grid's constraint, not a policy on links. A
/// cell is padded to its column's widest entry, so one `text (https://…)` at 60
/// chars widens every row in that column and pushes the block toward `split`'s
/// oversized path — where `pack_lines` cuts by width and destroys the alignment
/// that is the grid's only reason to exist. A missing URL beats an unreadable
/// grid whose URL was cut in half anyway. The lossless rendering is
/// `StdMarkdown { tables_native: true }`, which keeps the real table; every
/// ASCII target is a degraded view by construction and already drops
/// bold/italic/code the same way.
///
/// `plain.rs`'s `inline_text` is this function plus link URLs. They differ on
/// exactly the `Link` arm — that difference is the point, so they stay separate.
pub(crate) fn inline_plain(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(t) | Inline::Code(t) => out.push_str(t),
            Inline::Strong(c) | Inline::Emphasis(c) | Inline::Strikethrough(c) => {
                out.push_str(&inline_plain(c));
            }
            Inline::Link { text, .. } => out.push_str(&inline_plain(text)),
            Inline::SoftBreak | Inline::HardBreak => out.push(' '),
        }
    }
    out
}

/// Build an aligned ASCII table. Columns are padded to the widest cell; columns
/// are joined with `" | "` and the header separator with `"-+-"` so every line
/// has identical width.
///
/// The last column is padded like any other, so rows carry trailing spaces.
/// Deliberate: equal line width is the property callers check to know the grid
/// survived (see `html.rs`'s table-in-list-item guard), the padding is invisible
/// inside the `<pre>`/fence/indent every ASCII target wraps this in, and
/// right-trimming would only shrink a width `split` has already budgeted on.
pub fn ascii_table(headers: &[Vec<Inline>], rows: &[Vec<Vec<Inline>>]) -> String {
    let cols = headers
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if cols == 0 {
        return String::new();
    }

    let text_grid: Vec<Vec<String>> = std::iter::once(headers)
        .chain(rows.iter().map(Vec::as_slice))
        .map(|row| {
            (0..cols)
                .map(|c| {
                    row.get(c)
                        .map(|cell| inline_plain(cell))
                        .unwrap_or_default()
                })
                .collect()
        })
        .collect();

    let mut widths = vec![0usize; cols];
    for row in &text_grid {
        for (c, cell_text) in row.iter().enumerate() {
            widths[c] = widths[c].max(cell_text.chars().count());
        }
    }

    let fmt_row = |row: &[String]| -> String {
        (0..cols)
            .map(|c| {
                let s = row.get(c).cloned().unwrap_or_default();
                let pad = widths[c] - s.chars().count();
                let mut cell = s;
                cell.push_str(&" ".repeat(pad));
                cell
            })
            .collect::<Vec<_>>()
            .join(" | ")
    };

    // Same visual width as a row: sum(widths) + 3 * (cols - 1).
    let sep = (0..cols)
        .map(|c| "-".repeat(widths[c]))
        .collect::<Vec<_>>()
        .join("-+-");

    let mut lines = Vec::with_capacity(text_grid.len() + 1);
    lines.push(fmt_row(&text_grid[0]));
    lines.push(sep);
    for row in &text_grid[1..] {
        lines.push(fmt_row(row));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::format::ast::Inline;

    fn cell(s: &str) -> Vec<Inline> {
        vec![Inline::Text(s.to_string())]
    }

    #[test]
    fn aligns_columns_by_widest_cell() {
        let headers = vec![cell("Step"), cell("Perintah")];
        let rows = vec![
            vec![cell("1"), cell("python3 --version")],
            vec![cell("2"), cell("mkdir x")],
        ];
        let out = ascii_table(&headers, &rows);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4);
        let w = lines[0].chars().count();
        assert!(lines.iter().all(|l| l.chars().count() == w));
        assert!(lines[0].contains("Step"));
        assert!(lines[1].chars().all(|c| c == '-' || c == '+'));
    }

    #[test]
    fn single_column_table_does_not_panic() {
        let out = ascii_table(&[cell("A")], &[vec![cell("1")]]);
        assert_eq!(out, "A\n-\n1");
    }

    #[test]
    fn ragged_row_is_padded() {
        let out = ascii_table(&[cell("A"), cell("B")], &[vec![cell("1")]]);
        assert_eq!(out.lines().count(), 3);
    }

    #[test]
    fn inline_plain_flattens_formatting() {
        let inlines = vec![
            Inline::Strong(vec![Inline::Text("hi".into())]),
            Inline::Text(" there".into()),
        ];
        assert_eq!(inline_plain(&inlines), "hi there");
    }
}
