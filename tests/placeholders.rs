//! `Docx::placeholders` end to end: what the sample template holds, and
//! what is left once it is filled.

pub mod common;

use ciclostile::{Docx, Image, Table};
use common::Fixture;

fn template() -> Docx {
    Docx::from_bytes(Fixture::template().zip()).unwrap()
}

#[test]
fn template_placeholders_are_listed_in_document_order() {
    // The main part first: `{{customer}}` once, as its other copy is a
    // tracked deletion, `{{items}}` split across runs, and `{{boxed}}`
    // in both copies of its text box. Then the header, the footer, the
    // endnotes and the footnotes.
    assert_eq!(
        template().placeholders("{{", "}}"),
        [
            "{{number}}",
            "{{customer}}",
            "{{site}}",
            "{{phone}}",
            "{{logo}}",
            "{{items}}",
            "{{total}}",
            "{{boxed}}",
            "{{boxed}}",
            "{{date}}",
            "{{company}}",
            "{{logo}}",
            "{{company}}",
            "{{company}}",
            "{{signature}}",
            "{{currency}}",
        ]
    );
}

#[test]
fn filled_template_has_none_left() {
    let mut docx = template();
    let mut found = docx.placeholders("{{", "}}");
    found.sort();
    found.dedup();
    assert_eq!(found.len(), 12, "{found:?}");

    let images = ["{{logo}}", "{{signature}}"];
    let tables = ["{{items}}"];
    let text: Vec<(&str, &str)> = found
        .iter()
        .map(String::as_str)
        .filter(|key| !images.contains(key) && !tables.contains(key))
        .map(|key| (key, "filled"))
        .collect();
    docx.replace_text(text);
    let image = Image::from_bytes(common::PNG).unwrap();
    for placeholder in images {
        assert!(docx.replace_with_image(placeholder, &image) > 0);
    }
    for placeholder in tables {
        let replaced = docx.replace_with_table(placeholder, &Table::new([["a"]]));
        assert_eq!(replaced.unwrap(), 1);
    }

    assert_eq!(docx.placeholders("{{", "}}"), Vec::<String>::new());
}

#[test]
fn what_is_left_is_listed() {
    let mut docx = template();
    docx.replace_text([("{{company}}", "Ada Ltd"), ("{{number}}", "{{total}}")]);
    let left = docx.placeholders("{{", "}}");
    assert!(
        !left
            .iter()
            .any(|found| found == "{{company}}" || found == "{{number}}")
    );
    // A value that holds a placeholder is listed like one.
    assert_eq!(left.iter().filter(|found| *found == "{{total}}").count(), 2);
}

#[test]
fn other_delimiters_work() {
    assert_eq!(
        template().placeholders("{{c", "}}"),
        [
            "{{customer}}",
            "{{company}}",
            "{{company}}",
            "{{company}}",
            "{{currency}}"
        ]
    );
    assert_eq!(template().placeholders("[[", "]]"), Vec::<String>::new());
    assert_eq!(template().placeholders("", "}}"), Vec::<String>::new());
}
