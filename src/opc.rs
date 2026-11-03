//! Open Packaging Conventions (ECMA-376 Part 2) helpers: part names,
//! relationships parts and the content types part.
//!
//! Part names are written the way [`Package`](crate::package::Package)
//! stores them, without the leading `/` (`word/document.xml`), and compared
//! ignoring ASCII case (ECMA-376 Part 2, M1.12).
//!
//! Relationships parts and the content types part are edited as parsed
//! trees: new elements are appended to them, everything else is left as
//! written. Their children are matched in the namespace of their root,
//! and new children are created in it, so elements read and elements added
//! always agree, even in a part whose root is in an unexpected namespace.

use std::{borrow::Cow, collections::BTreeSet};

use crate::xml::{Element, Node, Ns, RELS_URI, XmlDocument, qualified_name};

// ── Names ────────────────────────────────────────────────────────────────────

/// Name of the content types part, which maps every part to its media type.
pub(crate) const CONTENT_TYPES: &str = "[Content_Types].xml";

/// Name of the relationships part of the package itself, which points to
/// the main document part.
pub(crate) const PACKAGE_RELS: &str = "_rels/.rels";

/// Media type of relationships parts.
pub(crate) const RELS_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-package.relationships+xml";

/// Return the directory of `part`, without a trailing `/`: `word` for
/// `word/document.xml`, and the empty string for a part at the root of the
/// package.
pub(crate) fn directory(part: &str) -> &str {
    part.rsplit_once('/').map_or("", |(directory, _)| directory)
}

/// Return `path`, a relative path, in `directory`, a directory as
/// [`directory`] returns it: `{directory}/{path}`, or `path` itself at the
/// root of the package.
pub(crate) fn join(directory: &str, path: &str) -> String {
    if directory.is_empty() {
        path.to_owned()
    } else {
        format!("{directory}/{path}")
    }
}

/// Return the name of the relationships part that holds the relationships
/// whose source is `part`: `word/_rels/document.xml.rels` for
/// `word/document.xml`.
pub(crate) fn rels_part_name(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((directory, file)) => format!("{directory}/_rels/{file}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

/// Return whether `name` has the shape of a relationships part name: a
/// `.rels` file in a `_rels` directory, ignoring ASCII case.
pub(crate) fn is_rels_part(name: &str) -> bool {
    let (directory, file) = name.rsplit_once('/').unwrap_or(("", name));
    let parent = directory
        .rsplit_once('/')
        .map_or(directory, |(_, last)| last);
    parent.eq_ignore_ascii_case("_rels") && ends_with_ignore_ascii_case(file, ".rels")
}

/// Resolve the `Target` of an internal relationship whose source is
/// `source` (a part name, or the empty string for the package itself) to
/// a part name.
///
/// The target is a relative reference resolved against the source part
/// name (RFC 3986, §5.2): a target that starts with `/` starts from the
/// root of the package, any other from the directory of the source; `.`
/// segments are dropped and `..` segments remove the segment before them,
/// never going above the root. Percent-encoded characters are not decoded:
/// Word and LibreOffice name their parts with characters that need no
/// encoding.
pub(crate) fn resolve_target(source: &str, target: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    let relative = if let Some(absolute) = target.strip_prefix('/') {
        absolute
    } else {
        segments.extend(directory(source).split('/').filter(|s| !s.is_empty()));
        target
    };
    for segment in relative.split('/') {
        match segment {
            "." => {}
            ".." => {
                segments.pop();
            }
            _ => segments.push(segment),
        }
    }
    segments.join("/")
}

/// Return the `Target` for a relationship from the part `source` to the
/// part `part`: a relative reference from the directory of `source`
/// (RFC 3986, §4.2), with the characters a URI path can't hold
/// percent-encoded.
///
/// Directories are compared exactly, not ignoring case, so the target
/// spells `part` exactly as given even for consumers that look parts up
/// case-sensitively.
pub(crate) fn relative_target(source: &str, part: &str) -> String {
    let from: Vec<&str> = directory(source)
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let to: Vec<&str> = part.split('/').collect();
    let to_directories = to.len().saturating_sub(1);
    let common = from
        .iter()
        .zip(to.iter().take(to_directories))
        .take_while(|(a, b)| a == b)
        .count();

    let mut target = "../".repeat(from.len().saturating_sub(common));
    target.push_str(&percent_encode(
        &to.get(common..).unwrap_or_default().join("/"),
    ));
    target
}

/// Percent-encode every byte of `path` that may not appear as is in the
/// path of a URI (RFC 3986, §3.3): anything but unreserved characters,
/// sub-delimiters, `@` and the `/` separators.
fn percent_encode(path: &str) -> Cow<'_, str> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let keep = |b: u8| b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=@/".contains(&b);
    if path.bytes().all(keep) {
        return Cow::Borrowed(path);
    }
    let mut encoded = String::with_capacity(path.len() * 3);
    for b in path.bytes() {
        if keep(b) {
            encoded.push(char::from(b));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(b >> 4)]));
            encoded.push(char::from(HEX[usize::from(b & 0x0f)]));
        }
    }
    Cow::Owned(encoded)
}

/// Return whether `s` ends with `suffix`, ignoring ASCII case.
fn ends_with_ignore_ascii_case(s: &str, suffix: &str) -> bool {
    s.len()
        .checked_sub(suffix.len())
        .and_then(|start| s.as_bytes().get(start..))
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix.as_bytes()))
}

// ── Relationships ────────────────────────────────────────────────────────────

/// A relationship read from a relationships part: a `Relationship`
/// element, with its attributes unescaped.
#[derive(Debug)]
pub(crate) struct Relationship<'a> {
    /// The `Id` attribute, unique within the relationships part.
    pub(crate) id: Cow<'a, str>,
    /// The `Type` attribute, a URI naming the role of the target.
    pub(crate) rel_type: Cow<'a, str>,
    /// The `Target` attribute: a URI reference to a part, or to an external
    /// resource.
    pub(crate) target: Cow<'a, str>,
    /// The `TargetMode` attribute: `External` for a target outside the
    /// package, `Internal` (the default) or absent otherwise.
    pub(crate) target_mode: Option<Cow<'a, str>>,
}

impl Relationship<'_> {
    /// Return whether the target is outside the package
    /// (`TargetMode="External"`), such as a hyperlink.
    pub(crate) fn is_external(&self) -> bool {
        self.target_mode.as_deref() == Some("External")
    }
}

/// Iterate over the relationships in `rels`, the root element of a
/// relationships part, in document order.
///
/// Children other than `Relationship`, and relationships without an `Id`,
/// a `Type` or a `Target`, are skipped.
pub(crate) fn relationships(rels: &Element) -> impl Iterator<Item = Relationship<'_>> {
    rels.child_elements()
        .filter(|element| element.is(rels.ns(), "Relationship"))
        .filter_map(|element| {
            Some(Relationship {
                id: element.attr("Id")?,
                rel_type: element.attr("Type")?,
                target: element.attr("Target")?,
                target_mode: element.attr("TargetMode"),
            })
        })
}

/// Return an id for a new relationship in `rels`, the root element of a
/// relationships part (`None` for a part that doesn't exist yet).
///
/// The id is `rId` followed by one more than the largest number among the
/// existing `rId<digits>` ids, continuing Word's own numbering, and
/// differs from every existing id even ignoring case.
pub(crate) fn fresh_relationship_id(rels: Option<&Element>) -> String {
    let taken: BTreeSet<String> = rels
        .into_iter()
        .flat_map(relationships)
        .map(|rel| rel.id.to_ascii_lowercase())
        .collect();
    let largest = taken
        .iter()
        .filter_map(|id| id.strip_prefix("rid"))
        .filter(|digits| digits.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|digits| digits.parse::<u64>().ok())
        .max();
    let mut number = largest.map_or(1, |largest| largest.saturating_add(1));
    // Only a number that saturated can be taken. Each candidate is new, so
    // this ends after at most `taken.len() + 1` tries.
    loop {
        let id = format!("rId{number}");
        if !taken.contains(&id.to_ascii_lowercase()) {
            return id;
        }
        number = number.wrapping_add(1);
    }
}

/// Append a relationship to `rels`, the root element of a relationships
/// part, written as Word writes it: `Id`, `Type`, then `Target`.
///
/// `id` must come from [`fresh_relationship_id`] and `target` must be a
/// relative reference to a part, such as one from [`relative_target`].
pub(crate) fn add_relationship(rels: &mut Element, id: &str, rel_type: &str, target: &str) {
    let mut relationship = Element::new(qualified_name(rels.prefix(), "Relationship"), rels.ns());
    relationship.set_attr("Id", id);
    relationship.set_attr("Type", rel_type);
    relationship.set_attr("Target", target);
    rels.children.push(Node::Element(relationship));
}

/// Return a new relationships part with no relationships.
pub(crate) fn new_relationships() -> XmlDocument {
    let mut root = Element::new("Relationships", Ns::Rels);
    root.set_attr("xmlns", RELS_URI);
    XmlDocument::new(root)
}

// ── Content types ────────────────────────────────────────────────────────────

/// Make sure `types`, the root element of the content types part, has a
/// `Default` element for the file extension `extension` (compared
/// ignoring ASCII case), adding one that maps it to `content_type` if not.
///
/// An existing `Default` is left alone, whatever its content type. A new
/// one goes right after the last `Default`, or first if there is none.
///
/// Returns whether `types` changed.
pub(crate) fn ensure_default(types: &mut Element, extension: &str, content_type: &str) -> bool {
    let exists = types.child_elements().any(|element| {
        element.is(types.ns(), "Default")
            && element
                .attr("Extension")
                .is_some_and(|existing| existing.eq_ignore_ascii_case(extension))
    });
    if exists {
        return false;
    }

    let position = types
        .children
        .iter()
        .rposition(|node| {
            node.as_element()
                .is_some_and(|element| element.is(types.ns(), "Default"))
        })
        .map_or(0, |last| last + 1);
    let mut default = Element::new(qualified_name(types.prefix(), "Default"), types.ns());
    default.set_attr("Extension", extension);
    default.set_attr("ContentType", content_type);
    types.children.insert(position, Node::Element(default));
    true
}

/// Make sure `types`, the root element of the content types part, has an
/// `Override` element for the part named `part` (compared ignoring ASCII
/// case), appending one that gives it `content_type` if not.
///
/// An existing `Override` is left alone, whatever its content type.
///
/// Returns whether `types` changed.
pub(crate) fn ensure_override(types: &mut Element, part: &str, content_type: &str) -> bool {
    // Part names are URIs: absolute, and percent-encoded where needed.
    let part_name = format!("/{}", percent_encode(part));
    let exists = types.child_elements().any(|element| {
        element.is(types.ns(), "Override")
            && element
                .attr("PartName")
                .is_some_and(|existing| existing.eq_ignore_ascii_case(&part_name))
    });
    if exists {
        return false;
    }

    let mut element = Element::new(qualified_name(types.prefix(), "Override"), types.ns());
    element.set_attr("PartName", &part_name);
    element.set_attr("ContentType", content_type);
    types.children.push(Node::Element(element));
    true
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ──────────────────────────────────────────────────────────────

    const RELS_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    const TYPES_NS: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
    const IMAGE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";
    const DECLARATION: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n";

    /// Parse a relationships part holding `relationships`.
    fn rels(relationships: &str) -> Element {
        Element::parse_fragment(&format!(
            "<Relationships xmlns=\"{RELS_NS}\">{relationships}</Relationships>"
        ))
        .unwrap()
    }

    /// Parse a content types part holding `children`.
    fn types(children: &str) -> Element {
        Element::parse_fragment(&format!("<Types xmlns=\"{TYPES_NS}\">{children}</Types>")).unwrap()
    }

    /// Return the ids of the relationships in `rels`.
    fn ids(rels: &Element) -> Vec<String> {
        relationships(rels).map(|rel| rel.id.into_owned()).collect()
    }

    // ── Names ────────────────────────────────────────────────────────────────

    #[test]
    fn rels_part_sits_in_a_rels_directory_next_to_its_source() {
        assert_eq!(
            rels_part_name("word/document.xml"),
            "word/_rels/document.xml.rels"
        );
        assert_eq!(rels_part_name("a/b/c.xml"), "a/b/_rels/c.xml.rels");
        assert_eq!(rels_part_name("document.xml"), "_rels/document.xml.rels");
    }

    #[test]
    fn rels_part_names_are_recognized_ignoring_case() {
        assert!(is_rels_part(PACKAGE_RELS));
        assert!(is_rels_part("word/_rels/document.xml.rels"));
        assert!(is_rels_part("WORD/_RELS/HEADER1.XML.RELS"));
        assert!(!is_rels_part("word/document.xml"));
        assert!(!is_rels_part("word/rels/document.xml.rels"));
        assert!(!is_rels_part("word/_rels/document.xml"));
        assert!(!is_rels_part("_rels"));
        assert!(!is_rels_part(".rels"));
    }

    #[test]
    fn directory_of_a_root_part_is_empty() {
        assert_eq!(directory("word/document.xml"), "word");
        assert_eq!(directory("a/b/c.xml"), "a/b");
        assert_eq!(directory("document.xml"), "");
    }

    #[test]
    fn paths_join_the_directory_of_a_part() {
        assert_eq!(join("word", "media/image"), "word/media/image");
        assert_eq!(join("a/b", "settings"), "a/b/settings");
        assert_eq!(join(directory("document.xml"), "settings"), "settings");
    }

    #[test]
    fn targets_resolve_against_the_source_directory() {
        let main = "word/document.xml";
        assert_eq!(resolve_target(main, "header1.xml"), "word/header1.xml");
        assert_eq!(resolve_target(main, "./media/a.png"), "word/media/a.png");
        assert_eq!(
            resolve_target(main, "../customXml/item1.xml"),
            "customXml/item1.xml"
        );
        assert_eq!(
            resolve_target(main, "/word/footer1.xml"),
            "word/footer1.xml"
        );
        assert_eq!(
            resolve_target(main, "media/../styles.xml"),
            "word/styles.xml"
        );
        assert_eq!(resolve_target("document.xml", "styles.xml"), "styles.xml");
        // The package relationships part has the package root as source.
        assert_eq!(resolve_target("", "word/document.xml"), "word/document.xml");
        assert_eq!(
            resolve_target("", "/word/document.xml"),
            "word/document.xml"
        );
    }

    #[test]
    fn targets_never_resolve_above_the_root() {
        let main = "word/document.xml";
        assert_eq!(resolve_target(main, "../../../secret.xml"), "secret.xml");
        assert_eq!(resolve_target(main, "/../word/a.xml"), "word/a.xml");
        assert_eq!(resolve_target(main, ".."), "");
    }

    #[test]
    fn targets_are_not_percent_decoded() {
        assert_eq!(
            resolve_target("word/document.xml", "header%201.xml"),
            "word/header%201.xml"
        );
    }

    #[test]
    fn relative_targets_climb_out_of_the_source_directory() {
        let image = "word/media/image1.png";
        assert_eq!(
            relative_target("word/document.xml", image),
            "media/image1.png"
        );
        assert_eq!(
            relative_target("word/sub/header1.xml", image),
            "../media/image1.png"
        );
        assert_eq!(
            relative_target("other/footer.xml", image),
            "../word/media/image1.png"
        );
        assert_eq!(
            relative_target("document.xml", image),
            "word/media/image1.png"
        );
        assert_eq!(
            relative_target("word/document.xml", "media/a.png"),
            "../media/a.png"
        );
        // Directory names differing in case are different spellings.
        assert_eq!(
            relative_target("Word/header1.xml", image),
            "../word/media/image1.png"
        );
    }

    #[test]
    fn relative_targets_resolve_back_to_the_part() {
        for (source, part) in [
            ("word/document.xml", "word/media/image7.jpeg"),
            ("word/a/b/header.xml", "word/media/image1.png"),
            ("x/y.xml", "media/image1.gif"),
            ("y.xml", "deep/er/media/image1.gif"),
        ] {
            assert_eq!(resolve_target(source, &relative_target(source, part)), part);
        }
    }

    #[test]
    fn relative_targets_are_percent_encoded() {
        assert_eq!(
            relative_target("a/b.xml", "my docs/media/immagine è.png"),
            "../my%20docs/media/immagine%20%C3%A8.png"
        );
        assert_eq!(relative_target("a/b.xml", "a/100%.png"), "100%25.png");
        assert_eq!(
            relative_target("a/b.xml", "a/x-y_z~(1).png"),
            "x-y_z~(1).png"
        );
    }

    // ── Relationships ────────────────────────────────────────────────────────

    #[test]
    fn relationships_are_read_with_their_attributes_unescaped() {
        let rels = rels(concat!(
            "<Relationship Id=\"rId2\" Type=\"t2\" Target=\"styles.xml\"/>",
            "<Relationship Id=\"rId1\" Type=\"t1\" Target=\"https://example.com/?a=1&amp;b=2\"",
            " TargetMode=\"External\"/>",
            "<Relationship Id=\"rId3\" Type=\"t3\" Target=\"a.xml\" TargetMode=\"Internal\"/>",
        ));
        let all: Vec<_> = relationships(&rels).collect();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, "rId2");
        assert_eq!(all[0].rel_type, "t2");
        assert_eq!(all[0].target, "styles.xml");
        assert!(!all[0].is_external());
        assert_eq!(all[1].target, "https://example.com/?a=1&b=2");
        assert!(all[1].is_external());
        assert!(!all[2].is_external());
    }

    #[test]
    fn relationships_skip_other_elements_and_incomplete_ones() {
        let rels = rels(concat!(
            "<Relationship Type=\"t\" Target=\"no-id.xml\"/>",
            "<Relationship Id=\"rId1\" Target=\"no-type.xml\"/>",
            "<Relationship Id=\"rId2\" Type=\"t\"/>",
            "<Other Id=\"rId3\" Type=\"t\" Target=\"other.xml\"/>",
            "<x:Relationship xmlns:x=\"urn:x\" Id=\"rId4\" Type=\"t\" Target=\"x.xml\"/>",
            "<Relationship Id=\"rId5\" Type=\"t\" Target=\"ok.xml\"/>",
        ));
        assert_eq!(ids(&rels), ["rId5"]);
    }

    #[test]
    fn fresh_id_continues_the_largest_number() {
        assert_eq!(fresh_relationship_id(None), "rId1");
        assert_eq!(fresh_relationship_id(Some(&rels(""))), "rId1");
        let rels = rels(concat!(
            "<Relationship Id=\"rId1\" Type=\"t\" Target=\"a\"/>",
            "<Relationship Id=\"rId12\" Type=\"t\" Target=\"b\"/>",
            "<Relationship Id=\"rId3\" Type=\"t\" Target=\"c\"/>",
            "<Relationship Id=\"rIdx99\" Type=\"t\" Target=\"d\"/>",
            "<Relationship Id=\"rId+50\" Type=\"t\" Target=\"d\"/>",
            "<Relationship Id=\"image7\" Type=\"t\" Target=\"e\"/>",
        ));
        assert_eq!(fresh_relationship_id(Some(&rels)), "rId13");
    }

    #[test]
    fn fresh_id_is_unique_ignoring_case() {
        let rels = rels(concat!(
            "<Relationship Id=\"rId2\" Type=\"t\" Target=\"a\"/>",
            "<Relationship Id=\"RID7\" Type=\"t\" Target=\"b\"/>",
        ));
        assert_eq!(fresh_relationship_id(Some(&rels)), "rId8");
    }

    #[test]
    fn fresh_id_survives_the_largest_number() {
        let rels = rels(concat!(
            "<Relationship Id=\"rId18446744073709551615\" Type=\"t\" Target=\"a\"/>",
            "<Relationship Id=\"rId0\" Type=\"t\" Target=\"b\"/>",
            "<Relationship Id=\"rId99999999999999999999999\" Type=\"t\" Target=\"c\"/>",
        ));
        let id = fresh_relationship_id(Some(&rels));
        assert_eq!(id, "rId1");
    }

    #[test]
    fn new_relationships_part_is_written_like_word_does() {
        let mut part = new_relationships();
        assert_eq!(
            String::from_utf8(part.to_bytes()).unwrap(),
            format!("{DECLARATION}<Relationships xmlns=\"{RELS_NS}\"/>")
        );
        let id = fresh_relationship_id(Some(&part.root));
        add_relationship(&mut part.root, &id, IMAGE, "media/image1.png");
        assert_eq!(
            String::from_utf8(part.to_bytes()).unwrap(),
            format!(
                "{DECLARATION}<Relationships xmlns=\"{RELS_NS}\">\
                 <Relationship Id=\"rId1\" Type=\"{IMAGE}\" Target=\"media/image1.png\"/>\
                 </Relationships>"
            )
        );
        // The new relationship reads back.
        let part = XmlDocument::parse(&part.to_bytes()).unwrap();
        assert_eq!(ids(&part.root), ["rId1"]);
    }

    #[test]
    fn added_relationships_reuse_the_root_prefix() {
        let mut rels = Element::parse_fragment(&format!(
            "<pr:Relationships xmlns:pr=\"{RELS_NS}\">\
             <pr:Relationship Id=\"rId4\" Type=\"t\" Target=\"a.xml\"/></pr:Relationships>"
        ))
        .unwrap();
        let id = fresh_relationship_id(Some(&rels));
        add_relationship(&mut rels, &id, "t&u", "b c.xml");
        assert!(rels.to_xml().ends_with(
            "<pr:Relationship Id=\"rId5\" Type=\"t&amp;u\" Target=\"b c.xml\"/></pr:Relationships>"
        ));
        assert_eq!(ids(&rels), ["rId4", "rId5"]);
        let reparsed = Element::parse_fragment(&rels.to_xml()).unwrap();
        assert_eq!(ids(&reparsed), ["rId4", "rId5"]);
    }

    // ── Content types ────────────────────────────────────────────────────────

    #[test]
    fn default_goes_after_the_last_default() {
        let mut types = types(concat!(
            "<Default Extension=\"rels\" ContentType=\"r\"/>",
            "<Default Extension=\"xml\" ContentType=\"application/xml\"/>",
            "<Override PartName=\"/word/document.xml\" ContentType=\"d\"/>",
        ));
        assert!(ensure_default(&mut types, "png", "image/png"));
        assert_eq!(
            types.to_xml(),
            format!(
                "<Types xmlns=\"{TYPES_NS}\">\
                 <Default Extension=\"rels\" ContentType=\"r\"/>\
                 <Default Extension=\"xml\" ContentType=\"application/xml\"/>\
                 <Default Extension=\"png\" ContentType=\"image/png\"/>\
                 <Override PartName=\"/word/document.xml\" ContentType=\"d\"/></Types>"
            )
        );
    }

    #[test]
    fn existing_default_is_left_alone_ignoring_case() {
        let original = types("<Default Extension=\"PNG\" ContentType=\"image/x-png\"/>");
        let mut types = original.clone();
        assert!(!ensure_default(&mut types, "png", "image/png"));
        assert_eq!(types, original);
    }

    #[test]
    fn default_goes_first_without_defaults() {
        let mut types = types("\n  <Override PartName=\"/a.xml\" ContentType=\"a\"/>\n");
        assert!(ensure_default(&mut types, "rels", RELS_CONTENT_TYPE));
        assert!(types.to_xml().starts_with(&format!(
            "<Types xmlns=\"{TYPES_NS}\"><Default Extension=\"rels\" \
             ContentType=\"{RELS_CONTENT_TYPE}\"/>\n  <Override"
        )));
        assert!(!ensure_default(&mut types, "rels", RELS_CONTENT_TYPE));
    }

    #[test]
    fn override_is_appended_once() {
        let mut types = types(concat!(
            "<Default Extension=\"xml\" ContentType=\"application/xml\"/>",
            "<Override PartName=\"/word/document.xml\" ContentType=\"d\"/>",
        ));
        assert!(ensure_override(&mut types, "word/settings.xml", "s"));
        assert!(types.to_xml().ends_with(
            "<Override PartName=\"/word/document.xml\" ContentType=\"d\"/>\
             <Override PartName=\"/word/settings.xml\" ContentType=\"s\"/></Types>"
        ));
        let once = types.clone();
        assert!(!ensure_override(&mut types, "WORD/Settings.xml", "other"));
        assert!(!ensure_override(&mut types, "word/document.xml", "other"));
        assert_eq!(types, once);
    }

    #[test]
    fn override_part_names_are_percent_encoded() {
        let mut types = types("");
        assert!(ensure_override(&mut types, "my docs/settings.xml", "s"));
        assert!(
            types
                .to_xml()
                .contains("PartName=\"/my%20docs/settings.xml\"")
        );
        assert!(!ensure_override(&mut types, "my docs/settings.xml", "s"));
    }

    #[test]
    fn default_reuses_the_root_prefix() {
        let mut types = Element::parse_fragment(&format!(
            "<ct:Types xmlns:ct=\"{TYPES_NS}\"><ct:Default Extension=\"xml\" ContentType=\"x\"/>\
             </ct:Types>"
        ))
        .unwrap();
        assert!(ensure_default(&mut types, "gif", "image/gif"));
        assert!(types.to_xml().contains(
            "<ct:Default Extension=\"xml\" ContentType=\"x\"/>\
             <ct:Default Extension=\"gif\" ContentType=\"image/gif\"/>"
        ));
        assert!(!ensure_default(&mut types, "GIF", "image/gif"));
    }
}
