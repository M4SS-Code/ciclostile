//! `Docx::replace_with_table` end to end: a paragraph holding only the
//! placeholder becomes a table, and a placeholder that shares its
//! paragraph is refused before anything changes.

pub mod common;

use ciclostile::{Docx, Table};
use common::{DOCUMENT, Fixture};

/// A small table with a header row.
fn items() -> Table {
    Table::new([
        ["Item", "Qty"],
        ["Analytical engine", "1"],
        ["Punched cards", "500"],
    ])
    .header(true)
}

/// Return the byte offsets of the `w:tbl` start tags in `xml` (not
/// `w:tblPr` and the like).
fn tables(xml: &str) -> Vec<usize> {
    xml.match_indices("<w:tbl")
        .filter(|(at, tag)| matches!(xml.as_bytes().get(at + tag.len()), Some(b'>' | b' ')))
        .map(|(at, _)| at)
        .collect()
}

/// Assert that no table in `xml` directly follows another: Word merges
/// adjacent tables.
fn assert_tables_apart(xml: &str) {
    for (end, _) in xml.match_indices("</w:tbl>") {
        let rest = &xml[end..];
        if let Some(&next) = tables(rest).first() {
            assert!(rest[..next].contains("<w:p"), "adjacent tables in {xml}");
        }
    }
}

#[test]
fn placeholder_paragraph_becomes_a_table() {
    let template = Fixture::template().zip();
    let baseline = common::untouched(&template);
    let mut docx = Docx::from_bytes(template).unwrap();
    assert_eq!(docx.replace_with_table("{{items}}", &items()).unwrap(), 1);
    let output = docx.to_bytes();

    let added = common::assert_kept_except(&baseline, &output, &["word/document.xml"]);
    assert_eq!(added, [] as [String; 0]);

    let document = common::read_text(&output, "word/document.xml");
    assert!(!document.contains("items}}"), "{document}");
    let tables = tables(&document);
    assert_eq!(tables.len(), 2, "the new table and the template's");
    // The new table takes the paragraph's place, right after the picture
    // and before the template's table, with a paragraph between the two.
    assert!(tables[0] > document.find("Signed: ").unwrap());
    assert!(tables[1] > tables[0]);
    assert!(document[tables[0]..tables[1]].contains("Punched cards"));
    assert_tables_apart(&document);
    let paragraphs = common::paragraphs(&document);
    for cell in [
        "Item",
        "Qty",
        "Analytical engine",
        "1",
        "Punched cards",
        "500",
    ] {
        assert!(paragraphs.iter().any(|text| text == cell), "{cell}");
    }
    // The XML before the replaced paragraph is untouched.
    let start = DOCUMENT.find(r#"<w:p w14:paraId="6F708192""#).unwrap();
    assert_eq!(document[..start], DOCUMENT[..start]);

    Docx::from_bytes(output).unwrap();
}

#[test]
fn table_can_replace_a_footer_paragraph() {
    let template = Fixture::template().zip();
    let baseline = common::untouched(&template);
    let mut docx = Docx::from_bytes(template).unwrap();
    assert_eq!(
        docx.replace_with_table("{{signature}}", &items()).unwrap(),
        1
    );
    let output = docx.to_bytes();

    let added = common::assert_kept_except(&baseline, &output, &["word/footer1.xml"]);
    assert_eq!(added, [] as [String; 0]);
    let footer = common::read_text(&output, "word/footer1.xml");
    assert_eq!(tables(&footer).len(), 1);
    // A story must end with a paragraph.
    assert!(!footer.ends_with("</w:tbl></w:ftr>"), "{footer}");
    assert!(!footer.contains("{{signature}}"));
}

#[test]
fn placeholder_sharing_its_paragraph_is_refused() {
    let template = Fixture::template().zip();
    let baseline = common::untouched(&template);
    let mut docx = Docx::from_bytes(template).unwrap();
    let err = docx
        .replace_with_table("{{customer}}", &items())
        .unwrap_err();
    assert_eq!(err.as_str(), "{{customer}}");
    assert_eq!(
        err.to_string(),
        "table placeholder `{{customer}}` must be the only content of its paragraph"
    );
    assert_eq!(docx.to_bytes(), baseline);
}

#[test]
fn refusal_leaves_qualifying_paragraphs_alone() {
    // `{{signature}}` is alone in a footer paragraph, but shares one in the
    // body: nothing is replaced anywhere, and the document stays usable.
    let fixture = Fixture::template().with(
        "word/document.xml",
        DOCUMENT.replace("Signed: ", "Signed by {{signature}}: "),
    );
    let template = fixture.zip();
    let baseline = common::untouched(&template);
    let mut docx = Docx::from_bytes(template).unwrap();
    let err = docx
        .replace_with_table("{{signature}}", &items())
        .unwrap_err();
    assert_eq!(err.into_inner(), "{{signature}}");
    assert_eq!(docx.to_bytes(), baseline);

    assert_eq!(docx.replace_text([("{{signature}}", "Ada")]), 2);
}

#[test]
fn missing_placeholder_inserts_nothing() {
    let template = Fixture::template().zip();
    let baseline = common::untouched(&template);
    let mut docx = Docx::from_bytes(template).unwrap();
    assert_eq!(docx.replace_with_table("{{nowhere}}", &items()).unwrap(), 0);
    assert_eq!(docx.to_bytes(), baseline);
}

#[test]
fn empty_placeholder_matches_nothing() {
    // Empty paragraphs would otherwise all qualify.
    let template = Fixture::template().zip();
    let baseline = common::untouched(&template);
    let mut docx = Docx::from_bytes(template).unwrap();
    assert_eq!(docx.replace_with_table("", &items()).unwrap(), 0);
    assert_eq!(docx.to_bytes(), baseline);
}
