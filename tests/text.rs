//! `Docx::replace_text` end to end: placeholders are replaced in every
//! story part whatever runs Word split them into, and only the parts that
//! changed are rewritten.

pub mod common;

use std::collections::{BTreeMap, HashMap};

use ciclostile::Docx;
use common::{DOCUMENT, Fixture};

/// Open the template and apply `edit`. Return what ciclostile writes for the
/// template untouched, what it writes after `edit`, and what `edit`
/// returned.
fn fill<T>(edit: impl FnOnce(&mut Docx) -> T) -> (Vec<u8>, Vec<u8>, T) {
    let template = Fixture::template().zip();
    let mut docx = Docx::from_bytes(template.clone()).unwrap();
    let result = edit(&mut docx);
    (
        common::untouched(&template),
        docx.to_bytes().to_vec(),
        result,
    )
}

/// Assert that the part `name` of the archive `zip` has a paragraph whose
/// text is `text`.
fn assert_paragraph(zip: &[u8], name: &str, text: &str) {
    let paragraphs = common::paragraphs(&common::read_text(zip, name));
    assert!(
        paragraphs.iter().any(|paragraph| paragraph == text),
        "no paragraph {text:?} in {name}: {paragraphs:#?}"
    );
}

#[test]
fn placeholder_split_across_runs_is_replaced() {
    // `{{`, `number` and `}}` are three runs with spell-check marks between
    // them.
    let (baseline, output, count) = fill(|docx| docx.replace_text([("{{number}}", "2026-0042")]));
    assert_eq!(count, 1);
    assert_paragraph(&output, "word/document.xml", "Invoice 2026-0042");
    let added = common::assert_kept_except(&baseline, &output, &["word/document.xml"]);
    assert_eq!(added, [] as [String; 0]);
}

#[test]
fn placeholders_are_replaced_in_every_story() {
    let (baseline, output, count) = fill(|docx| {
        docx.replace_text([
            ("{{number}}", "42"),
            ("{{customer}}", "Ada Lovelace"),
            ("{{site}}", "example.com"),
            ("{{phone}}", "555-0100"),
            ("{{total}}", "£ 17,000"),
            ("{{boxed}}", "Paid"),
            ("{{date}}", "3 November 2026"),
            ("{{company}}", "Babbage & Co."),
            ("{{currency}}", "pounds"),
        ])
    });
    // In the body: one each, but the text box twice, as Word stores its
    // content twice (DrawingML and the VML fallback). Then the company in
    // the header, the footer and the endnote, and the currency in the
    // footnote.
    assert_eq!(count, 8 + 3 + 1);

    let document = "word/document.xml";
    assert_paragraph(&output, document, "Invoice 42");
    assert_paragraph(&output, document, "Dear Ada Lovelace,");
    assert_paragraph(&output, document, "Visit example.com or call 555-0100.");
    assert_paragraph(&output, document, "£ 17,000");
    assert_paragraph(&output, document, "3 November 2026");
    let paragraphs = common::paragraphs(&common::read_text(&output, document));
    let paid = paragraphs.iter().filter(|text| *text == "Paid").count();
    assert_eq!(paid, 2, "{paragraphs:#?}");
    assert_paragraph(&output, "word/header1.xml", "Babbage & Co. - {{logo}}");
    assert_paragraph(&output, "word/footer1.xml", "Page 1 - Babbage & Co.");
    assert_paragraph(&output, "word/footnotes.xml", " Prices in pounds.");
    assert_paragraph(
        &output,
        "word/endnotes.xml",
        " Issued by Babbage & Co. in London.",
    );

    // Every story changed; nothing else did, and nothing was added.
    let added = common::assert_kept_except(
        &baseline,
        &output,
        &[
            "word/document.xml",
            "word/footnotes.xml",
            "word/endnotes.xml",
            "word/header1.xml",
            "word/footer1.xml",
        ],
    );
    assert_eq!(added, [] as [String; 0]);
}

#[test]
fn only_changed_parts_are_rewritten() {
    let (baseline, output, count) = fill(|docx| docx.replace_text([("{{currency}}", "euros")]));
    assert_eq!(count, 1);
    let added = common::assert_kept_except(&baseline, &output, &["word/footnotes.xml"]);
    assert_eq!(added, [] as [String; 0]);
}

#[test]
fn edited_part_keeps_the_bytes_around_the_edit() {
    let (_, output, _) = fill(|docx| docx.replace_text([("{{customer}}", "Ada")]));
    let after = common::read_text(&output, "word/document.xml");

    // Everything before and after the paragraph holding the placeholder
    // is byte for byte the same: declaration, root namespaces, rsids.
    let start = DOCUMENT.find(r#"<w:p w14:paraId="2B3C4D5E""#).unwrap();
    let end = start + DOCUMENT[start..].find("</w:p>").unwrap() + "</w:p>".len();
    let tail = &DOCUMENT[end..];
    assert_eq!(after[..start], DOCUMENT[..start]);
    assert!(after.ends_with(tail));
    assert_paragraph(&output, "word/document.xml", "Dear Ada,");
    // The tracked deletion of another copy is still there.
    let paragraph = &after[start..after.len() - tail.len()];
    assert!(
        paragraph.contains("<w:delText xml:space=\"preserve\">{{customer}} </w:delText>"),
        "{paragraph}"
    );
}

#[test]
fn unmatched_placeholders_leave_the_document_untouched() {
    // `{{title}}` is only in the core properties, which are not searched.
    let (baseline, output, count) =
        fill(|docx| docx.replace_text([("{{nowhere}}", "x"), ("{{title}}", "y")]));
    assert_eq!(count, 0);
    assert_eq!(output, baseline);
}

#[test]
fn empty_keys_are_ignored() {
    let (baseline, output, count) = fill(|docx| docx.replace_text([("", "x")]));
    assert_eq!(count, 0);
    assert_eq!(output, baseline);
}

#[test]
fn replacements_are_made_in_a_single_pass() {
    // The value given for one key is searched neither for another key nor
    // for its own.
    let (_, output, count) = fill(|docx| {
        docx.replace_text([
            ("{{customer}}", "{{phone}} and {{customer}}"),
            ("{{phone}}", "555-0100"),
        ])
    });
    assert_eq!(count, 2);
    assert_paragraph(
        &output,
        "word/document.xml",
        "Dear {{phone}} and {{customer}},",
    );
    assert_paragraph(
        &output,
        "word/document.xml",
        "Visit {{site}} or call 555-0100.",
    );
}

#[test]
fn longer_key_wins_at_the_same_position() {
    let (_, output, count) =
        fill(|docx| docx.replace_text([("{{cust", "short"), ("{{customer}}", "long")]));
    assert_eq!(count, 1);
    assert_paragraph(&output, "word/document.xml", "Dear long,");
}

#[test]
fn first_value_wins_for_a_repeated_key() {
    let (_, output, count) =
        fill(|docx| docx.replace_text([("{{date}}", "first"), ("{{date}}", "second")]));
    assert_eq!(count, 1);
    assert_paragraph(&output, "word/document.xml", "first");
}

#[test]
fn line_breaks_and_tabs_become_breaks_and_tabs() {
    let (_, output, _) =
        fill(|docx| docx.replace_text([("{{date}}", "one\ntwo\r\nthree\rfour\tfive")]));
    assert_paragraph(&output, "word/document.xml", "one\ntwo\nthree\nfour\tfive");
    let document = common::read_text(&output, "word/document.xml");
    assert_eq!(common::count(&document, "<w:br/>"), 3, "{document}");
    assert_eq!(common::count(&document, "<w:tab/>"), 1, "{document}");
}

#[test]
fn values_are_escaped() {
    let (_, output, _) =
        fill(|docx| docx.replace_text([("{{date}}", "<b>\"Fish\" & 'chips'</b>\u{1}\u{fffe}")]));
    assert_paragraph(&output, "word/document.xml", "<b>\"Fish\" & 'chips'</b>");
    // The result is well-formed: it opens again.
    Docx::from_bytes(output).unwrap();
}

#[test]
fn values_can_come_from_maps() {
    let template = Fixture::template().zip();

    let mut docx = Docx::from_bytes(template.clone()).unwrap();
    let values = HashMap::from([
        ("{{date}}".to_owned(), "today".to_owned()),
        ("{{total}}".to_owned(), "0".to_owned()),
    ]);
    assert_eq!(docx.replace_text(&values), 2);

    let mut docx = Docx::from_bytes(template).unwrap();
    let values = BTreeMap::from([("{{date}}", String::from("today"))]);
    assert_eq!(docx.replace_text(values), 1);
}

#[test]
fn filled_document_can_be_filled_again() {
    let (_, output, _) = fill(|docx| docx.replace_text([("{{customer}}", "{{phone}}")]));
    let mut docx = Docx::from_bytes(output).unwrap();
    assert_eq!(docx.replace_text([("{{phone}}", "555-0100")]), 2);
    let output = docx.to_bytes();
    assert_paragraph(&output, "word/document.xml", "Dear 555-0100,");
    assert_paragraph(
        &output,
        "word/document.xml",
        "Visit {{site}} or call 555-0100.",
    );
}
