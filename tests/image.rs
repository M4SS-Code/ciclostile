//! `Docx::replace_with_image` end to end: the picture replaces its
//! placeholders in every story, its media part is added once per call with
//! the relationships and content type it needs, and drawing ids stay
//! unique.

pub mod common;

use ciclostile::{Docx, Image, Length, error::InvalidImage};
use common::{CONTENT_TYPES, DOCUMENT_RELS, FOOTER_RELS, Fixture, JPEG, PNG};

const IMAGE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";

/// The PNG fixture, 3 cm wide, with alternative text.
fn png() -> Image {
    Image::from_bytes(PNG)
        .unwrap()
        .width(Length::from_cm(3.0))
        .alt_text("Logo")
}

/// The JPEG fixture at its pixel size.
fn jpeg() -> Image {
    Image::from_bytes(JPEG).unwrap()
}

/// The image relationship `id` to `target`, as written.
fn relationship(id: &str, target: &str) -> String {
    format!(r#"<Relationship Id="{id}" Type="{IMAGE}" Target="{target}"/>"#)
}

/// `rels` with `relationship` appended.
fn appended(rels: &str, relationship: &str) -> String {
    rels.replace(
        "</Relationships>",
        &format!("{relationship}</Relationships>"),
    )
}

#[test]
fn image_replaces_its_placeholders_in_every_story() {
    let template = Fixture::template().zip();
    let baseline = common::untouched(&template);
    let mut docx = Docx::from_bytes(template).unwrap();
    assert_eq!(docx.replace_with_image("{{logo}}", &png()), 2);
    let output = docx.to_bytes();

    // The body and the header change. The header had no relationships
    // part and gets one; the file is added once, as the smallest free
    // `imageN.png`. The content types already map `png` and `rels`.
    let added = common::assert_kept_except(
        &baseline,
        &output,
        &[
            "word/document.xml",
            "word/_rels/document.xml.rels",
            "word/header1.xml",
        ],
    );
    assert_eq!(
        added,
        ["word/_rels/header1.xml.rels", "word/media/image2.png"]
    );
    assert_eq!(common::read(&output, "word/media/image2.png"), PNG);

    // The main part's relationship continues its numbering after rId10.
    assert_eq!(
        common::read_text(&output, "word/_rels/document.xml.rels"),
        appended(DOCUMENT_RELS, &relationship("rId11", "media/image2.png"))
    );
    assert_eq!(
        common::read_text(&output, "word/_rels/header1.xml.rels"),
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image""#,
            r#" Target="media/image2.png"/></Relationships>"#,
        )
    );

    // Each drawing refers to its story's relationship, at the requested
    // size: 3 cm wide, 2 cm high like the 3x2 pixels.
    let document = common::read_text(&output, "word/document.xml");
    let header = common::read_text(&output, "word/header1.xml");
    assert!(!document.contains("{{logo}}") && !header.contains("logo}}"));
    assert_eq!(common::count(&document, r#"<a:blip r:embed="rId11"/>"#), 1);
    assert_eq!(common::count(&header, r#"<a:blip r:embed="rId1"/>"#), 1);
    for xml in [&document, &header] {
        assert_eq!(
            common::count(xml, r#"<wp:extent cx="1080000" cy="720000"/>"#),
            1
        );
        assert_eq!(common::count(xml, r#" descr="Logo"/>"#), 1);
    }

    // `wp:docPr` ids must be unique in the whole document: the template
    // uses 1 and 2.
    let mut ids = common::drawing_ids(&document);
    ids.extend(common::drawing_ids(&header));
    ids.sort_unstable();
    assert_eq!(ids, [1, 2, 3, 4]);

    // The output opens again, with the image in place.
    Docx::from_bytes(output).unwrap();
}

#[test]
fn image_in_a_story_with_relationships_gets_the_next_id() {
    let template = Fixture::template().zip();
    let baseline = common::untouched(&template);
    let mut docx = Docx::from_bytes(template).unwrap();
    assert_eq!(docx.replace_with_image("{{signature}}", &jpeg()), 1);
    let output = docx.to_bytes();

    // The footer already refers to a hyperlink as rId1. JPEG is new to the
    // content types, which gain a default after the last one.
    let added = common::assert_kept_except(
        &baseline,
        &output,
        &[
            "[Content_Types].xml",
            "word/footer1.xml",
            "word/_rels/footer1.xml.rels",
        ],
    );
    assert_eq!(added, ["word/media/image1.jpeg"]);
    assert_eq!(
        common::read_text(&output, "word/_rels/footer1.xml.rels"),
        appended(FOOTER_RELS, &relationship("rId2", "media/image1.jpeg"))
    );
    assert_eq!(
        common::read_text(&output, "[Content_Types].xml"),
        CONTENT_TYPES.replace(
            r#"<Default Extension="xml" ContentType="application/xml"/>"#,
            concat!(
                r#"<Default Extension="xml" ContentType="application/xml"/>"#,
                r#"<Default Extension="jpeg" ContentType="image/jpeg"/>"#,
            ),
        )
    );

    let footer = common::read_text(&output, "word/footer1.xml");
    assert_eq!(common::count(&footer, r#"<a:blip r:embed="rId2"/>"#), 1);
    assert_eq!(common::drawing_ids(&footer), [3]);
    // At its pixel size: 3x2 pixels at 9,525 EMU each.
    assert_eq!(
        common::count(&footer, r#"<wp:extent cx="28575" cy="19050"/>"#),
        1
    );
}

#[test]
fn each_call_adds_its_own_media_part() {
    let template = Fixture::template().zip();
    let baseline = common::untouched(&template);
    let mut docx = Docx::from_bytes(template).unwrap();
    assert_eq!(docx.replace_with_image("{{logo}}", &png()), 2);
    assert_eq!(docx.replace_with_image("{{signature}}", &png()), 1);
    let output = docx.to_bytes();

    let added = common::assert_kept_except(
        &baseline,
        &output,
        &[
            "word/document.xml",
            "word/_rels/document.xml.rels",
            "word/header1.xml",
            "word/footer1.xml",
            "word/_rels/footer1.xml.rels",
        ],
    );
    assert_eq!(
        added,
        [
            "word/_rels/header1.xml.rels",
            "word/media/image2.png",
            "word/media/image3.png"
        ]
    );
    assert_eq!(
        common::read_text(&output, "word/_rels/footer1.xml.rels"),
        appended(FOOTER_RELS, &relationship("rId2", "media/image3.png"))
    );

    let mut ids: Vec<u32> = ["word/document.xml", "word/header1.xml", "word/footer1.xml"]
        .into_iter()
        .flat_map(|name| common::drawing_ids(&common::read_text(&output, name)))
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, [1, 2, 3, 4, 5]);
}

#[test]
fn media_names_skip_taken_names_ignoring_case() {
    let fixture = Fixture::template()
        .with("word/media/IMAGE2.PNG", PNG)
        .with("word/media/image3.png", PNG);
    let template = fixture.zip();
    let baseline = common::untouched(&template);
    let mut docx = Docx::from_bytes(template).unwrap();
    assert_eq!(docx.replace_with_image("{{signature}}", &png()), 1);
    let added = common::assert_kept_except(
        &baseline,
        &docx.to_bytes(),
        &["word/footer1.xml", "word/_rels/footer1.xml.rels"],
    );
    assert_eq!(added, ["word/media/image4.png"]);
}

#[test]
fn missing_placeholder_adds_nothing() {
    let template = Fixture::template().zip();
    let baseline = common::untouched(&template);
    let mut docx = Docx::from_bytes(template).unwrap();
    assert_eq!(docx.replace_with_image("{{nowhere}}", &jpeg()), 0);
    assert_eq!(docx.to_bytes(), baseline);
    // The media name was not used up either.
    assert_eq!(docx.replace_with_image("{{signature}}", &jpeg()), 1);
    assert!(common::contains(&docx.to_bytes(), "word/media/image1.jpeg"));
}

#[test]
fn empty_placeholder_matches_nothing() {
    let template = Fixture::template().zip();
    let baseline = common::untouched(&template);
    let mut docx = Docx::from_bytes(template).unwrap();
    assert_eq!(docx.replace_with_image("", &png()), 0);
    assert_eq!(docx.to_bytes(), baseline);
}

#[test]
fn new_drawings_skip_ids_used_by_comments_and_the_glossary() {
    // Neither part is searched, and both stay as they are, but their
    // pictures take `wp:docPr` ids 3 and 4, which must not be given again.
    let fixture = Fixture::template()
        .with(
            "word/_rels/document.xml.rels",
            appended(DOCUMENT_RELS, common::COMMENTS_AND_GLOSSARY_RELS),
        )
        .with("word/comments.xml", common::COMMENTS)
        .with("word/glossary/document.xml", common::GLOSSARY);
    let template = fixture.zip();
    let baseline = common::untouched(&template);
    let mut docx = Docx::from_bytes(template).unwrap();
    assert_eq!(docx.replace_with_image("{{logo}}", &png()), 2);
    let output = docx.to_bytes();

    let added = common::assert_kept_except(
        &baseline,
        &output,
        &[
            "word/document.xml",
            "word/_rels/document.xml.rels",
            "word/header1.xml",
        ],
    );
    assert_eq!(
        added,
        ["word/_rels/header1.xml.rels", "word/media/image2.png"]
    );
    let mut ids: Vec<u32> = ["word/document.xml", "word/header1.xml"]
        .into_iter()
        .flat_map(|name| common::drawing_ids(&common::read_text(&output, name)))
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, [1, 2, 5, 6]);
}

#[test]
fn missing_comments_and_glossary_are_skipped() {
    // They are never edited, so a dangling relationship doesn't matter.
    let fixture = Fixture::template().with(
        "word/_rels/document.xml.rels",
        appended(DOCUMENT_RELS, common::COMMENTS_AND_GLOSSARY_RELS),
    );
    let mut docx = Docx::from_bytes(fixture.zip()).unwrap();
    assert_eq!(docx.replace_with_image("{{signature}}", &png()), 1);
    let footer = common::read_text(&docx.to_bytes(), "word/footer1.xml");
    assert_eq!(common::drawing_ids(&footer), [3]);
}

#[test]
fn gif_files_are_unsupported() {
    // img-parts, which reads the images, has no GIF support: convert them
    // to PNG first.
    let gif = b"GIF89a\x03\x00\x02\x00\x80\x00\x00\xff\x00\x00\x00\x00\xff\x3b";
    assert!(matches!(
        Image::from_bytes(&gif[..]),
        Err(InvalidImage::UnsupportedFormat)
    ));
}

#[test]
fn media_follow_the_main_part_and_targets_are_relative() {
    // The main part lives in `content/`, a footer in `shared/`: media go
    // next to the main part, and each story refers to them relative to
    // its own directory.
    let main = common::DOCUMENT.replace(
        r#"<w:footerReference w:type="default" r:id="rId10"/>"#,
        r#"<w:footerReference w:type="default" r:id="rId2"/>"#,
    );
    let fixture = Fixture::new([("[Content_Types].xml", CONTENT_TYPES.as_bytes())])
        .with(
            "_rels/.rels",
            common::PACKAGE_RELS.replace("word/document.xml", "content/main.xml"),
        )
        .with("content/main.xml", main)
        .with(
            "content/_rels/main.xml.rels",
            concat!(
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
                r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="../shared/footer.xml"/>"#,
                "</Relationships>",
            ),
        )
        .with("shared/footer.xml", common::FOOTER);
    let mut docx = Docx::from_bytes(fixture.zip()).unwrap();
    assert_eq!(docx.replace_with_image("{{logo}}", &png()), 1);
    assert_eq!(docx.replace_with_image("{{signature}}", &png()), 1);
    let output = docx.to_bytes();

    assert!(common::contains(&output, "content/media/image1.png"));
    assert!(common::contains(&output, "content/media/image2.png"));
    assert!(
        common::read_text(&output, "content/_rels/main.xml.rels")
            .contains(&relationship("rId3", "media/image1.png"))
    );
    assert!(
        common::read_text(&output, "shared/_rels/footer.xml.rels")
            .contains(&relationship("rId1", "../content/media/image2.png"))
    );
}
