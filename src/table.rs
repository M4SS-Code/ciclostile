//! Tables to insert in place of a placeholder paragraph, and the `<w:tbl>`
//! snippet they produce.

use std::{
    fmt::{self, Display},
    num::NonZeroU64,
};

use crate::{
    length::Length,
    replace::{RunText, split_run_text},
    xml::{escape_attr, escape_text},
};

// ── Layout constants ─────────────────────────────────────────────────────────

/// Width in twips that columns share when the table has no explicit widths:
/// 17 cm, the text width of an A4 page with 2 cm margins
/// (11,906 - 2 * 1,134 twips).
const DEFAULT_WIDTH_TWIPS: u64 = 9_638;

/// Thin single borders around and between all cells, for tables without a
/// style: `sz` is in eighths of a point, so 4 is 1/2 pt. Children in schema
/// order (`CT_TblBorders`, ECMA-376 Part 1).
const BORDERS: &str = concat!(
    "<w:tblBorders>",
    "<w:top w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>",
    "<w:left w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>",
    "<w:bottom w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>",
    "<w:right w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>",
    "<w:insideH w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>",
    "<w:insideV w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>",
    "</w:tblBorders>",
);

/// Which parts of the table style apply to a table with a header row: the
/// first row and first column get their conditional formatting, columns
/// are not banded (Word's default). `w:val` is the legacy bitmask of the
/// same flags (ECMA-376 1st edition, Part 4, `tblLook`): 0x0020 first row,
/// 0x0080 first column, 0x0400 no vertical banding.
const LOOK_WITH_HEADER: &str = concat!(
    "<w:tblLook w:val=\"04A0\" w:firstRow=\"1\" w:lastRow=\"0\" w:firstColumn=\"1\"",
    " w:lastColumn=\"0\" w:noHBand=\"0\" w:noVBand=\"1\"/>",
);
/// Like [`LOOK_WITH_HEADER`], without first-row formatting.
const LOOK_WITHOUT_HEADER: &str = concat!(
    "<w:tblLook w:val=\"0480\" w:firstRow=\"0\" w:lastRow=\"0\" w:firstColumn=\"1\"",
    " w:lastColumn=\"0\" w:noHBand=\"0\" w:noVBand=\"1\"/>",
);

// ── Table ────────────────────────────────────────────────────────────────────

/// A table of plain text to insert in place of a placeholder paragraph.
///
/// Rows may have different lengths: shorter rows are padded with empty
/// cells, so the table has as many cells as its number of rows times the
/// length of its longest row. One row of 2,000 cells among 2,000 rows of
/// one cell makes 4 million cells, so bound the length of the longest row
/// when rows come from untrusted input. Word rejects rows and tables
/// without cells, so a table always has at least one column, and a table
/// with no rows gets one empty row: `Table::new` with no rows yields a
/// single empty cell.
///
/// In cell text, line breaks (`\n`, `\r\n` or `\r`) become line breaks
/// inside the cell and tabs become tab characters. Characters that XML
/// can't represent (most control characters) are dropped.
///
/// ```
/// use ciclostile::{Length, Table};
///
/// let table = Table::new([
///     ["Item", "Quantity"],
///     ["Apples", "3"],
///     ["Pears", "12"],
/// ])
/// .header(true)
/// .column_widths([Length::from_cm(8.0), Length::from_cm(3.0)]);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    /// Cell texts, row by row.
    rows: Vec<Vec<String>>,
    /// Table style id, if set; never empty.
    style: Option<String>,
    /// Column widths from the first column on; empty when not set.
    column_widths: Vec<Length>,
    /// Whether the first row is a header row.
    header: bool,
}

impl Table {
    /// Create a table from its rows of cell texts.
    ///
    /// Rows may have different lengths: shorter rows are padded with empty
    /// cells, so the table has as many cells as rows times the length of
    /// the longest row. With no rows the table is a single empty cell, and
    /// rows that are all empty get one empty cell each.
    #[must_use]
    pub fn new<R, C, S>(rows: R) -> Self
    where
        R: IntoIterator<Item = C>,
        C: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(Into::into).collect())
                .collect(),
            style: None,
            column_widths: Vec::new(),
            header: false,
        }
    }

    /// Apply a table style defined in the template, by its style id.
    ///
    /// The id is the `w:styleId` in the template's `styles.xml`, which can
    /// differ from the name Word shows: the built-in "Table Grid" style has
    /// the id `TableGrid`. A styled table gets no borders of its own, so
    /// the style decides them. If the template doesn't define the style,
    /// word processors fall back to the document's default table style,
    /// which usually has no borders. An empty id is the same as no style.
    ///
    /// Default: no style, with thin single borders around and between all
    /// cells
    #[must_use]
    pub fn style(mut self, style_id: impl Into<String>) -> Self {
        let style_id = style_id.into();
        self.style = (!style_id.is_empty()).then_some(style_id);
        self
    }

    /// Set the column widths, from the first column on.
    ///
    /// The columns then keep these widths whatever their content (fixed
    /// layout), and the table is as wide as their sum. Widths are written
    /// in twips (1/20 point), rounded to the nearest one. Widths beyond the
    /// number of columns are ignored. Columns without a width share what
    /// the given widths leave of the default 17 cm, but never get less than
    /// with no widths at all (17 cm split evenly between all columns). An
    /// empty list is the same as no widths.
    ///
    /// Default: the table spans the full text width, and its columns start
    /// out equal and adapt to their content
    #[must_use]
    pub fn column_widths(mut self, widths: impl IntoIterator<Item = Length>) -> Self {
        self.column_widths = widths.into_iter().collect();
        self
    }

    /// Mark the first row as a header row: its text is bold, and it repeats
    /// at the top of every page the table spans.
    ///
    /// Default: false
    #[must_use]
    pub fn header(mut self, header: bool) -> Self {
        self.header = header;
        self
    }

    /// Return the `<w:tbl>` element for this table.
    ///
    /// The element declares the `w` namespace itself, so it can go into
    /// any story whatever prefix the document uses, and has no whitespace
    /// between elements. Elements follow the schema order Word requires
    /// (`CT_Tbl`, `CT_TblPr`, `CT_Row`, `CT_Tc` and `CT_TcPr`, ECMA-376
    /// Part 1).
    pub(crate) fn to_xml(&self) -> String {
        TableXml(self).to_string()
    }

    /// Return the width of each of the `columns` columns, in twips (see
    /// [`column_widths`](Self::column_widths) for the rules).
    fn column_twips(&self, columns: usize) -> Vec<u64> {
        let even = split_evenly(DEFAULT_WIDTH_TWIPS, columns);
        if self.column_widths.is_empty() {
            return even;
        }

        let mut widths: Vec<u64> = self
            .column_widths
            .iter()
            .take(columns)
            .map(|width| width.as_twips())
            .collect();
        let missing = columns.saturating_sub(widths.len());
        if missing > 0 {
            let given = widths
                .iter()
                .fold(0_u64, |total, width| total.saturating_add(*width));
            let leftover = DEFAULT_WIDTH_TWIPS.saturating_sub(given);
            let even_share: u64 = even.iter().skip(widths.len()).sum();
            widths.extend(split_evenly(leftover.max(even_share), missing));
        }
        widths
    }
}

/// Split `total` into `parts` widths that differ by at most one, the wider
/// ones first.
fn split_evenly(total: u64, parts: usize) -> Vec<u64> {
    let Some(parts) = NonZeroU64::new(parts as u64) else {
        return Vec::new();
    };
    let (width, remainder) = (total / parts, total % parts);
    // `width + 1` can't overflow: a remainder needs at least two parts, so
    // `width` is at most half of `total`.
    (0..parts.get())
        .map(|index| width + u64::from(index < remainder))
        .collect()
}

// ── Snippet ──────────────────────────────────────────────────────────────────

/// The `<w:tbl>` snippet of a [`Table`], written by its `Display` impl so
/// that every part streams into one buffer.
struct TableXml<'a>(&'a Table);

impl Display for TableXml<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let table = self.0;
        let no_rows = [Vec::new()];
        let rows = if table.rows.is_empty() {
            &no_rows[..]
        } else {
            &table.rows[..]
        };
        let columns = rows.iter().map(Vec::len).max().unwrap_or_default().max(1);
        let widths = table.column_twips(columns);

        f.write_str(
            "<w:tbl xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">",
        )?;
        write_properties(f, table, &widths)?;
        f.write_str("<w:tblGrid>")?;
        for width in &widths {
            write!(f, "<w:gridCol w:w=\"{width}\"/>")?;
        }
        f.write_str("</w:tblGrid>")?;
        for (index, row) in rows.iter().enumerate() {
            let header = table.header && index == 0;
            f.write_str("<w:tr>")?;
            if header {
                f.write_str("<w:trPr><w:tblHeader/></w:trPr>")?;
            }
            for (column, width) in widths.iter().enumerate() {
                write!(
                    f,
                    "<w:tc><w:tcPr><w:tcW w:w=\"{width}\" w:type=\"dxa\"/></w:tcPr>"
                )?;
                write_paragraph(f, row.get(column).map_or("", String::as_str), header)?;
                f.write_str("</w:tc>")?;
            }
            f.write_str("</w:tr>")?;
        }
        f.write_str("</w:tbl>")
    }
}

/// Write the `w:tblPr` element of `table`, whose columns have `widths`.
fn write_properties(f: &mut fmt::Formatter<'_>, table: &Table, widths: &[u64]) -> fmt::Result {
    // Children in `CT_TblPr` order: tblStyle, tblW, tblBorders, tblLayout,
    // tblLook.
    f.write_str("<w:tblPr>")?;
    if let Some(style) = &table.style {
        write!(f, "<w:tblStyle w:val=\"{}\"/>", escape_attr(style))?;
    }
    let fixed = !table.column_widths.is_empty();
    if fixed {
        let total = widths
            .iter()
            .fold(0_u64, |total, width| total.saturating_add(*width));
        write!(f, "<w:tblW w:w=\"{total}\" w:type=\"dxa\"/>")?;
    } else {
        // 5,000 fiftieths of a percent: the full text width.
        f.write_str("<w:tblW w:w=\"5000\" w:type=\"pct\"/>")?;
    }
    if table.style.is_none() {
        f.write_str(BORDERS)?;
    }
    if fixed {
        f.write_str("<w:tblLayout w:type=\"fixed\"/>")?;
    }
    f.write_str(if table.header {
        LOOK_WITH_HEADER
    } else {
        LOOK_WITHOUT_HEADER
    })?;
    f.write_str("</w:tblPr>")
}

/// Write the paragraph of a cell that holds `text`, in bold if `bold`.
///
/// The text goes in a single run: `w:t` elements that preserve spaces for
/// the text, `w:br` for line breaks and `w:tab` for tabs. A cell without
/// text gets an empty paragraph, since every cell must end with one.
fn write_paragraph(f: &mut fmt::Formatter<'_>, text: &str, bold: bool) -> fmt::Result {
    let content = run_content(text);
    if content.is_empty() {
        return f.write_str("<w:p/>");
    }
    f.write_str(if bold {
        "<w:p><w:r><w:rPr><w:b/></w:rPr>"
    } else {
        "<w:p><w:r>"
    })?;
    f.write_str(&content)?;
    f.write_str("</w:r></w:p>")
}

/// Return the run content for `text`: `\r\n`, `\r` and `\n` become
/// `<w:br/>`, `\t` becomes `<w:tab/>`, and the text between them goes in
/// `<w:t xml:space="preserve">` elements, escaped. Text replacements split
/// their values the same way ([`split_run_text`]).
fn run_content(text: &str) -> String {
    let mut content = String::new();
    for piece in split_run_text(text) {
        match piece {
            RunText::Text(part) => {
                // Text made only of characters XML can't represent escapes
                // to nothing: skip the element rather than write an empty
                // one.
                let part = escape_text(part);
                if !part.is_empty() {
                    content.push_str("<w:t xml:space=\"preserve\">");
                    content.push_str(&part);
                    content.push_str("</w:t>");
                }
            }
            RunText::Break => content.push_str("<w:br/>"),
            RunText::Tab => content.push_str("<w:tab/>"),
        }
    }
    content
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::snippet::{self, Element};

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Generate and parse the snippet of `table`, checking it has no text
    /// outside `w:t` and no namespace declarations below the root.
    fn parse(table: &Table) -> Element {
        let xml = table.to_xml();
        let tbl = snippet::parse(&xml);
        assert_eq!(tbl.name, "w:tbl");
        assert_eq!(
            tbl.declarations,
            [(
                "w".to_owned(),
                "http://schemas.openxmlformats.org/wordprocessingml/2006/main".to_owned()
            )]
        );
        for element in tbl.descendants().into_iter().skip(1) {
            assert!(element.declarations.is_empty(), "{xml}");
            assert!(element.name == "w:t" || element.text.is_empty(), "{xml}");
        }
        tbl
    }

    /// Return the rows of `tbl`.
    fn rows(tbl: &Element) -> Vec<&Element> {
        tbl.children
            .iter()
            .filter(|child| child.name == "w:tr")
            .collect()
    }

    /// Return the cells of `row`.
    fn cells(row: &Element) -> Vec<&Element> {
        row.children
            .iter()
            .filter(|child| child.name == "w:tc")
            .collect()
    }

    /// Return the `w:w` of every `w:gridCol`.
    fn grid(tbl: &Element) -> Vec<&str> {
        let grid = tbl.child("w:tblGrid");
        assert!(grid.children.iter().all(|col| col.name == "w:gridCol"));
        grid.children
            .iter()
            .map(|col| col.attr("w:w").unwrap())
            .collect()
    }

    /// Return the outline of the only paragraph of the only cell.
    fn single_cell(table: &Table) -> String {
        let tbl = parse(table);
        let rows = rows(&tbl);
        assert_eq!(rows.len(), 1);
        let cells = cells(rows[0]);
        assert_eq!(cells.len(), 1);
        cells[0].child("w:p").outline()
    }

    /// Return the content of the only run of the only cell, as `w:t` texts
    /// and the names of other elements.
    fn run_items(text: &str) -> Vec<String> {
        let tbl = parse(&Table::new([[text]]));
        let row = tbl.child("w:tr");
        let run = row.child("w:tc").child("w:p").child("w:r");
        run.children
            .iter()
            .map(|item| {
                if item.name == "w:t" {
                    assert_eq!(item.attr("xml:space"), Some("preserve"));
                    format!("t:{}", item.text)
                } else {
                    item.name.clone()
                }
            })
            .collect()
    }

    // ── Structure ─────────────────────────────────────────────────────────────

    #[test]
    fn table_elements_follow_the_schema_order() {
        let table = Table::new([["a", "b"], ["c", "d"]]);
        let cell = "w:tc[w:tcPr[w:tcW],w:p[w:r[w:t]]]";
        let row = format!("w:tr[{cell},{cell}]");
        assert_eq!(
            parse(&table).outline(),
            format!(
                "w:tbl[w:tblPr[w:tblW,w:tblBorders[w:top,w:left,w:bottom,w:right,w:insideH,\
                 w:insideV],w:tblLook],w:tblGrid[w:gridCol,w:gridCol],{row},{row}]"
            )
        );
    }

    #[test]
    fn table_properties_follow_the_schema_order() {
        // CT_TblPr: tblStyle, tblW, tblBorders, tblLayout, tblLook.
        let all_but_borders = Table::new([["a"]])
            .style("Grid")
            .column_widths([Length::from_cm(2.0)])
            .header(true);
        let tbl = parse(&all_but_borders);
        assert_eq!(
            tbl.child("w:tblPr").child_names(),
            ["w:tblStyle", "w:tblW", "w:tblLayout", "w:tblLook"]
        );
        let all_but_style = Table::new([["a"]]).column_widths([Length::from_cm(2.0)]);
        let tbl = parse(&all_but_style);
        assert_eq!(
            tbl.child("w:tblPr").child_names(),
            ["w:tblW", "w:tblBorders", "w:tblLayout", "w:tblLook"]
        );
    }

    #[test]
    fn style_replaces_the_default_borders() {
        let tbl = parse(&Table::new([["a"]]).style("TableGrid"));
        let properties = tbl.child("w:tblPr");
        assert_eq!(
            properties.child("w:tblStyle").attr("w:val"),
            Some("TableGrid")
        );
        assert!(!properties.child_names().contains(&"w:tblBorders"));

        let tbl = parse(&Table::new([["a"]]));
        let borders = tbl.child("w:tblPr").child("w:tblBorders");
        for border in &borders.children {
            assert_eq!(
                border.attributes,
                [
                    ("w:val".to_owned(), "single".to_owned()),
                    ("w:sz".to_owned(), "4".to_owned()),
                    ("w:space".to_owned(), "0".to_owned()),
                    ("w:color".to_owned(), "auto".to_owned()),
                ]
            );
        }
    }

    #[test]
    fn empty_style_id_means_no_style() {
        let tbl = parse(&Table::new([["a"]]).style("TableGrid").style(""));
        assert_eq!(
            tbl.child("w:tblPr").child_names(),
            ["w:tblW", "w:tblBorders", "w:tblLook"]
        );
    }

    #[test]
    fn style_id_is_escaped() {
        let tbl = parse(&Table::new([["a"]]).style("My \"<&>\" style\u{1}"));
        let style = tbl.child("w:tblPr").child("w:tblStyle");
        assert_eq!(style.attr("w:val"), Some("My \"<&>\" style"));
    }

    #[test]
    fn rows_are_padded_to_the_widest_row() {
        let table = Table::new(vec![vec!["a"], vec!["b", "c", "d"], vec![]]);
        let tbl = parse(&table);
        assert_eq!(grid(&tbl).len(), 3);
        let rows = rows(&tbl);
        assert_eq!(rows.len(), 3);
        let outlines: Vec<Vec<String>> = rows
            .iter()
            .map(|row| {
                cells(row)
                    .iter()
                    .map(|cell| cell.child("w:p").outline())
                    .collect()
            })
            .collect();
        let text = "w:p[w:r[w:t]]".to_owned();
        let empty = "w:p".to_owned();
        assert_eq!(
            outlines,
            [
                vec![text.clone(), empty.clone(), empty.clone()],
                vec![text.clone(), text.clone(), text],
                vec![empty.clone(), empty.clone(), empty],
            ]
        );
    }

    #[test]
    fn table_without_rows_is_one_empty_cell() {
        let table = Table::new(Vec::<Vec<String>>::new());
        assert_eq!(single_cell(&table), "w:p");
        assert_eq!(grid(&parse(&table)), ["9638"]);
        assert_eq!(single_cell(&table.header(true)), "w:p");
    }

    #[test]
    fn rows_without_cells_get_one_empty_cell() {
        let tbl = parse(&Table::new([[""; 0]; 3]));
        assert_eq!(grid(&tbl), ["9638"]);
        let rows = rows(&tbl);
        assert_eq!(rows.len(), 3);
        for row in rows {
            assert_eq!(row.outline(), "w:tr[w:tc[w:tcPr[w:tcW],w:p]]");
        }
    }

    #[test]
    fn every_cell_has_properties_then_a_paragraph() {
        let tbl = parse(&Table::new([["", "a\nb"], ["c", ""]]).header(true));
        for row in rows(&tbl) {
            for cell in cells(row) {
                let names = cell.child_names();
                assert_eq!(names[0], "w:tcPr");
                assert_eq!(cell.child("w:tcPr").child_names()[0], "w:tcW");
                assert!(names.len() > 1 && names[1..].iter().all(|name| *name == "w:p"));
            }
        }
    }

    // ── Cell text ─────────────────────────────────────────────────────────────

    #[test]
    fn line_breaks_and_tabs_become_elements() {
        assert_eq!(
            run_items("a\r\nb\rc\nd\te"),
            [
                "t:a", "w:br", "t:b", "w:br", "t:c", "w:br", "t:d", "w:tab", "t:e"
            ]
        );
        assert_eq!(run_items("\n\n"), ["w:br", "w:br"]);
        assert_eq!(run_items("\r\r\n"), ["w:br", "w:br"]);
        assert_eq!(run_items("\t x \t"), ["w:tab", "t: x ", "w:tab"]);
    }

    #[test]
    fn empty_cell_is_an_empty_paragraph() {
        assert_eq!(single_cell(&Table::new([[""]])), "w:p");
        // Nothing left once illegal characters are dropped.
        assert_eq!(single_cell(&Table::new([["\u{0}\u{1b}"]])), "w:p");
    }

    #[test]
    fn cell_text_is_escaped_and_illegal_characters_are_dropped() {
        let text = "<b> & \"quotes\" ]]> \u{0}\u{b}\u{c}\u{1f}\u{fffe}\u{ffff}end";
        let xml = Table::new([[text]]).to_xml();
        assert!(
            !xml.contains(|c: char| {
                matches!(c, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}')
                    || matches!(c, '\u{fffe}' | '\u{ffff}')
            }),
            "{xml:?}"
        );
        assert_eq!(run_items(text), ["t:<b> & \"quotes\" ]]> end"]);
    }

    #[test]
    fn spaces_are_preserved() {
        assert_eq!(run_items("  two  spaces  "), ["t:  two  spaces  "]);
    }

    // ── Header row ────────────────────────────────────────────────────────────

    #[test]
    fn header_row_repeats_and_is_bold() {
        let tbl = parse(&Table::new([["Name", ""], ["Ada", "Lovelace"]]).header(true));
        let look = tbl.child("w:tblPr").child("w:tblLook");
        assert_eq!(look.attr("w:val"), Some("04A0"));
        assert_eq!(look.attr("w:firstRow"), Some("1"));

        let rows = rows(&tbl);
        assert_eq!(rows[0].child_names()[0], "w:trPr");
        assert_eq!(rows[0].child("w:trPr").outline(), "w:trPr[w:tblHeader]");
        assert_eq!(
            cells(rows[0])[0].child("w:p").outline(),
            "w:p[w:r[w:rPr[w:b],w:t]]"
        );
        assert_eq!(cells(rows[0])[1].child("w:p").outline(), "w:p");

        assert!(!rows[1].child_names().contains(&"w:trPr"));
        for cell in cells(rows[1]) {
            assert_eq!(cell.child("w:p").outline(), "w:p[w:r[w:t]]");
        }
    }

    #[test]
    fn without_header_no_row_repeats_and_first_row_formatting_is_off() {
        let tbl = parse(&Table::new([["Name"], ["Ada"]]));
        let look = tbl.child("w:tblPr").child("w:tblLook");
        assert_eq!(
            look.attributes,
            [
                ("w:val".to_owned(), "0480".to_owned()),
                ("w:firstRow".to_owned(), "0".to_owned()),
                ("w:lastRow".to_owned(), "0".to_owned()),
                ("w:firstColumn".to_owned(), "1".to_owned()),
                ("w:lastColumn".to_owned(), "0".to_owned()),
                ("w:noHBand".to_owned(), "0".to_owned()),
                ("w:noVBand".to_owned(), "1".to_owned()),
            ]
        );
        for row in rows(&tbl) {
            assert!(!row.child_names().contains(&"w:trPr"));
            assert_eq!(cells(row)[0].child("w:p").outline(), "w:p[w:r[w:t]]");
        }
    }

    // ── Widths ────────────────────────────────────────────────────────────────

    #[test]
    fn without_widths_the_table_spans_the_text_width() {
        let tbl = parse(&Table::new([["a", "b", "c"]]));
        let properties = tbl.child("w:tblPr");
        let width = properties.child("w:tblW");
        assert_eq!(width.attr("w:w"), Some("5000"));
        assert_eq!(width.attr("w:type"), Some("pct"));
        assert!(!properties.child_names().contains(&"w:tblLayout"));
        // 9,638 = 3 * 3,212 + 2: the first two columns get a twip more.
        assert_eq!(grid(&tbl), ["3213", "3213", "3212"]);
        for (cell, expected) in cells(tbl.child("w:tr"))
            .iter()
            .zip(["3213", "3213", "3212"])
        {
            let cell_width = cell.child("w:tcPr").child("w:tcW");
            assert_eq!(cell_width.attr("w:w"), Some(expected));
            assert_eq!(cell_width.attr("w:type"), Some("dxa"));
        }
    }

    #[test]
    fn explicit_widths_are_fixed_and_add_up_to_the_table_width() {
        let table = Table::new([["a", "b"], ["c", "d"]])
            .column_widths([Length::from_inches(1.0), Length::from_cm(2.0)]);
        let tbl = parse(&table);
        let properties = tbl.child("w:tblPr");
        let width = properties.child("w:tblW");
        // 1 in = 1,440 twips; 2 cm = 1,133.86 twips.
        assert_eq!(width.attr("w:w"), Some("2574"));
        assert_eq!(width.attr("w:type"), Some("dxa"));
        assert_eq!(
            properties.child("w:tblLayout").attr("w:type"),
            Some("fixed")
        );
        assert_eq!(grid(&tbl), ["1440", "1134"]);
        for row in rows(&tbl) {
            let widths: Vec<_> = cells(row)
                .iter()
                .map(|cell| cell.child("w:tcPr").child("w:tcW").attr("w:w").unwrap())
                .collect();
            assert_eq!(widths, ["1440", "1134"]);
        }
    }

    #[test]
    fn missing_widths_share_the_rest_of_the_default_width() {
        // 3 cm = 1,701 twips; the other two share 9,638 - 1,701 = 7,937.
        let table = Table::new([["a", "b", "c"]]).column_widths([Length::from_cm(3.0)]);
        let tbl = parse(&table);
        assert_eq!(grid(&tbl), ["1701", "3969", "3968"]);
        assert_eq!(
            tbl.child("w:tblPr").child("w:tblW").attr("w:w"),
            Some("9638")
        );
    }

    #[test]
    fn missing_widths_are_never_narrower_than_an_even_split() {
        // 16 cm leaves 567 twips, less than the 2 * 3,212.67 the other two
        // columns get with no widths at all.
        let table = Table::new([["a", "b", "c"]]).column_widths([Length::from_cm(16.0)]);
        assert_eq!(grid(&parse(&table)), ["9071", "3213", "3212"]);
        let wider_than_the_page = Table::new([["a", "b"]]).column_widths([Length::from_cm(30.0)]);
        assert_eq!(grid(&parse(&wider_than_the_page)), ["17008", "4819"]);
    }

    #[test]
    fn extra_widths_are_ignored() {
        let table =
            Table::new([["a"]]).column_widths([Length::from_pt(10.0), Length::from_pt(20.0)]);
        let tbl = parse(&table);
        assert_eq!(grid(&tbl), ["200"]);
        assert_eq!(
            tbl.child("w:tblPr").child("w:tblW").attr("w:w"),
            Some("200")
        );
    }

    #[test]
    fn empty_width_list_means_no_widths() {
        let table = Table::new([["a", "b"]]).column_widths([]);
        let tbl = parse(&table);
        assert_eq!(
            tbl.child("w:tblPr").child("w:tblW").attr("w:type"),
            Some("pct")
        );
        assert_eq!(grid(&tbl), ["4819", "4819"]);
    }

    #[test]
    fn huge_widths_saturate_instead_of_overflowing() {
        // u64::MAX EMU is 2.9e16 twips: 636 such columns overflow a u64 sum.
        let huge = Length::from_emu(u64::MAX);
        let table = Table::new([vec!["a"; 701]]).column_widths(vec![huge; 700]);
        let tbl = parse(&table);
        let twips = huge.as_twips().to_string();
        let widths = grid(&tbl);
        assert!(widths[..700].iter().all(|width| *width == twips));
        // Nothing is left of the default width: the last column gets its
        // share of an even split, 9,638 / 701 twips.
        assert_eq!(widths[700], "13");
        let total = u64::MAX.to_string();
        assert_eq!(
            tbl.child("w:tblPr").child("w:tblW").attr("w:w"),
            Some(total.as_str())
        );
    }

    #[test]
    fn many_columns_split_the_default_width() {
        let tbl = parse(&Table::new([vec![""; 10_000]]));
        let widths = grid(&tbl);
        assert_eq!(widths.len(), 10_000);
        assert!(widths.iter().all(|width| *width == "0" || *width == "1"));
        assert_eq!(widths.iter().filter(|width| **width == "1").count(), 9_638);
    }
}
