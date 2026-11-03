//! [`Docx`]: open a .docx package, find the parts whose text is searched,
//! run the replacements on them and write the package back.
//!
//! Only the parts editing needs are parsed: the content types, the
//! relationships, the story parts (ECMA-376 Part 1, §11.3: the main
//! document, its headers, footers, footnotes and endnotes), the document
//! settings, and the comments and the glossary document, whose trees are
//! dropped once the ids of their drawings are known. Writing serializes
//! the trees that changed and the parts that were added; every other entry
//! is copied from the original archive by [`Package::write`], never
//! re-serialized.

use std::{
    collections::BTreeSet,
    fmt::{self, Debug},
    io::{self, Write},
    iter,
};

use bytes::Bytes;
use thiserror::Error;

use crate::{
    image::Image,
    opc,
    package::{DECOMPRESSION_BUDGET, InvalidZip, MAX_ENTRY_NAME_LEN, Package},
    replace,
    table::Table,
    xml::{Element, InvalidXml, NODE_BUDGET, Node, Ns, W_URI, XmlDocument},
};

// ── Relationship types ───────────────────────────────────────────────────────

/// Relationship from the package to its main part (ECMA-376 Part 1,
/// §11.3). Strict OOXML documents use another URI, so they have no main
/// part here.
const OFFICE_DOCUMENT: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";

/// Relationship from the main part to a header part.
const HEADER: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/header";

/// Relationship from the main part to a footer part.
const FOOTER: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer";

/// Relationship from the main part to the footnotes part.
const FOOTNOTES: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes";

/// Relationship from the main part to the endnotes part.
const ENDNOTES: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/endnotes";

/// Relationships from the main part to the other story parts.
const STORY_TYPES: [&str; 4] = [HEADER, FOOTER, FOOTNOTES, ENDNOTES];

/// Relationship from the main part to the comments part.
const COMMENTS: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments";

/// Relationship from the main part to the glossary document, which holds
/// building blocks such as cover pages.
const GLOSSARY_DOCUMENT: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/glossaryDocument";

/// Relationships from the main part to parts that are not searched but can
/// hold drawings, whose ids new drawings must not reuse.
const DRAWING_ID_TYPES: [&str; 2] = [COMMENTS, GLOSSARY_DOCUMENT];

/// Relationship from the main part to the document settings part.
const SETTINGS: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings";

/// Media type of the document settings part.
const SETTINGS_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml";

/// The children of `w:settings` that come after `w:updateFields` in schema
/// order (`CT_Settings`, ECMA-376 Part 1, §17.15.1), which Word expects.
/// Children in other namespaces (`m:mathPr`, `sl:schemaLibrary`, Word's own
/// extensions and `mc:AlternateContent`) all come after it too.
const AFTER_UPDATE_FIELDS: [&str; 19] = [
    "hdrShapeDefaults",
    "footnotePr",
    "endnotePr",
    "compat",
    "docVars",
    "rsids",
    "attachedSchema",
    "themeFontLang",
    "clrSchemeMapping",
    "doNotIncludeSubdocsInStats",
    "doNotAutoCompressPictures",
    "forceUpgrade",
    "captions",
    "readModeInkLockDown",
    "smartTagType",
    "shapeDefaults",
    "doNotEmbedSmartTags",
    "decimalSymbol",
    "listSeparator",
];

/// Relationship from a story part to an image it shows.
const IMAGE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";

// ── Errors ───────────────────────────────────────────────────────────────────

/// Error returned when [`Docx::from_bytes`] can't open a file as a Word
/// document.
///
/// Part names come from the file: messages escape control characters in
/// them.
///
/// The enum may grow new rejection reasons: match with a wildcard arm.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum InvalidDocx {
    /// The file is not a ZIP archive that can be read and written back, or
    /// one of the parts that must be parsed can't be decompressed - it is
    /// corrupt, or exceeds the decompression budget. The
    /// [`source`](std::error::Error::source) says what is wrong.
    #[error("unreadable ZIP archive")]
    Zip(#[source] InvalidZip),
    /// A part that must be parsed is not well-formed XML, uses something
    /// this crate doesn't support, or makes the parts read exceed their
    /// node budget (see [`InvalidXml`] and the limits of
    /// [`Docx::from_bytes`]).
    #[error("unreadable XML part `{}`", .part.escape_debug())]
    Xml {
        /// The part name, without the leading `/` (`word/document.xml`).
        part: String,
        /// What is wrong with the XML.
        #[source]
        source: InvalidXml,
    },
    /// A part the document needs is missing: `[Content_Types].xml`,
    /// `_rels/.rels`, the main document part, or a header, footer,
    /// footnotes or endnotes part the main part refers to. The part name
    /// has no leading `/`.
    #[error("missing part `{}`", .0.escape_debug())]
    MissingPart(String),
    /// The package has no main document part, or its main part is not a
    /// Transitional WordprocessingML document (`w:document`): the file is
    /// a spreadsheet, a presentation or another kind of package, or a
    /// Strict OOXML document.
    #[error("not a WordprocessingML document")]
    NotWordprocessingMl,
}

/// Error returned when [`Docx::replace_with_table`] finds its placeholder
/// in a paragraph that holds anything besides it.
///
/// A table can only take the place of a whole paragraph, so the
/// placeholder must be the only content of every paragraph it occurs in.
/// The placeholder can be recovered with [`into_inner`](Self::into_inner).
#[derive(Debug, Error)]
#[error("table placeholder `{0}` must be the only content of its paragraph")]
#[non_exhaustive]
pub struct InvalidPlaceholder(String);

impl InvalidPlaceholder {
    /// View the placeholder as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Return the placeholder.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

// ── Docx ─────────────────────────────────────────────────────────────────────

/// A Word document opened as a template.
///
/// [`from_bytes`](Self::from_bytes) opens a .docx file, the `replace_*`
/// methods fill in its placeholders, and [`to_bytes`](Self::to_bytes) or
/// [`write_to`](Self::write_to) give the result. Replacements apply to the
/// main text and to every header, footer, footnote and endnote; see
/// [`replace_text`](Self::replace_text) for how placeholders are matched.
///
/// Writing doesn't consume the document, and a `Docx` can be cloned, so a
/// template can be opened once and each output filled from a clone of it.
/// Clones share the original file and copy only the parsed parts.
///
/// ```no_run
/// use ciclostile::Docx;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let template = Docx::from_bytes(std::fs::read("letter.docx")?)?;
/// for name in ["Ada", "Grace"] {
///     let mut letter = template.clone();
///     letter.replace_text([("{{name}}", name)]);
///     std::fs::write(format!("letter-{name}.docx"), letter.to_bytes())?;
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Docx {
    /// The original archive, which entries that aren't rewritten are
    /// copied from.
    package: Package,
    /// `[Content_Types].xml`, which gains a `Default` when an image or a
    /// relationships part needs one.
    content_types: Part,
    /// The story parts: the main part first, then the other stories in the
    /// order of the main part's relationships. No two share a name, ignoring
    /// ASCII case, and none is the content types part or a relationships
    /// part (see [`is_reserved`]).
    stories: Vec<Story>,
    /// Media parts added by [`replace_with_image`](Self::replace_with_image),
    /// with their content, in the order added.
    media: Vec<(String, Bytes)>,
    /// The names in `media`, in ASCII lowercase, to find free names quickly.
    media_names: BTreeSet<String>,
    /// Source of new `wp:docPr` ids.
    drawing_ids: DrawingIds,
    /// The document settings, which
    /// [`update_fields_on_open`](Self::update_fields_on_open) edits.
    settings: Settings,
}

/// A parsed XML part.
#[derive(Clone)]
struct Part {
    /// The part name, without the leading `/`.
    name: String,
    /// The parsed content.
    xml: XmlDocument,
    /// Whether `xml` was modified, or created, and must be written.
    dirty: bool,
}

/// The document settings part, as found when opening the document.
#[derive(Clone)]
enum Settings {
    /// The parsed settings part, which is never a story part, a
    /// relationships part or the content types part.
    Part(Part),
    /// The main part has no settings relationship: a part and its
    /// relationship are created when needed.
    Absent,
    /// The main part's settings relationship points to this missing part,
    /// whose name is a plain `.xml` name (see [`is_plain_xml_name`]): it is
    /// created there when needed.
    Missing(String),
    /// The settings relationship points to a part that can't be edited as
    /// settings: one read as something else, one whose root isn't
    /// `w:settings`, or a missing one with an unusual name. It is left
    /// alone.
    Unusable,
}

/// A part whose text the replacements search, with its relationships.
#[derive(Clone)]
struct Story {
    /// The story part.
    part: Part,
    /// Its relationships part, if it has one or one was created.
    rels: Option<Part>,
}

impl Story {
    /// Iterate over the names of the parts that the story's internal
    /// relationships of one of `types` point to, in document order.
    fn targets(&self, types: &[&str]) -> impl Iterator<Item = String> {
        self.rels
            .iter()
            .flat_map(|rels| opc::relationships(&rels.xml.root))
            .filter(move |rel| !rel.is_external() && types.contains(&&*rel.rel_type))
            .map(|rel| opc::resolve_target(&self.part.name, &rel.target))
    }

    /// Return whether a relationship can be added to the story: it has a
    /// relationships part, or the one it would get, named after it, fits
    /// in a ZIP archive.
    fn can_add_relationship(&self) -> bool {
        self.rels.is_some() || opc::rels_part_name(&self.part.name).len() <= MAX_ENTRY_NAME_LEN
    }

    /// Return an id for a new relationship of the story.
    fn fresh_relationship_id(&self) -> String {
        opc::fresh_relationship_id(self.rels.as_ref().map(|rels| &rels.xml.root))
    }

    /// Add a relationship of type `rel_type` from the story to the part
    /// named `part`, under `id`, and return whether the story's
    /// relationships part was created for it.
    ///
    /// `id` must come from
    /// [`fresh_relationship_id`](Self::fresh_relationship_id), and the
    /// story must pass [`can_add_relationship`](Self::can_add_relationship).
    fn add_relationship(&mut self, id: &str, rel_type: &str, part: &str) -> bool {
        let created = self.rels.is_none();
        let rels = self.rels.get_or_insert_with(|| Part {
            name: opc::rels_part_name(&self.part.name),
            xml: opc::new_relationships(),
            dirty: true,
        });
        let target = opc::relative_target(&self.part.name, part);
        opc::add_relationship(&mut rels.xml.root, id, rel_type, &target);
        rels.dirty = true;
        created
    }
}

impl Docx {
    /// Open a .docx file.
    ///
    /// `bytes` can be anything that converts into [`Bytes`] without a
    /// copy, such as the `Vec<u8>` that [`std::fs::read`] returns or a
    /// `&'static [u8]`. Entries that are copied to the output are not
    /// copied in memory: the output shares them with `bytes`.
    ///
    /// Only Transitional Office Open XML is supported, the format Word and
    /// LibreOffice write, with XML parts in UTF-8: Strict Office Open XML
    /// documents, encrypted (password-protected) documents and legacy
    /// `.doc` files are rejected. Templates (.dotx, .dotm) and
    /// macro-enabled documents (.docm) open too, and keep their content
    /// type once filled, so save them with the same kind of extension: Word
    /// refuses to open a .dotx saved as .docx. To produce a .docx, start
    /// from a .docx.
    ///
    /// Only the parts the replacements need are decompressed and parsed:
    /// the content types, the relationships, the main document part and
    /// its headers, footers, footnotes and endnotes, the document settings,
    /// plus the comments and the glossary document, if any, to collect the
    /// ids of their drawings. Other entries, such as images, are copied
    /// without ever being decompressed.
    ///
    /// # Limits
    ///
    /// The parts read may decompress to 64 MiB in total and hold 4 million
    /// nodes (elements, runs of text, comments) together. Their elements
    /// may nest 512 levels deep, under at most 128 namespace declarations,
    /// with element and attribute names of at most 256 bytes. Input beyond
    /// these limits is rejected, so opening a document takes at most about
    /// 650 MB of memory, whatever the input.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidDocx::Zip`] when `bytes` is not a ZIP archive that
    /// can be read and written back, or when a part that must be parsed
    /// can't be decompressed (including when the parts read add up to more
    /// than 64 MiB); [`InvalidDocx::MissingPart`] when `[Content_Types].xml`,
    /// `_rels/.rels`, the main document part or a story part it refers to
    /// is missing; [`InvalidDocx::Xml`] when one of the parts read is
    /// malformed, or when they hold more than 4,000,000 nodes together;
    /// and [`InvalidDocx::NotWordprocessingMl`] when the file is not a
    /// Transitional WordprocessingML document, such as a spreadsheet, a
    /// presentation or a Strict OOXML document.
    pub fn from_bytes(bytes: impl Into<Bytes>) -> Result<Self, InvalidDocx> {
        let budget = Budget {
            bytes: DECOMPRESSION_BUDGET,
            nodes: NODE_BUDGET,
        };
        Self::open(bytes.into(), budget)
    }

    /// Open a .docx file, charging every part read to `budget`, which tests
    /// make small.
    ///
    /// # Errors
    ///
    /// See [`from_bytes`](Self::from_bytes).
    fn open(bytes: Bytes, mut budget: Budget) -> Result<Self, InvalidDocx> {
        let package = Package::parse(bytes).map_err(InvalidDocx::Zip)?;
        let content_types = read_part(&package, opc::CONTENT_TYPES, &mut budget)?;
        let package_rels = read_part(&package, opc::PACKAGE_RELS, &mut budget)?;

        let main_name = opc::relationships(&package_rels.xml.root)
            .find(|rel| rel.rel_type == OFFICE_DOCUMENT && !rel.is_external())
            .map(|rel| opc::resolve_target("", &rel.target))
            .filter(|name| !is_reserved(name))
            .ok_or(InvalidDocx::NotWordprocessingMl)?;
        let main = read_part(&package, &main_name, &mut budget)?;
        // Strict OOXML has its own WordprocessingML namespace, which is
        // `Ns::Other` here, as are SpreadsheetML and PresentationML.
        if !main.xml.root.is(Ns::W, "document") {
            return Err(InvalidDocx::NotWordprocessingMl);
        }
        let main = Story {
            rels: read_rels(&package, &main_name, &mut budget)?,
            part: main,
        };

        // Relationships to reserved names are malformed: they aren't
        // followed.
        let targets = |types: &[&str]| -> Vec<String> {
            main.targets(types)
                .filter(|name| !is_reserved(name))
                .collect()
        };
        let story_names = targets(&STORY_TYPES);
        let drawing_id_names = targets(&DRAWING_ID_TYPES);
        // Reserved names are not filtered out: a relationship to one still
        // means the document has settings, which can't be edited.
        let settings_name = main.targets(&[SETTINGS]).next();

        // Sections often share headers and footers: read each part once.
        let mut seen = BTreeSet::from([main_name.to_ascii_lowercase()]);
        let mut stories = vec![main];
        for name in story_names {
            if seen.insert(name.to_ascii_lowercase()) {
                let part = read_part(&package, &name, &mut budget)?;
                let rels = read_rels(&package, &name, &mut budget)?;
                stories.push(Story { part, rels });
            }
        }
        let settings = read_settings(&package, settings_name, &mut seen, &mut budget)?;

        let mut drawing_ids = DrawingIds::default();
        for story in &stories {
            drawing_ids.collect(&story.part.xml.root);
        }
        // Comments and the glossary document are never edited, so they may
        // be missing; their trees are dropped once their ids are known.
        // Parts read already, the settings included, are not read again.
        for name in drawing_id_names {
            if seen.insert(name.to_ascii_lowercase())
                && let Some(part) = read_optional_part(&package, &name, &mut budget)?
            {
                drawing_ids.collect(&part.xml.root);
            }
        }

        Ok(Self {
            package,
            content_types,
            stories,
            media: Vec::new(),
            media_names: BTreeSet::new(),
            drawing_ids,
            settings,
        })
    }

    /// Replace every occurrence of each key of `replacements` with its
    /// value, and return the number of replacements made.
    ///
    /// The keys are placeholders: plain text, such as `{{name}}`, matched
    /// exactly and case-sensitively in the text of each paragraph, as Word
    /// displays it. Word often stores text typed in one go as several runs -
    /// after a formatting change, a spelling check or an edit made on
    /// another day - so `{{name}}` may be stored as `{{`, `name` and `}}`
    /// with markup in between: it still matches. A placeholder never spans
    /// two paragraphs, nor anything in a paragraph that isn't text: one
    /// interrupted by a tab, a line break, a picture, an equation, a
    /// footnote reference or a field is left as it is. Text deleted with
    /// tracked changes isn't matched.
    ///
    /// The main text is searched, including its tables, text boxes and
    /// content controls, and so are headers, footers, footnotes and
    /// endnotes. Other parts, such as comments, are left alone. Word stores
    /// each text box twice, as DrawingML and as VML for older readers, so a
    /// placeholder in a text box counts twice.
    ///
    /// All pairs are applied in a single pass over the text as it was
    /// before the call: text inserted for a key is never searched again, for
    /// that key or another one, so replacing `{{a}}` with `{{b}}` and
    /// `{{b}}` with `x` in the same call turns `{{a}}` into `{{b}}`. Where
    /// two keys start at the same place, the longer one wins. Empty keys are
    /// ignored, and a key given more than once only uses its first value.
    ///
    /// A value takes the formatting of the first character of the
    /// placeholder it replaces. In values, line breaks (`\n`, `\r\n` or
    /// `\r`) become line breaks in the paragraph and tabs become tab
    /// characters. Characters that XML can't represent (most control
    /// characters) are dropped.
    ///
    /// Each occurrence gets its own copy of its value, and the template
    /// decides how many occurrences there are, so a small template can make
    /// a large document. With untrusted templates, bound their size or the
    /// number of placeholders they hold.
    ///
    /// Pairs can come from any collection of string-like pairs, such as an
    /// array of tuples, a `Vec<(String, String)>` or a map. A
    /// `&Vec<(String, String)>` yields references to tuples instead: pass
    /// `pairs.iter().map(|(k, v)| (k, v))`, or the `Vec` itself.
    ///
    /// ```no_run
    /// use std::collections::HashMap;
    ///
    /// use ciclostile::Docx;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut docx = Docx::from_bytes(std::fs::read("template.docx")?)?;
    /// let values = HashMap::from([
    ///     ("{{name}}".to_owned(), "Ada Lovelace".to_owned()),
    ///     ("{{address}}".to_owned(), "12 St James's Square\nLondon".to_owned()),
    /// ]);
    /// let replaced = docx.replace_text(&values);
    /// println!("{replaced} placeholders replaced");
    /// # Ok(())
    /// # }
    /// ```
    pub fn replace_text<I, K, V>(&mut self, replacements: I) -> usize
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let replacements: Vec<(K, V)> = replacements.into_iter().collect();
        let pairs: Vec<(&str, &str)> = replacements
            .iter()
            .map(|(key, value)| (key.as_ref(), value.as_ref()))
            .collect();

        let mut total = 0_usize;
        for story in &mut self.stories {
            let count = replace::replace_text(&mut story.part.xml.root, &pairs);
            if count > 0 {
                story.part.dirty = true;
                total = total.saturating_add(count);
            }
        }
        total
    }

    /// Replace every occurrence of `placeholder` with `image`, and return
    /// the number of replacements made.
    ///
    /// The placeholder is matched like a key of
    /// [`replace_text`](Self::replace_text). The image takes its place
    /// inline, as part of the line of text, at the size set on `image`. Its
    /// file is added to the document once per call, however many
    /// placeholders it replaces. An empty placeholder matches nothing.
    ///
    /// Each occurrence gets a drawing of its own, so a small template can
    /// make a large document: 200,000 image placeholders in a 2.4 KB .docx
    /// take 830 MB of memory to fill. With untrusted templates, bound their
    /// size or the number of placeholders they hold. An occurrence nested
    /// so deep, or under so many namespace declarations, that its drawing
    /// would exceed the limits of [`from_bytes`](Self::from_bytes) is left
    /// as it is, and not counted.
    ///
    /// When no placeholder is found, the document is left unchanged.
    pub fn replace_with_image(&mut self, placeholder: &str, image: &Image) -> usize {
        if placeholder.is_empty() {
            return 0;
        }
        let Some(media_name) = self.new_media_name(image.extension()) else {
            return 0;
        };

        let mut total = 0_usize;
        let mut created_rels = false;
        for story in &mut self.stories {
            // A story without a relationships part gets one, named after
            // it. If that name is too long for a ZIP archive, the story
            // can't refer to the image: its placeholders stay as they are.
            if !story.can_add_relationship() {
                continue;
            }

            // The relationship is only added if a placeholder is found, but
            // the drawings need its id first.
            let rel_id = story.fresh_relationship_id();
            let drawing_ids = &mut self.drawing_ids;
            let count =
                replace::replace_with_element(&mut story.part.xml.root, placeholder, || {
                    parse_snippet(&image.drawing_xml(&rel_id, drawing_ids.allocate()))
                });
            if count == 0 {
                continue;
            }
            story.part.dirty = true;
            total = total.saturating_add(count);
            created_rels |= story.add_relationship(&rel_id, IMAGE, &media_name);
        }
        if total == 0 {
            return 0;
        }

        let types = &mut self.content_types;
        types.dirty |=
            opc::ensure_default(&mut types.xml.root, image.extension(), image.content_type());
        types.dirty |= created_rels
            && opc::ensure_default(&mut types.xml.root, "rels", opc::RELS_CONTENT_TYPE);
        self.media_names.insert(media_name.to_ascii_lowercase());
        self.media.push((media_name, image.data().clone()));
        total
    }

    /// Replace every paragraph that holds only `placeholder` with `table`,
    /// and return the number of paragraphs replaced.
    ///
    /// The placeholder is matched like a key of
    /// [`replace_text`](Self::replace_text), and must be the only content
    /// of its paragraph: the two are compared with the whitespace around
    /// each trimmed, non-breaking spaces included. The table takes the
    /// paragraph's place, and keeps its bookmarks and comment anchors.
    /// Empty paragraphs are kept or added where Word requires them: after
    /// the table when the placeholder's paragraph ended a section or isn't
    /// followed by another paragraph (a table cell, for one, must end with
    /// a paragraph), and before it when it would follow another table
    /// (adjacent tables would merge). An empty placeholder matches nothing.
    ///
    /// Each paragraph replaced gets a copy of the whole table (see
    /// [`Table`] for how many cells it has), so a small template can make a
    /// large document. With untrusted templates, bound their size or the
    /// number of placeholders they hold. A paragraph nested so deep, or
    /// under so many namespace declarations, that the table would exceed
    /// the limits of [`from_bytes`](Self::from_bytes) is left as it is, and
    /// not counted.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidPlaceholder`] when `placeholder` occurs in a
    /// paragraph that holds anything else. The document is then left
    /// unchanged: no paragraph is replaced, not even those that qualify.
    pub fn replace_with_table(
        &mut self,
        placeholder: &str,
        table: &Table,
    ) -> Result<usize, InvalidPlaceholder> {
        if placeholder.is_empty() {
            return Ok(0);
        }
        let alone = self
            .stories
            .iter()
            .all(|story| replace::table_placeholders_are_alone(&story.part.xml.root, placeholder));
        if !alone {
            return Err(InvalidPlaceholder(placeholder.to_owned()));
        }

        let table = parse_snippet(&table.to_xml());
        let mut total = 0_usize;
        for story in &mut self.stories {
            let count = replace::replace_paragraphs_with_table(
                &mut story.part.xml.root,
                placeholder,
                &table,
            );
            if count > 0 {
                story.part.dirty = true;
                total = total.saturating_add(count);
            }
        }
        Ok(total)
    }

    /// Make Word offer to update the document's fields when it opens the
    /// file, so that tables of contents and page references show the pages
    /// of the filled document.
    ///
    /// Nothing else needs to be laid out again: a .docx file stores no
    /// pages. Word and LibreOffice paginate a document when they open it,
    /// so longer text simply pushes what follows onto the next pages. Only
    /// the cached results of fields depend on the layout: a .docx file
    /// stores the result each field had when it was last saved, so a table
    /// of contents or a page reference keeps the page numbers saved in the
    /// template, even where the replacements moved text to other pages, and
    /// Word only refreshes them when the fields are updated (F9).
    ///
    /// This sets `w:updateFields` in the document settings (ECMA-376 Part 1,
    /// §17.15.1.90), creating the settings part if the document has none.
    /// Word then asks the reader, when opening the file, whether to update
    /// the fields: the reader has to accept the prompt, and Word then
    /// updates every field with its own layout. LibreOffice ignores the
    /// setting: there, update the fields with Tools > Update > Update All.
    ///
    /// Calling it again changes nothing.
    ///
    /// ```no_run
    /// use ciclostile::Docx;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut docx = Docx::from_bytes(std::fs::read("report.docx")?)?;
    /// docx.replace_text([("{{findings}}", "A long text that moves the conclusion.")]);
    /// docx.update_fields_on_open();
    /// std::fs::write("filled.docx", docx.to_bytes())?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn update_fields_on_open(&mut self) {
        let target = match &mut self.settings {
            Settings::Part(part) => {
                if switch_on_update_fields(&mut part.xml.root) {
                    part.dirty = true;
                }
                return;
            }
            Settings::Unusable => return,
            Settings::Absent => None,
            Settings::Missing(name) => Some(std::mem::take(name)),
        };
        self.settings = self.create_settings(target);
    }

    /// Write the document as a .docx file to `writer`.
    ///
    /// Parts that no replacement modified are never re-serialized: their
    /// compressed entries are copied byte for byte, and uncompressed
    /// entries keep identical content but come out compressed. Modified
    /// parts keep the exact bytes of all the XML the replacements didn't
    /// touch: attribute order, namespace prefixes, whitespace and comments
    /// included. Writing doesn't change the document, which can be written
    /// again or modified further.
    ///
    /// `writer` receives many small writes: wrap a file or a socket in a
    /// [`BufWriter`](std::io::BufWriter), and flush it once `write_to`
    /// returns, so that an error from it isn't lost when it is dropped.
    ///
    /// ```no_run
    /// use std::{
    ///     fs::File,
    ///     io::{BufWriter, Write},
    /// };
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let docx = ciclostile::Docx::from_bytes(std::fs::read("template.docx")?)?;
    /// let mut out = BufWriter::new(File::create("output.docx")?);
    /// docx.write_to(&mut out)?;
    /// out.flush()?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the [`io::Error`] of `writer` when writing to it fails;
    /// `writer` may then hold part of the archive.
    pub fn write_to<W: Write + ?Sized>(&self, writer: &mut W) -> io::Result<()> {
        self.package.write(writer, &self.changed_parts()).map(drop)
    }

    /// Return the document as the content of a .docx file.
    ///
    /// The same as [`write_to`](Self::write_to) into a `Vec<u8>`, which
    /// can't fail, and which the returned [`Bytes`] takes over without a
    /// copy.
    #[must_use]
    #[expect(
        clippy::missing_panics_doc,
        reason = "writing to a Vec only fails on invalid part names, which are never generated"
    )]
    pub fn to_bytes(&self) -> Bytes {
        let archive = self
            .package
            .write(Vec::new(), &self.changed_parts())
            .expect("parts to write have distinct, canonical names that fit in a ZIP archive");
        Bytes::from(archive)
    }

    /// Return the name for a new media part holding a file with
    /// `extension`, or `None` if it would be too long for a ZIP archive.
    ///
    /// Like Word, media go in a `media` directory next to the main part,
    /// named `image{n}.{extension}` with the smallest `n` that no entry of
    /// the package and no media part added earlier uses, ignoring ASCII
    /// case. New relationships parts can't take the name: they end with
    /// `.rels`.
    fn new_media_name(&self, extension: &str) -> Option<String> {
        let main = self.stories.first()?;
        let prefix = opc::join(opc::directory(&main.part.name), "media/image");
        let candidates = (1..=u64::MAX).map(|n| format!("{prefix}{n}.{extension}"));
        free_name(candidates, |name| {
            self.package.contains(name) || self.media_names.contains(&name.to_ascii_lowercase())
        })
    }

    /// Create a settings part with `w:updateFields` switched on, and return
    /// it.
    ///
    /// It goes under `target`, the resolved target of the main part's
    /// settings relationship, or else under a free name next to the main
    /// part, with a new relationship. The content types get an `Override`
    /// for it. Returns [`Settings::Unusable`], changing nothing, when no
    /// name fits in a ZIP archive.
    fn create_settings(&mut self, target: Option<String>) -> Settings {
        let Some(main) = self.stories.first_mut() else {
            return Settings::Unusable;
        };
        let types = &mut self.content_types;
        let name = if let Some(name) = target {
            name
        } else {
            if !main.can_add_relationship() {
                return Settings::Unusable;
            }
            let Some(name) = free_settings_name(&self.package, opc::directory(&main.part.name))
            else {
                return Settings::Unusable;
            };
            let id = main.fresh_relationship_id();
            let created_rels = main.add_relationship(&id, SETTINGS, &name);
            types.dirty |= created_rels
                && opc::ensure_default(&mut types.xml.root, "rels", opc::RELS_CONTENT_TYPE);
            name
        };
        types.dirty |= opc::ensure_override(&mut types.xml.root, &name, SETTINGS_CONTENT_TYPE);

        let mut root = Element::new("w:settings", Ns::W);
        root.set_attr("xmlns:w", W_URI);
        switch_on_update_fields(&mut root);
        Settings::Part(Part {
            name,
            xml: XmlDocument::new(root),
            dirty: true,
        })
    }

    /// Iterate over the parsed parts: the content types, then each story
    /// followed by its relationships part, then the settings.
    fn parts(&self) -> impl Iterator<Item = &Part> {
        let settings = match &self.settings {
            Settings::Part(part) => Some(part),
            Settings::Absent | Settings::Missing(_) | Settings::Unusable => None,
        };
        iter::once(&self.content_types)
            .chain(
                self.stories
                    .iter()
                    .flat_map(|story| iter::once(&story.part).chain(&story.rels)),
            )
            .chain(settings)
    }

    /// Return the name and content of every part that writing replaces or
    /// adds: the modified and created XML parts, serialized, then the media
    /// parts.
    ///
    /// The names are distinct ignoring ASCII case: stories are distinct and
    /// never the content types part or a relationships part, relationships
    /// parts are named after distinct stories, the settings part is none of
    /// those, and created parts take free names (media parts never end with
    /// `.xml`, created settings parts always do). None names a directory
    /// (all end with a file name), all are canonical (built from canonical
    /// names and plain segments) and none is longer than
    /// [`MAX_ENTRY_NAME_LEN`]: that is all [`Package::write`] requires.
    fn changed_parts(&self) -> Vec<(String, Bytes)> {
        self.parts()
            .filter(|part| part.dirty)
            .map(|part| (part.name.clone(), Bytes::from(part.xml.to_bytes())))
            // Cloning `Bytes` shares the image files, it doesn't copy them.
            .chain(self.media.iter().cloned())
            .collect()
    }
}

impl Debug for Docx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Names only: the parsed parts and the media can be megabytes long.
        let stories: Vec<&str> = self.stories.iter().map(|story| &*story.part.name).collect();
        let changed: Vec<&str> = self
            .parts()
            .filter(|part| part.dirty)
            .map(|part| &*part.name)
            .chain(self.media.iter().map(|(name, _)| &**name))
            .collect();
        f.debug_struct("Docx")
            .field("parts", &self.package.names().collect::<Vec<_>>())
            .field("stories", &stories)
            .field("changed", &changed)
            .finish_non_exhaustive()
    }
}

// ── Opening ──────────────────────────────────────────────────────────────────

/// What the parts read to open a document may still take.
struct Budget {
    /// Bytes [`Package::read`] may still return.
    bytes: usize,
    /// Nodes the parsed trees may still hold, the trees dropped after
    /// reading their drawing ids included.
    nodes: usize,
}

/// Read and parse the part named `name`, charging it to `budget`.
///
/// # Errors
///
/// Returns [`InvalidDocx::MissingPart`] when the package has no such part,
/// and the errors of [`read_optional_part`] otherwise.
fn read_part(package: &Package, name: &str, budget: &mut Budget) -> Result<Part, InvalidDocx> {
    read_optional_part(package, name, budget)?
        .ok_or_else(|| InvalidDocx::MissingPart(name.to_owned()))
}

/// Read and parse the part named `name`, if the package has one, charging
/// it to `budget`.
///
/// # Errors
///
/// Returns [`InvalidDocx::Zip`] when the part can't be decompressed or
/// exceeds the remaining bytes, and [`InvalidDocx::Xml`] when it is
/// malformed or exceeds the remaining nodes.
fn read_optional_part(
    package: &Package,
    name: &str,
    budget: &mut Budget,
) -> Result<Option<Part>, InvalidDocx> {
    let Some(content) = package
        .read(name, &mut budget.bytes)
        .map_err(InvalidDocx::Zip)?
    else {
        return Ok(None);
    };
    let xml = XmlDocument::parse_with_budget(&content, &mut budget.nodes).map_err(|source| {
        InvalidDocx::Xml {
            part: name.to_owned(),
            source,
        }
    })?;
    Ok(Some(Part {
        name: name.to_owned(),
        xml,
        dirty: false,
    }))
}

/// Read and parse the relationships part of the part named `part`, if it
/// has one, charging it to `budget`.
///
/// # Errors
///
/// See [`read_optional_part`].
fn read_rels(
    package: &Package,
    part: &str,
    budget: &mut Budget,
) -> Result<Option<Part>, InvalidDocx> {
    read_optional_part(package, &opc::rels_part_name(part), budget)
}

/// Find the document settings: the part named `name`, the resolved target
/// of the main part's settings relationship, if it has one. The part is
/// read if it exists, and charged to `budget`.
///
/// `seen` holds the names, in ASCII lowercase, of the parts read so far:
/// a part that is already a story can't be edited as settings too. The
/// settings part is added to it, so that it isn't read again.
///
/// # Errors
///
/// See [`read_optional_part`].
fn read_settings(
    package: &Package,
    name: Option<String>,
    seen: &mut BTreeSet<String>,
    budget: &mut Budget,
) -> Result<Settings, InvalidDocx> {
    let Some(name) = name else {
        return Ok(Settings::Absent);
    };
    if is_reserved(&name) {
        return Ok(Settings::Unusable);
    }
    if !seen.insert(name.to_ascii_lowercase()) {
        return Ok(Settings::Unusable);
    }
    let settings = match read_optional_part(package, &name, budget)? {
        Some(part) if part.xml.root.is(Ns::W, "settings") => Settings::Part(part),
        None if is_plain_xml_name(&name) => Settings::Missing(name),
        // A part with another root, or a missing one with an unusual name.
        _ => Settings::Unusable,
    };
    Ok(settings)
}

/// Return whether a relationship target resolved to `name` can't be a
/// story part: it is the package root, the content types part or a
/// relationships part.
///
/// No valid document has such a relationship. The parts this crate may
/// rewrite must have distinct names, and the content types and
/// relationships parts are rewritten on their own, so these targets are
/// not followed.
fn is_reserved(name: &str) -> bool {
    name.is_empty() || name.eq_ignore_ascii_case(opc::CONTENT_TYPES) || opc::is_rels_part(name)
}

/// Return whether a missing part may be created under `name`: segments of
/// ASCII letters, digits, `-`, `_` and `.` (but not `.` or `..`), short
/// enough for a ZIP archive, and an `.xml` extension.
///
/// Such a name is a canonical entry name, and can't be the name of a
/// relationships part or of a media part created later.
fn is_plain_xml_name(name: &str) -> bool {
    let plain_segment = |segment: &str| {
        !matches!(segment, "" | "." | "..")
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    name.len() <= MAX_ENTRY_NAME_LEN
        && name.split('/').all(plain_segment)
        && name
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("xml"))
}

/// Return the first of `settings.xml`, `settings1.xml`, `settings2.xml`,
/// ... in `directory` that no entry of `package` uses, or `None` if it
/// doesn't fit in a ZIP archive.
fn free_settings_name(package: &Package, directory: &str) -> Option<String> {
    let prefix = opc::join(directory, "settings");
    let candidates = iter::once(format!("{prefix}.xml"))
        .chain((1..=u64::MAX).map(|n| format!("{prefix}{n}.xml")));
    free_name(candidates, |name| package.contains(name))
}

/// Return the first of `candidates` that is not `taken`, or `None` if it
/// doesn't fit in a ZIP archive.
///
/// The candidates must be distinct: each one tried is then new, so this
/// ends after at most one try per taken name, plus one.
fn free_name(
    mut candidates: impl Iterator<Item = String>,
    taken: impl Fn(&str) -> bool,
) -> Option<String> {
    candidates
        .find(|name| !taken(name))
        .filter(|name| name.len() <= MAX_ENTRY_NAME_LEN)
}

/// Switch `w:updateFields` on in `settings`, the root element of a settings
/// part, and return whether `settings` changed.
///
/// Attribute prefixes aren't resolved in the tree, so the part is left
/// alone only when its one `w:updateFields` is certainly on (see
/// [`is_plainly_on`]), or is the element this function writes. Otherwise
/// every `w:updateFields` child is removed, and a new one goes where the
/// first was, or else before the first child that the schema orders after
/// it (see [`AFTER_UPDATE_FIELDS`]), or last.
fn switch_on_update_fields(settings: &mut Element) -> bool {
    let prefix = settings.prefix();
    let element = new_update_fields(prefix);
    if only_update_fields(settings)
        .is_some_and(|only| *only == element || is_plainly_on(only, prefix))
    {
        return false;
    }

    let is_update_fields = |node: &Node| {
        node.as_element()
            .is_some_and(|child| child.is(Ns::W, "updateFields"))
    };
    let position = settings
        .children
        .iter()
        .position(is_update_fields)
        .unwrap_or_else(|| {
            settings
                .children
                .iter()
                .position(|node| {
                    node.as_element().is_some_and(|child| {
                        child.ns() != Ns::W || AFTER_UPDATE_FIELDS.contains(&child.local_name())
                    })
                })
                .unwrap_or(settings.children.len())
        });
    // Nothing before `position` is removed, so it stays where the first
    // `w:updateFields` was.
    settings.children.retain(|node| !is_update_fields(node));
    settings.children.insert(position, Node::Element(element));
    true
}

/// Return the `w:updateFields` child of `settings` if it has exactly one.
fn only_update_fields(settings: &Element) -> Option<&Element> {
    let mut existing = settings
        .child_elements()
        .filter(|child| child.is(Ns::W, "updateFields"));
    match (existing.next(), existing.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

/// Return whether `element`, a `w:updateFields` child of a settings root
/// whose prefix is `root_prefix`, is certainly on.
///
/// The root is `w:settings` in WordprocessingML, so `root_prefix` is bound
/// to it there. The element must have that prefix too, and no attribute
/// with the local name `val`, which means on (`CT_OnOff`), or only
/// `{root_prefix}:val`, set to `true`, `1` or `on`. Any other `val` can't
/// be read without resolving its prefix.
fn is_plainly_on(element: &Element, root_prefix: Option<&str>) -> bool {
    if element.prefix() != root_prefix {
        return false;
    }
    let mut vals = element
        .attr_names()
        .filter(|name| name.rsplit_once(':').map_or(*name, |(_, local)| local) == "val");
    match (vals.next(), vals.next()) {
        (None, _) => true,
        (Some(name), None) => {
            root_prefix.is_some_and(|prefix| name.strip_prefix(prefix) == Some(":val"))
                && element
                    .attr(name)
                    .is_some_and(|value| matches!(&*value, "true" | "1" | "on"))
        }
        (Some(_), Some(_)) => false,
    }
}

/// Return a `w:updateFields` element switched on, for a settings root
/// whose prefix is `root_prefix`: `<{p}:updateFields {p}:val="true"/>` with
/// that prefix.
///
/// Under a root without one, WordprocessingML is the default namespace, but
/// unprefixed attributes are in no namespace (Namespaces in XML 1.0, §6.2):
/// the element then declares and uses `w` itself.
fn new_update_fields(root_prefix: Option<&str>) -> Element {
    let prefix = root_prefix.unwrap_or("w");
    let mut element = Element::new(format!("{prefix}:updateFields"), Ns::W);
    if root_prefix.is_none() {
        element.set_attr("xmlns:w", W_URI);
    }
    element.set_attr(&format!("{prefix}:val"), "true");
    element
}

// ── Snippets ─────────────────────────────────────────────────────────────────

/// Parse a snippet generated by this crate: a drawing or a table.
///
/// # Panics
///
/// Panics if `xml` is not a single well-formed element. The generators
/// escape every value they write and declare every prefix they use, so
/// that would be a bug in them.
fn parse_snippet(xml: &str) -> Element {
    Element::parse_fragment(xml).expect("generated snippets are well-formed elements")
}

/// The `wp:docPr` ids of a document, and the source of new ones that no
/// other drawing uses: Word reports a document with duplicate ids as
/// corrupt.
#[derive(Clone, Debug, Default)]
struct DrawingIds {
    /// The ids the document used when opened, and those handed out since.
    used: BTreeSet<u32>,
    /// The next id to try.
    next: u32,
}

impl DrawingIds {
    /// Add the ids of the `wp:docPr` elements under `root` to the ids in
    /// use, and continue numbering after the largest.
    ///
    /// Ids that are not numbers (`ST_DrawingElementId` is an
    /// `xsd:unsignedInt`) can't clash with new ones and are ignored.
    fn collect(&mut self, root: &Element) {
        self.used.extend(
            root.descendants()
                .filter(|element| element.is(Ns::Wp, "docPr"))
                .filter_map(|element| element.attr("id")?.trim().parse::<u32>().ok()),
        );
        self.next = self
            .used
            .last()
            .map_or(1, |largest| largest.saturating_add(1));
    }

    /// Return an id that no drawing uses, and mark it used.
    ///
    /// Ids count up from one more than the largest used, like Word's. Past
    /// `u32::MAX` they wrap around to the gaps between used ids, skipping
    /// 0; a document holds far fewer than four billion drawings, so a free
    /// id is always found.
    fn allocate(&mut self) -> u32 {
        loop {
            let id = self.next;
            self.next = self.next.checked_add(1).unwrap_or(1);
            if id != 0 && self.used.insert(id) {
                return id;
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `xml` as an element.
    fn element(xml: &str) -> Element {
        Element::parse_fragment(xml).unwrap()
    }

    /// The namespace URIs, for building test inputs.
    const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    const WP: &str = "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";
    const RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

    #[test]
    fn drawing_ids_continue_after_the_largest() {
        // Ids that are not numbers, and `docPr` elements in no namespace,
        // are not drawing ids.
        let body = element(&format!(
            "<body xmlns:wp=\"{WP}\"><wp:docPr id=\"4\"/><x><wp:docPr id=\" 9 \"/></x>\
             <wp:docPr id=\"not a number\"/><wp:docPr/><docPr id=\"77\"/></body>"
        ));
        let mut ids = DrawingIds::default();
        ids.collect(&body);
        assert_eq!(ids.allocate(), 10);
        assert_eq!(ids.allocate(), 11);
    }

    #[test]
    fn drawing_ids_are_matched_by_namespace_not_prefix() {
        // The header has the largest id, under the prefix `w`, and a
        // larger one under `wp` bound to another namespace.
        let body = element(&format!(
            "<body xmlns:wp=\"{WP}\"><wp:docPr id=\"4\"/></body>"
        ));
        let header = element(&format!(
            "<hdr xmlns:w=\"{WP}\" xmlns:wp=\"urn:other\">\
             <w:docPr id=\"12\"/><wp:docPr id=\"20\"/></hdr>"
        ));
        let mut ids = DrawingIds::default();
        ids.collect(&body);
        ids.collect(&header);
        assert_eq!(ids.allocate(), 13);
        assert_eq!(ids.allocate(), 14);
    }

    #[test]
    fn drawing_ids_start_at_one() {
        let mut ids = DrawingIds::default();
        ids.collect(&element("<body/>"));
        assert_eq!(ids.allocate(), 1);
        assert_eq!(ids.allocate(), 2);
    }

    #[test]
    fn drawing_ids_wrap_around_to_free_ids() {
        let body = element(&format!(
            "<body xmlns:wp=\"{WP}\"><wp:docPr id=\"4294967295\"/><wp:docPr id=\"1\"/>\
             <wp:docPr id=\"3\"/></body>"
        ));
        let mut ids = DrawingIds::default();
        ids.collect(&body);
        assert_eq!(ids.allocate(), 2);
        assert_eq!(ids.allocate(), 4);
        assert_eq!(ids.allocate(), 5);
    }

    /// Zip `parts`, deflated.
    fn zip(parts: &[(&str, String)]) -> Bytes {
        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        for (name, content) in parts {
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file(*name, options).unwrap();
            writer.write_all(content.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner().into()
    }

    /// The parts of a document whose main part has the relationships
    /// `relationships`, with a header, `word/header1.xml`, last.
    fn parts(relationships: &str) -> Vec<(&'static str, String)> {
        vec![
            (
                "[Content_Types].xml",
                "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
                 <Default Extension=\"xml\" ContentType=\"application/xml\"/></Types>"
                    .to_owned(),
            ),
            (
                "_rels/.rels",
                format!(
                    "<Relationships xmlns=\"{RELS}\"><Relationship Id=\"rId1\" \
                     Type=\"{OFFICE_DOCUMENT}\" Target=\"word/document.xml\"/></Relationships>"
                ),
            ),
            (
                "word/document.xml",
                format!("<w:document xmlns:w=\"{W}\"><w:body><w:p/></w:body></w:document>"),
            ),
            (
                "word/_rels/document.xml.rels",
                format!("<Relationships xmlns=\"{RELS}\">{relationships}</Relationships>"),
            ),
            (
                "word/header1.xml",
                format!("<w:hdr xmlns:w=\"{W}\"><w:p>\n</w:p></w:hdr>"),
            ),
        ]
    }

    #[test]
    fn parts_read_share_one_budget() {
        // In the order they are read; the header comes last.
        let parts = parts(&format!(
            "<Relationship Id=\"rId1\" Type=\"{HEADER}\" Target=\"header1.xml\"/>"
        ));
        let zip = zip(&parts);

        // What each part takes, counted on its own.
        let bytes: usize = parts.iter().map(|(_, xml)| xml.len()).sum();
        let nodes: usize = parts
            .iter()
            .map(|(_, xml)| {
                let mut left = usize::MAX;
                XmlDocument::parse_with_budget(xml.as_bytes(), &mut left).unwrap();
                usize::MAX - left
            })
            .sum();
        assert!(nodes > 10);

        // Exactly enough for all of them together, and one less.
        Docx::open(zip.clone(), Budget { bytes, nodes }).unwrap();
        let err = Docx::open(
            zip.clone(),
            Budget {
                bytes,
                nodes: nodes - 1,
            },
        )
        .unwrap_err();
        assert!(
            matches!(&err, InvalidDocx::Xml { part, .. } if part == "word/header1.xml"),
            "{err:?}"
        );
        let err = Docx::open(
            zip,
            Budget {
                bytes: bytes - 1,
                nodes,
            },
        )
        .unwrap_err();
        assert!(matches!(err, InvalidDocx::Zip(_)), "{err:?}");
    }

    #[test]
    fn reserved_names_are_not_stories() {
        assert!(is_reserved(""));
        assert!(is_reserved("[Content_Types].xml"));
        assert!(is_reserved("[content_types].XML"));
        assert!(is_reserved("_rels/.rels"));
        assert!(is_reserved("word/_rels/document.xml.rels"));
        assert!(!is_reserved("word/header1.xml"));
        assert!(!is_reserved("word/rels.xml"));
    }

    #[test]
    fn relationships_to_package_parts_are_not_stories() {
        // A header or footer relationship to the content types part, to a
        // relationships part or to the package root is malformed:
        // following it would edit those parts as text.
        let parts = parts(&format!(
            "<Relationship Id=\"rId1\" Type=\"{HEADER}\" Target=\"/[Content_Types].xml\"/>\
             <Relationship Id=\"rId2\" Type=\"{FOOTER}\" Target=\"_rels/document.xml.rels\"/>\
             <Relationship Id=\"rId3\" Type=\"{FOOTER}\" Target=\"/\"/>\
             <Relationship Id=\"rId4\" Type=\"{HEADER}\" Target=\"header1.xml\"/>"
        ));
        let docx = Docx::from_bytes(zip(&parts)).unwrap();
        let stories: Vec<&str> = docx.stories.iter().map(|story| &*story.part.name).collect();
        assert_eq!(stories, ["word/document.xml", "word/header1.xml"]);
    }

    #[test]
    fn invalid_placeholder_gives_back_the_placeholder() {
        let err = InvalidPlaceholder("{{items}}".to_owned());
        assert_eq!(
            err.to_string(),
            "table placeholder `{{items}}` must be the only content of its paragraph"
        );
        assert_eq!(err.as_str(), "{{items}}");
        assert_eq!(err.into_inner(), "{{items}}");
    }

    #[test]
    fn error_messages_escape_part_names() {
        assert_eq!(
            InvalidDocx::MissingPart("word/header1.xml".to_owned()).to_string(),
            "missing part `word/header1.xml`"
        );
        assert_eq!(
            InvalidDocx::MissingPart("a\nb\u{202e}".to_owned()).to_string(),
            "missing part `a\\nb\\u{202e}`"
        );
    }
}
