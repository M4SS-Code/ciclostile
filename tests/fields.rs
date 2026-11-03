//! `Docx::update_fields_on_open`: `w:updateFields` is switched on in the
//! document settings, where the schema puts it, and a settings part is
//! created when the document has none. Documents that don't ask for it are
//! left byte for byte as they were.

pub mod common;

use ciclostile::Docx;
use common::{CONTENT_TYPES, DOCUMENT_RELS, Fixture, WORD_SETTINGS};

/// The element the method adds.
const ON: &str = r#"<w:updateFields w:val="true"/>"#;

/// The settings relationship of the template, rId2.
const SETTINGS_RELATIONSHIP: &str = r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings" Target="settings.xml"/>"#;

/// The settings override of the template's content types.
const SETTINGS_OVERRIDE: &str = r#"<Override PartName="/word/settings.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml"/>"#;

/// What a created settings part holds.
const CREATED: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n",
    r#"<w:settings xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
    r#"<w:updateFields w:val="true"/></w:settings>"#,
);

/// Open `fixture`, call `update_fields_on_open` `times` times, and return
/// what ciclostile writes for the fixture untouched and what it writes then.
fn update(fixture: &Fixture, times: usize) -> (Vec<u8>, Vec<u8>) {
    let template = fixture.zip();
    let mut docx = Docx::from_bytes(template.clone()).unwrap();
    for _ in 0..times {
        docx.update_fields_on_open();
    }
    (common::untouched(&template), docx.to_bytes().to_vec())
}

/// Return the settings part that `update_fields_on_open` makes of
/// `settings`, checking that nothing else changes.
fn updated_settings(settings: &str) -> String {
    let fixture = Fixture::template().with("word/settings.xml", settings);
    let (baseline, output) = update(&fixture, 1);
    let added = common::assert_kept_except(&baseline, &output, &["word/settings.xml"]);
    assert_eq!(added, [] as [String; 0]);
    common::read_text(&output, "word/settings.xml")
}

#[test]
fn update_fields_goes_before_compat_in_word_settings() {
    assert_eq!(
        updated_settings(WORD_SETTINGS),
        WORD_SETTINGS.replace("<w:compat>", &format!("{ON}<w:compat>"))
    );
}

#[test]
fn update_fields_goes_before_the_first_child_ordered_after_it() {
    // Without `w:compat` and `w:rsids`, math properties come next.
    let compat = WORD_SETTINGS.find("<w:compat>").unwrap();
    let math = WORD_SETTINGS.find("<m:mathPr>").unwrap();
    let without_compat = [&WORD_SETTINGS[..compat], &WORD_SETTINGS[math..]].concat();
    assert_eq!(
        updated_settings(&without_compat),
        without_compat.replace("<m:mathPr>", &format!("{ON}<m:mathPr>"))
    );

    // Word's extensions count, whatever their namespace.
    let only_extensions = concat!(
        r#"<w:settings xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#,
        r#" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml">"#,
        r#"<w:zoom w:percent="100"/><w14:docId w14:val="1"/></w:settings>"#,
    );
    assert_eq!(
        updated_settings(only_extensions),
        only_extensions.replace("<w14:docId", &format!("{ON}<w14:docId"))
    );

    // The template's settings start the later children with the footnote
    // properties; LibreOffice writes nothing after `w:compat`.
    assert_eq!(
        updated_settings(common::SETTINGS),
        common::SETTINGS.replace("<w:footnotePr>", &format!("{ON}<w:footnotePr>"))
    );

    // With nothing ordered after it, it comes last.
    let early_only = concat!(
        r#"<w:settings xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
        r#"<w:zoom w:percent="100"/></w:settings>"#,
    );
    assert_eq!(
        updated_settings(early_only),
        early_only.replace("</w:settings>", &format!("{ON}</w:settings>"))
    );
}

#[test]
fn update_fields_takes_the_prefix_of_the_settings() {
    let prefixed = concat!(
        r#"<ns0:settings xmlns:ns0="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
        r#"<ns0:compat/></ns0:settings>"#,
    );
    assert_eq!(
        updated_settings(prefixed),
        prefixed.replace(
            "<ns0:compat/>",
            r#"<ns0:updateFields ns0:val="true"/><ns0:compat/>"#
        )
    );

    // Attributes have no default namespace: `w` is declared for `w:val`.
    let unprefixed = concat!(
        r#"<settings xmlns="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
        "<compat/></settings>",
    );
    assert_eq!(
        updated_settings(unprefixed),
        unprefixed.replace("<compat/>", &format!("{DECLARED}<compat/>"))
    );
}

/// The element added under a settings root without a prefix.
const DECLARED: &str = concat!(
    r#"<w:updateFields xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#,
    r#" w:val="true"/>"#,
);

/// A settings part with the root `{root}`, which is `x:settings` with the
/// namespace declarations given, holding `children`.
fn settings_with(root: &str, children: &str) -> String {
    format!("<x:settings{root}>{children}</x:settings>")
}

/// The WordprocessingML namespace URI.
const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

#[test]
fn update_fields_under_another_prefix_is_replaced() {
    // `w` and `x` are both bound to WordprocessingML, but the tree doesn't
    // resolve attribute prefixes: only the root's prefix is trusted.
    let local = format!(r#" xmlns:x="{W}""#);
    let both = format!(r#" xmlns:x="{W}" xmlns:w="{W}""#);
    for (root, update) in [
        (
            &local,
            format!(r#"<x:updateFields xmlns:w="{W}" w:val="false"/>"#),
        ),
        (&both, r#"<x:updateFields w:val="false"/>"#.to_owned()),
        (&both, r#"<w:updateFields w:val="true"/>"#.to_owned()),
        (
            &both,
            r#"<x:updateFields x:val="true" w:val="false"/>"#.to_owned(),
        ),
    ] {
        let settings = settings_with(root, &format!("<x:zoom/>{update}<x:compat/>"));
        assert_eq!(
            updated_settings(&settings),
            settings.replace(&update, r#"<x:updateFields x:val="true"/>"#),
            "{update}"
        );
    }
}

#[test]
fn several_update_fields_become_one() {
    // Where the first one was, switched on.
    for children in [
        r#"<w:updateFields w:val="0"/><w:updateFields w:val="off"/>"#,
        r#"<w:updateFields w:val="true"/><w:zoom/><w:updateFields w:val="false"/>"#,
    ] {
        let settings = format!(r#"<w:settings xmlns:w="{W}">{children}<w:compat/></w:settings>"#);
        let updated = updated_settings(&settings);
        assert_eq!(common::count(&updated, "updateFields"), 1, "{updated}");
        assert!(
            updated.starts_with(&format!(r#"<w:settings xmlns:w="{W}">{ON}"#)),
            "{updated}"
        );
    }
}

#[test]
fn default_namespace_settings_are_handled() {
    // An unprefixed element without a value is on, and so is the element
    // the method writes: both are left alone.
    for on in ["<updateFields/>", DECLARED] {
        let settings = format!(r#"<settings xmlns="{W}"><zoom/>{on}<compat/></settings>"#);
        let fixture = Fixture::template().with("word/settings.xml", &settings);
        let (baseline, output) = update(&fixture, 1);
        assert_eq!(output, baseline, "{on}");
    }

    // Anything else is replaced by an element that declares `w`.
    for update in [
        r#"<updateFields val="true"/>"#.to_owned(),
        format!(r#"<updateFields xmlns:w="{W}" w:val="false"/>"#),
        format!(r#"<w:updateFields xmlns:w="{W}" w:val="1"/>"#),
    ] {
        let settings = format!(r#"<settings xmlns="{W}"><zoom/>{update}<compat/></settings>"#);
        assert_eq!(
            updated_settings(&settings),
            settings.replace(&update, DECLARED),
            "{update}"
        );
    }
}

#[test]
fn update_fields_that_is_off_is_switched_on_in_place() {
    for off in ["false", "0", "off"] {
        let settings = WORD_SETTINGS.replace(
            "<w:compat>",
            &format!(r#"<w:updateFields w:val="{off}"/><w:compat>"#),
        );
        assert_eq!(
            updated_settings(&settings),
            WORD_SETTINGS.replace("<w:compat>", &format!("{ON}<w:compat>")),
            "{off}"
        );
    }
}

#[test]
fn update_fields_that_is_on_is_left_alone() {
    for on in [ON, r#"<w:updateFields w:val="1"/>"#, "<w:updateFields/>"] {
        let settings = WORD_SETTINGS.replace("<w:compat>", &format!("{on}<w:compat>"));
        let fixture = Fixture::template().with("word/settings.xml", &settings);
        let (baseline, output) = update(&fixture, 1);
        assert_eq!(output, baseline, "{on}");
    }
}

#[test]
fn switching_on_twice_is_switching_on_once() {
    let fixture = Fixture::template().with("word/settings.xml", WORD_SETTINGS);
    let (_, once) = update(&fixture, 1);
    let (_, twice) = update(&fixture, 2);
    assert_eq!(twice, once);
    let settings = common::read_text(&twice, "word/settings.xml");
    assert_eq!(common::count(&settings, "updateFields"), 1);

    // A document that was switched on stays as it is.
    let mut reopened = Docx::from_bytes(once.clone()).unwrap();
    reopened.update_fields_on_open();
    assert_eq!(reopened.to_bytes().to_vec(), common::untouched(&once));
}

#[test]
fn missing_settings_part_is_created() {
    // Like python-docx's `set-no-settings-part.docx`: no part, no
    // relationship, no override.
    let fixture = Fixture::template()
        .without("word/settings.xml")
        .with(
            "word/_rels/document.xml.rels",
            DOCUMENT_RELS.replace(SETTINGS_RELATIONSHIP, ""),
        )
        .with(
            "[Content_Types].xml",
            CONTENT_TYPES.replace(SETTINGS_OVERRIDE, ""),
        );
    let (baseline, output) = update(&fixture, 1);

    let added = common::assert_kept_except(
        &baseline,
        &output,
        &["[Content_Types].xml", "word/_rels/document.xml.rels"],
    );
    assert_eq!(added, ["word/settings.xml"]);
    assert_eq!(common::read_text(&output, "word/settings.xml"), CREATED);
    assert!(common::read_text(&output, "word/_rels/document.xml.rels").ends_with(concat!(
        r#"<Relationship Id="rId11" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings""#,
        r#" Target="settings.xml"/></Relationships>"#,
    )));
    assert!(
        common::read_text(&output, "[Content_Types].xml")
            .ends_with(&format!("{SETTINGS_OVERRIDE}</Types>"))
    );

    // The new part is found again.
    let mut reopened = Docx::from_bytes(output.clone()).unwrap();
    reopened.update_fields_on_open();
    assert_eq!(reopened.to_bytes().to_vec(), common::untouched(&output));
}

#[test]
fn dangling_settings_relationship_gets_its_part() {
    // The template's relationship and override stay: only the part is new.
    let fixture = Fixture::template().without("word/settings.xml");
    let (baseline, output) = update(&fixture, 1);
    let added = common::assert_kept_except(&baseline, &output, &[]);
    assert_eq!(added, ["word/settings.xml"]);
    assert_eq!(common::read_text(&output, "word/settings.xml"), CREATED);

    // Wherever the relationship points.
    let fixture = Fixture::template().without("word/settings.xml").with(
        "word/_rels/document.xml.rels",
        DOCUMENT_RELS.replace(r#"Target="settings.xml""#, r#"Target="config/options.xml""#),
    );
    let (baseline, output) = update(&fixture, 1);
    let added = common::assert_kept_except(&baseline, &output, &["[Content_Types].xml"]);
    assert_eq!(added, ["word/config/options.xml"]);
    assert!(
        common::read_text(&output, "[Content_Types].xml").contains(
            r#"<Override PartName="/word/config/options.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml"/>"#
        )
    );
    Docx::from_bytes(output).unwrap();
}

#[test]
fn created_settings_part_takes_a_free_name() {
    // `word/settings.xml` exists, but nothing refers to it as settings.
    let fixture = Fixture::template().with(
        "word/_rels/document.xml.rels",
        DOCUMENT_RELS.replace(SETTINGS_RELATIONSHIP, ""),
    );
    let (baseline, output) = update(&fixture, 1);
    let added = common::assert_kept_except(
        &baseline,
        &output,
        &["[Content_Types].xml", "word/_rels/document.xml.rels"],
    );
    assert_eq!(added, ["word/settings1.xml"]);
    assert!(
        common::read_text(&output, "word/_rels/document.xml.rels")
            .contains(r#"Target="settings1.xml"/>"#)
    );
}

#[test]
fn settings_relationship_to_another_kind_of_part_is_left_alone() {
    for target in ["document.xml", "styles.xml", "_rels/document.xml.rels"] {
        let fixture = Fixture::template().with(
            "word/_rels/document.xml.rels",
            DOCUMENT_RELS.replace(r#"Target="settings.xml""#, &format!(r#"Target="{target}""#)),
        );
        let (baseline, output) = update(&fixture, 1);
        assert_eq!(output, baseline, "{target}");
    }
}

#[test]
fn settings_stay_byte_for_byte_without_the_call() {
    let fixture = Fixture::template().with("word/settings.xml", WORD_SETTINGS);
    let template = fixture.zip();
    let mut docx = Docx::from_bytes(template.clone()).unwrap();
    docx.replace_text([("{{customer}}", "Ada")]);
    let output = docx.to_bytes().to_vec();
    let added = common::assert_kept_except(
        &common::untouched(&template),
        &output,
        &["word/document.xml"],
    );
    assert_eq!(added, [] as [String; 0]);
    assert_eq!(
        common::read_text(&output, "word/settings.xml"),
        WORD_SETTINGS
    );
    common::assert_copied(&template, &common::untouched(&template));
}
