//! Opening documents: what `Docx::from_bytes` accepts, how it reports the
//! files it can't open, and that damaged input never panics.

pub mod common;

use std::{error::Error as _, io::Cursor};

use ciclostile::{Docx, error::InvalidDocx};
use common::{CONTENT_TYPES, DOCUMENT, DOCUMENT_RELS, Fixture, PACKAGE_RELS};

/// Open `fixture` and return why it was rejected.
fn rejection(fixture: &Fixture) -> InvalidDocx {
    match Docx::from_bytes(fixture.zip()) {
        Ok(docx) => panic!("opened {docx:?}"),
        Err(err) => err,
    }
}

/// Assert that `err` reports the part `name` as missing.
fn assert_missing(err: &InvalidDocx, name: &str) {
    assert!(
        matches!(err, InvalidDocx::MissingPart(part) if part == name),
        "{err:?}"
    );
    assert_eq!(err.to_string(), format!("missing part `{name}`"));
}

/// Assert that `err` reports the part `name` as malformed XML.
fn assert_malformed(err: &InvalidDocx, name: &str) {
    assert!(
        matches!(err, InvalidDocx::Xml { part, .. } if part == name),
        "{err:?}"
    );
    assert_eq!(err.to_string(), format!("unreadable XML part `{name}`"));
    assert!(err.source().is_some(), "the XML error is the source");
}

/// The main part of a package that only holds a main part with `body`.
fn minimal_document(body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n\
         <w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
         <w:body>{body}</w:body></w:document>"
    )
}

/// A package whose `_rels/.rels` holds `relationships`, with the template's
/// content types and `word/document.xml`.
fn with_package_relationships(relationships: &str) -> Fixture {
    Fixture::new([("[Content_Types].xml", CONTENT_TYPES.as_bytes())])
        .with(
            "_rels/.rels",
            format!(
                "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
                 {relationships}</Relationships>"
            ),
        )
        .with("word/document.xml", minimal_document(""))
}

// ── Accepted documents ───────────────────────────────────────────────────────

#[test]
fn word_template_opens() {
    Docx::from_bytes(Fixture::template().zip()).unwrap();
}

#[test]
fn main_part_is_found_through_the_package_relationships() {
    // Nothing hardcodes `word/document.xml`: the main part can live
    // anywhere, and its stories are found relative to it.
    let main = minimal_document("<w:p><w:r><w:t>{{name}}</w:t></w:r></w:p>");
    let fixture = Fixture::new([("[Content_Types].xml", CONTENT_TYPES.as_bytes())])
        .with(
            "_rels/.rels",
            concat!(
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
                r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="/content/main.xml"/>"#,
                "</Relationships>",
            ),
        )
        .with("content/main.xml", &main)
        .with(
            "content/_rels/main.xml.rels",
            concat!(
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
                r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="parts/../header.xml"/>"#,
                r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="/shared/footer.xml"/>"#,
                "</Relationships>",
            ),
        )
        .with("content/header.xml", common::HEADER)
        .with("shared/footer.xml", common::FOOTER);
    let template = fixture.zip();
    let mut docx = Docx::from_bytes(template.clone()).unwrap();
    // The name in the main part, the company in the header and the footer.
    let count = docx.replace_text([("{{name}}", "Ada"), ("{{company}}", "Babbage & Co.")]);
    assert_eq!(count, 3);
    let added = common::assert_kept_except(
        &common::untouched(&template),
        &docx.to_bytes(),
        &[
            "content/main.xml",
            "content/header.xml",
            "shared/footer.xml",
        ],
    );
    assert_eq!(added, [] as [String; 0]);
}

#[test]
fn stories_shared_by_sections_are_read_once() {
    // Two more sections share the header, once spelled with another case:
    // part names are compared ignoring ASCII case.
    let rels = DOCUMENT_RELS.replace(
        "</Relationships>",
        concat!(
            r#"<Relationship Id="rId11" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/>"#,
            r#"<Relationship Id="rId12" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="HEADER1.xml"/>"#,
            "</Relationships>",
        ),
    );
    let template = Fixture::template()
        .with("word/_rels/document.xml.rels", rels)
        .zip();
    let mut docx = Docx::from_bytes(template.clone()).unwrap();
    // Once each in the header, the footer and the endnote: the header
    // counts once.
    assert_eq!(docx.replace_text([("{{company}}", "Babbage & Co.")]), 3);
    let added = common::assert_kept_except(
        &common::untouched(&template),
        &docx.to_bytes(),
        &["word/header1.xml", "word/endnotes.xml", "word/footer1.xml"],
    );
    assert_eq!(added, [] as [String; 0]);
}

#[test]
fn relationships_to_package_parts_are_not_stories() {
    // A header relationship to the content types part or to a
    // relationships part is malformed: following it would edit those parts
    // as text. Such targets are skipped, and the document still opens and
    // writes back as it was.
    let rels = DOCUMENT_RELS.replace(
        "</Relationships>",
        concat!(
            r#"<Relationship Id="rId11" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="/[Content_Types].xml"/>"#,
            r#"<Relationship Id="rId12" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="_rels/document.xml.rels"/>"#,
            r#"<Relationship Id="rId13" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="/"/>"#,
            "</Relationships>",
        ),
    );
    let fixture = Fixture::template().with("word/_rels/document.xml.rels", rels);
    let docx = Docx::from_bytes(fixture.zip()).unwrap();
    common::assert_copied(&fixture.zip(), &docx.to_bytes());
}

#[test]
fn external_story_targets_are_not_followed() {
    let rels = DOCUMENT_RELS.replace(
        "</Relationships>",
        concat!(
            r#"<Relationship Id="rId11" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="https://example.com/header.xml" TargetMode="External"/>"#,
            "</Relationships>",
        ),
    );
    Docx::from_bytes(
        Fixture::template()
            .with("word/_rels/document.xml.rels", rels)
            .zip(),
    )
    .unwrap();
}

// ── Not a document ───────────────────────────────────────────────────────────

#[test]
fn file_that_is_not_a_zip_archive_is_rejected() {
    let err = Docx::from_bytes(b"{\\rtf1\\ansi Hello}".to_vec()).unwrap_err();
    assert!(matches!(err, InvalidDocx::Zip(_)), "{err:?}");
    assert_eq!(err.to_string(), "unreadable ZIP archive");
    assert!(err.source().is_some(), "the ZIP error is the source");
}

#[test]
fn legacy_and_encrypted_documents_are_rejected() {
    // Both .doc files and password-protected .docx files are OLE compound
    // files, not ZIP archives.
    let mut ole = vec![0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
    ole.resize(4096, 0);
    let err = Docx::from_bytes(ole).unwrap_err();
    assert!(matches!(err, InvalidDocx::Zip(_)), "{err:?}");
}

#[test]
fn empty_file_is_rejected() {
    let err = Docx::from_bytes(Vec::new()).unwrap_err();
    assert!(matches!(err, InvalidDocx::Zip(_)), "{err:?}");
}

#[test]
fn spreadsheet_is_not_wordprocessingml() {
    let fixture = Fixture::new([
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        (
            "_rels/.rels",
            concat!(
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
                r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>"#,
                "</Relationships>",
            )
            .as_bytes(),
        ),
        (
            "xl/workbook.xml",
            concat!(
                r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main""#,
                r#" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
                r#"<sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            )
            .as_bytes(),
        ),
        ("xl/_rels/workbook.xml.rels", b"not even XML".as_slice()),
    ]);
    let err = rejection(&fixture);
    assert!(matches!(err, InvalidDocx::NotWordprocessingMl), "{err:?}");
    assert_eq!(err.to_string(), "not a WordprocessingML document");
}

#[test]
fn strict_documents_are_not_supported() {
    // Strict OOXML names the main relationship differently...
    let strict_relationship = with_package_relationships(concat!(
        r#"<Relationship Id="rId1" Type="http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument""#,
        r#" Target="word/document.xml"/>"#,
    ));
    let err = rejection(&strict_relationship);
    assert!(matches!(err, InvalidDocx::NotWordprocessingMl), "{err:?}");

    // ... and puts `w:document` in its own namespace.
    let strict_namespace = Fixture::template().with(
        "word/document.xml",
        "<w:document xmlns:w=\"http://purl.oclc.org/ooxml/wordprocessingml/main\"><w:body/></w:document>",
    );
    let err = rejection(&strict_namespace);
    assert!(matches!(err, InvalidDocx::NotWordprocessingMl), "{err:?}");
}

#[test]
fn package_without_main_part_is_not_wordprocessingml() {
    let only_properties = with_package_relationships(concat!(
        r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties""#,
        r#" Target="docProps/core.xml"/>"#,
    ));
    let err = rejection(&only_properties);
    assert!(matches!(err, InvalidDocx::NotWordprocessingMl), "{err:?}");

    let external = with_package_relationships(concat!(
        r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument""#,
        r#" Target="https://example.com/word/document.xml" TargetMode="External"/>"#,
    ));
    let err = rejection(&external);
    assert!(matches!(err, InvalidDocx::NotWordprocessingMl), "{err:?}");
}

#[test]
fn main_part_must_be_a_word_document() {
    let header_as_main = Fixture::template().with("word/document.xml", common::HEADER);
    let err = rejection(&header_as_main);
    assert!(matches!(err, InvalidDocx::NotWordprocessingMl), "{err:?}");

    let relationships_as_main = with_package_relationships(concat!(
        r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument""#,
        r#" Target="_rels/.rels"/>"#,
    ));
    let err = rejection(&relationships_as_main);
    assert!(matches!(err, InvalidDocx::NotWordprocessingMl), "{err:?}");
}

// ── Missing parts ────────────────────────────────────────────────────────────

#[test]
fn missing_content_types_part_is_reported() {
    let err = rejection(&Fixture::template().without("[Content_Types].xml"));
    assert_missing(&err, "[Content_Types].xml");
}

#[test]
fn missing_package_relationships_are_reported() {
    let err = rejection(&Fixture::template().without("_rels/.rels"));
    assert_missing(&err, "_rels/.rels");
}

#[test]
fn missing_main_part_is_reported() {
    let err = rejection(&Fixture::template().without("word/document.xml"));
    assert_missing(&err, "word/document.xml");
}

#[test]
fn missing_story_parts_are_reported() {
    let err = rejection(&Fixture::template().without("word/header1.xml"));
    assert_missing(&err, "word/header1.xml");
    let err = rejection(&Fixture::template().without("word/endnotes.xml"));
    assert_missing(&err, "word/endnotes.xml");
}

#[test]
fn parts_are_looked_up_ignoring_case() {
    let fixture = Fixture::template()
        .without("word/document.xml")
        .with("Word/Document.XML", DOCUMENT);
    Docx::from_bytes(fixture.zip()).unwrap();
}

#[test]
fn missing_part_names_are_escaped_in_messages() {
    let fixture = with_package_relationships(concat!(
        r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument""#,
        r#" Target="word/doc&#10;ument.xml"/>"#,
    ));
    let err = rejection(&fixture);
    assert!(
        matches!(&err, InvalidDocx::MissingPart(part) if part == "word/doc\nument.xml"),
        "{err:?}"
    );
    assert_eq!(err.to_string(), "missing part `word/doc\\nument.xml`");
}

// ── Malformed parts ──────────────────────────────────────────────────────────

#[test]
fn malformed_main_part_is_named() {
    let truncated = &DOCUMENT[..DOCUMENT.len() / 2];
    let err = rejection(&Fixture::template().with("word/document.xml", truncated));
    assert_malformed(&err, "word/document.xml");
}

#[test]
fn malformed_story_parts_are_named() {
    let err = rejection(&Fixture::template().with("word/footer1.xml", "<w:ftr>"));
    assert_malformed(&err, "word/footer1.xml");
    let unbound_prefix = "<w:footnotes><w:footnote w:id=\"1\"/></w:footnotes>";
    let err = rejection(&Fixture::template().with("word/footnotes.xml", unbound_prefix));
    assert_malformed(&err, "word/footnotes.xml");
}

#[test]
fn malformed_relationships_are_named() {
    let err =
        rejection(&Fixture::template().with("word/_rels/footer1.xml.rels", "<Relationships>"));
    assert_malformed(&err, "word/_rels/footer1.xml.rels");
    let err = rejection(&Fixture::template().with("_rels/.rels", PACKAGE_RELS.replace("/>", ">")));
    assert_malformed(&err, "_rels/.rels");
}

#[test]
fn duplicate_attributes_are_malformed() {
    // A replacement that keeps the second `xml:space` writes it twice.
    let document = DOCUMENT.replacen(
        "<w:t xml:space=\"preserve\">Dear </w:t>",
        "<w:t xml:space=\"default\" xml:space=\"preserve\">Dear </w:t>",
        1,
    );
    assert_ne!(document, DOCUMENT);
    let err = rejection(&Fixture::template().with("word/document.xml", document));
    assert_malformed(&err, "word/document.xml");

    // Readers would disagree on which part a relationship points to.
    let settings = r#"Target="settings.xml""#;
    for doubled in [
        r#"Target="settings.xml" Target="styles.xml""#.to_owned(),
        format!(
            r#"{settings} Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles""#
        ),
    ] {
        let rels = DOCUMENT_RELS.replace(settings, &doubled);
        assert_ne!(rels, DOCUMENT_RELS);
        let err = rejection(&Fixture::template().with("word/_rels/document.xml.rels", rels));
        assert_malformed(&err, "word/_rels/document.xml.rels");
    }
}

#[test]
fn malformed_comments_are_named() {
    // Comments are only read for the ids of their pictures, but a part
    // that is read must be well-formed.
    let rels = DOCUMENT_RELS.replace(
        "</Relationships>",
        &format!("{}</Relationships>", common::COMMENTS_AND_GLOSSARY_RELS),
    );
    let fixture = Fixture::template()
        .with("word/_rels/document.xml.rels", rels)
        .with("word/comments.xml", "<w:comments>");
    let err = rejection(&fixture);
    assert_malformed(&err, "word/comments.xml");
}

#[test]
fn malformed_content_types_part_is_named() {
    let err = rejection(&Fixture::template().with("[Content_Types].xml", "<Types></Default>"));
    assert_malformed(&err, "[Content_Types].xml");
}

#[test]
fn unsupported_xml_is_reported_as_malformed() {
    let doctype = DOCUMENT.replacen(
        "<w:document",
        "<!DOCTYPE w:document [<!ENTITY x \"y\">]><w:document",
        1,
    );
    let err = rejection(&Fixture::template().with("word/document.xml", doctype));
    assert_malformed(&err, "word/document.xml");

    let mut utf16 = vec![0xff, 0xfe];
    utf16.extend(DOCUMENT.encode_utf16().flat_map(u16::to_le_bytes));
    let err = rejection(&Fixture::template().with("word/document.xml", utf16));
    assert_malformed(&err, "word/document.xml");

    let deep = minimal_document(&"<w:sdt><w:sdtContent>".repeat(300));
    let err = rejection(&Fixture::template().with("word/document.xml", deep));
    assert_malformed(&err, "word/document.xml");
}

#[test]
fn corrupt_compressed_part_is_a_zip_error() {
    // Damage the deflated bytes of the main part: the CRC-32 check (or the
    // decompressor) catches it, and the cause is a ZIP error.
    let mut zip = Fixture::template().zip();
    let start = {
        let mut archive = zip::ZipArchive::new(Cursor::new(&zip)).unwrap();
        let file = archive.by_name("word/document.xml").unwrap();
        usize::try_from(file.data_start().unwrap()).unwrap()
    };
    zip[start + 40] ^= 0x55;
    let err = Docx::from_bytes(zip).unwrap_err();
    assert!(matches!(err, InvalidDocx::Zip(_)), "{err:?}");
}

// ── Damaged input ────────────────────────────────────────────────────────────

#[test]
fn truncated_files_are_rejected() {
    let zip = Fixture::template().zip();
    for len in 0..zip.len() {
        if let Ok(docx) = Docx::from_bytes(zip[..len].to_vec()) {
            panic!("a {len}-byte prefix opened: {docx:?}");
        }
    }
}

#[test]
fn damaged_files_never_panic() {
    // Deterministic byte flips all over the archive: whatever opens must
    // also write a readable archive.
    let zip = Fixture::template().zip();
    for (position, mask) in (0..zip.len())
        .step_by(7)
        .zip([0x01, 0x80, 0xff, 0x20].iter().cycle())
    {
        let mut damaged = zip.clone();
        damaged[position] ^= mask;
        if let Ok(docx) = Docx::from_bytes(damaged) {
            let output = docx.to_bytes();
            zip::ZipArchive::new(Cursor::new(output)).unwrap();
        }
    }
}
