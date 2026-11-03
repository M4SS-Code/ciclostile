//! Replacement engine: find text in WordprocessingML paragraphs and
//! splice new content in its place.
//!
//! # Paragraph text model
//!
//! Word splits the text of a paragraph into runs wherever formatting,
//! revision marks or proofing state change, so a placeholder typed in one
//! go can end up as `{{cus` in one run and `tomer}}` in the next, with a
//! `w:proofErr` or a bookmark between them. Matching therefore works on
//! the text of whole paragraphs (`w:p`, wherever they are in the tree):
//!
//! - every `w:t` child of a run (`w:r`) is a *text atom*, holding the text
//!   Word displays: without `xml:space="preserve"`, Word ignores its
//!   leading and trailing whitespace, and so does matching;
//! - every other child of a run, except its properties (`w:rPr`) and
//!   `w:lastRenderedPageBreak`, is a *barrier* (`w:tab`, `w:br`,
//!   `w:drawing`, `w:fldChar`, `w:instrText`, `w:sym`,
//!   `mc:AlternateContent`, ...): no occurrence spans one;
//! - between runs, the runs in `w:hyperlink`, `w:ins`, `w:moveTo`,
//!   `w:smartTag`, `w:sdt`, `w:customXml`, `w:fldSimple`, `w:dir` and
//!   `w:bdo` belong to the paragraph, and zero-width markers
//!   (`w:proofErr`, and the bounds of bookmarks, comments, permissions and
//!   revisions) are ignored;
//! - properties (`w:pPr`, `w:sdtPr`, `w:smartTagPr`, ...) and deleted or
//!   moved-away revisions (`w:del`, `w:moveFrom`) are not text, and are
//!   never searched nor edited;
//! - the branches of an `mc:AlternateContent` between runs hold
//!   alternative versions of the same content, so barriers separate them;
//! - anything else between runs, such as an equation (`m:oMath`), a
//!   sub-document (`w:subDoc`) or an extension element, is a barrier, and
//!   the runs it may hold are not part of the paragraph;
//! - a paragraph inside a paragraph, such as one in a text box anchored in
//!   one of its runs, is a paragraph of its own (in each copy of the text
//!   box: `mc:Choice` and `mc:Fallback`), inside a barrier of the outer
//!   one.
//!
//! # Matching and splicing
//!
//! Each barrier-free stretch of a paragraph's text is searched once,
//! before anything changes: occurrences never overlap, the leftmost wins,
//! then the longest key. Inserted content is never searched.
//!
//! The new content goes into the run holding the first character of an
//! occurrence, where the occurrence starts, so it takes that run's
//! formatting: the `w:t` keeps the text before the occurrence, and the new
//! content and the rest of its text follow it in the same run. The other
//! characters of the occurrence are cut from the atoms holding them; a
//! `w:t` left empty is removed, its run is kept. Edited and new `w:t`
//! elements hold the text Word displayed and preserve spaces
//! (`xml:space="preserve"`), so whitespace Word ignored stays out of
//! sight; new elements take the prefix of their run. Everything else
//! keeps its bytes.
//!
//! Paragraphs are processed in document order, nested ones when the walk
//! of their outer paragraph reaches them, so replacement content is made
//! in document order too.
//!
//! # Limits
//!
//! What the engine writes must parse again, so edited trees stay within
//! [`MAX_DEPTH`] levels and [`MAX_NAMESPACE_BINDINGS`] namespace
//! declarations in scope. An element or a table that would end up deeper,
//! or under more declarations (counted, not matched against those in
//! scope), is not inserted: its occurrence is left as it is, and not
//! counted. Text replacements add elements next to the ones they edit, so
//! they never go deeper, and the names of those elements always fit
//! [`MAX_NAME_LEN`](crate::xml::MAX_NAME_LEN): a prefix is at most 250
//! bytes long, as the attribute declaring it is a name too.

use std::{borrow::Cow, cmp::Reverse, ops::Range};

use crate::xml::{
    Element, MAX_DEPTH, MAX_NAMESPACE_BINDINGS, Node, Ns, escape_text, qualified_name,
};

// ── Entry points ─────────────────────────────────────────────────────────────

/// Replace every occurrence of the keys of `replacements` in every
/// paragraph under `root` with the corresponding value, in a single pass.
///
/// Occurrences are found as the module docs describe: the leftmost wins,
/// then the longest key, then the pair listed first, and values are never
/// searched for keys. Empty keys are ignored. In values, line breaks
/// (`\r\n`, `\r` or `\n`) become `w:br` elements and tabs `w:tab`
/// elements; characters XML cannot represent are dropped.
///
/// Returns the number of occurrences replaced.
pub(crate) fn replace_text(root: &mut Element, replacements: &[(&str, &str)]) -> usize {
    let pairs: Vec<(&str, &str)> = replacements
        .iter()
        .copied()
        .filter(|(key, _)| !key.is_empty())
        .collect();
    let keys = pairs.iter().map(|&(key, _)| key).collect();
    // Text always fits: see the module docs.
    Replacer::new(keys, |index, _, pieces: &mut Pieces| {
        if let Some(&(_, value)) = pairs.get(index) {
            pieces.push_value(value);
        }
        true
    })
    .run(root)
}

/// Replace every occurrence of `placeholder` in every paragraph under `root`
/// with the run content returned by `make`, which is called once per
/// occurrence replaced, in document order.
///
/// Occurrences are found as by [`replace_text`], and the element goes into
/// the run holding the first character of each one. An empty `placeholder`
/// matches nothing.
///
/// An occurrence where the element would be nested deeper than
/// [`MAX_DEPTH`] levels, or under more than [`MAX_NAMESPACE_BINDINGS`]
/// namespace declarations, is left as it is and not counted: the part
/// could not be parsed again. `make` is not called for it if an element
/// like the last one made would not fit; if none was made yet, the one it
/// returns is dropped.
///
/// Returns the number of occurrences replaced.
pub(crate) fn replace_with_element(
    root: &mut Element,
    placeholder: &str,
    mut make: impl FnMut() -> Element,
) -> usize {
    let keys = if placeholder.is_empty() {
        Vec::new()
    } else {
        vec![placeholder]
    };
    // The shape of the elements `make` returns, once it returned one.
    let mut shape = None;
    Replacer::new(keys, |_, room: Room, pieces: &mut Pieces| {
        if shape.is_some_and(|shape| !room.fits(shape)) {
            return false;
        }
        let element = make();
        let size = Shape::of(&element);
        shape = Some(size);
        if !room.fits(size) {
            return false;
        }
        pieces.push_element(element);
        true
    })
    .run(root)
}

/// Return whether every paragraph under `root` that contains `placeholder`
/// consists of nothing but the placeholder, so it can be replaced by a
/// table.
///
/// The placeholder is trimmed of whitespace first; nothing is left of
/// one made only of whitespace, which is then in no paragraph. A
/// paragraph contains the placeholder when [`replace_text`] would find it
/// there. It consists of nothing but the placeholder when its text,
/// trimmed of whitespace, is the placeholder, and it holds nothing a
/// table taking its place would lose: only properties, zero-width
/// markers, and runs of text, in wrappers or not. Barriers, equations,
/// content controls (`w:sdt`), alternate content and tracked deletions
/// all make a paragraph more than its text.
pub(crate) fn table_placeholders_are_alone(root: &Element, placeholder: &str) -> bool {
    let placeholder = placeholder.trim();
    placeholder.is_empty()
        || every_paragraph(root, &mut |paragraph| {
            let model = TextModel::new(paragraph);
            model.is_only(placeholder) || !model.contains(placeholder)
        })
}

/// Replace every paragraph under `root` whose only content is `placeholder`
/// with a copy of `table`.
///
/// The placeholder is trimmed, and paragraphs qualify, as described in
/// [`table_placeholders_are_alone`]; paragraphs that merely contain the
/// placeholder are left alone. Word merges adjacent tables and wants
/// every table followed by a paragraph (a table cell, like the body, must
/// end with one), so, zero-width markers between blocks aside:
///
/// - the placeholder paragraph, emptied of everything but its `w:pPr`,
///   stays after the table when it ends a section (its `w:pPr` holds a
///   `w:sectPr`) or when the next block is not a `w:p`;
/// - an empty paragraph goes before the table when the previous block is
///   a `w:tbl`.
///
/// No range loses one of its ends: the range markers of the placeholder
/// paragraph (the bounds of bookmarks, comments, permissions and
/// revisions) move right before the table, except that the ends stay in
/// the paragraph when it stays. Either way the table takes the place of
/// the placeholder in the ranges around it, so a page reference to a
/// bookmark around the placeholder gives the page where the table starts:
/// LibreOffice can't resolve one to a bookmark that starts after it.
///
/// A paragraph where the table would be nested deeper than [`MAX_DEPTH`]
/// levels, or under more than [`MAX_NAMESPACE_BINDINGS`] namespace
/// declarations, is left as it is and not counted: the part could not be
/// parsed again. So a table whose cell holds the placeholder again, put in
/// over and over, stops going deeper there.
///
/// Returns the number of paragraphs replaced.
pub(crate) fn replace_paragraphs_with_table(
    root: &mut Element,
    placeholder: &str,
    table: &Element,
) -> usize {
    let placeholder = placeholder.trim();
    if placeholder.is_empty() {
        return 0;
    }
    let replacement = TableReplacement {
        placeholder,
        table,
        shape: Shape::of(table),
    };
    replacement.replace_in(root, Room::of_root(root))
}

// ── Run text ─────────────────────────────────────────────────────────────────

/// A piece of plain text written as run content, see [`split_run_text`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunText<'a> {
    /// Text without line breaks or tabs, never empty, still unescaped: it
    /// goes into a `w:t`.
    Text(&'a str),
    /// A line break, written `w:br`.
    Break,
    /// A tab, written `w:tab`.
    Tab,
}

/// Split `text` into the run content that renders it: each line break
/// (`\r\n`, `\r` or `\n`) and each tab is a piece of its own, and the text
/// between them is a [`RunText::Text`].
///
/// The text is returned as is: escaping it, and dropping the characters
/// XML cannot represent, is up to the caller.
pub(crate) fn split_run_text(text: &str) -> impl Iterator<Item = RunText<'_>> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = rest.find(['\r', '\n', '\t']).unwrap_or(rest.len());
        let (piece, tail) = if end > 0 {
            let (text, tail) = rest.split_at_checked(end)?;
            (RunText::Text(text), tail)
        } else if let Some(tail) = rest.strip_prefix('\t') {
            (RunText::Tab, tail)
        } else {
            // `\r\n` is a single line break.
            let tail = rest
                .strip_prefix("\r\n")
                .or_else(|| rest.strip_prefix(['\r', '\n']))?;
            (RunText::Break, tail)
        };
        rest = tail;
        Some(piece)
    })
}

// ── Paragraph text model ─────────────────────────────────────────────────────

/// Local names of the WordprocessingML elements that hold properties or
/// layout hints but no content: the properties of paragraphs, runs,
/// content controls, smart tags, custom XML and simple fields, and
/// `w:lastRenderedPageBreak`, which records where a page ended when the
/// document was last laid out.
const PROPERTIES: [&str; 8] = [
    "pPr",
    "rPr",
    "sdtPr",
    "sdtEndPr",
    "smartTagPr",
    "customXmlPr",
    "fldData",
    "lastRenderedPageBreak",
];

/// Local names of the WordprocessingML elements whose runs belong to
/// the paragraph around them: hyperlinks, tracked insertions and moves,
/// smart tags, content controls, custom XML, simple fields, and
/// bidirectional embeddings and overrides.
const WRAPPERS: [&str; 10] = [
    "hyperlink",
    "ins",
    "moveTo",
    "smartTag",
    "sdt",
    "sdtContent",
    "customXml",
    "fldSimple",
    "dir",
    "bdo",
];

/// Local names of the zero-width markers that may stand between runs, and
/// between blocks: proofing errors and range markup, the bounds of
/// bookmarks, comments, editing permissions and moved or custom XML
/// revisions (ECMA-376 Part 1, §17.13).
const MARKERS: [&str; 19] = [
    "proofErr",
    "bookmarkStart",
    "bookmarkEnd",
    "commentRangeStart",
    "commentRangeEnd",
    "permStart",
    "permEnd",
    "moveFromRangeStart",
    "moveFromRangeEnd",
    "moveToRangeStart",
    "moveToRangeEnd",
    "customXmlInsRangeStart",
    "customXmlInsRangeEnd",
    "customXmlDelRangeStart",
    "customXmlDelRangeEnd",
    "customXmlMoveFromRangeStart",
    "customXmlMoveFromRangeEnd",
    "customXmlMoveToRangeStart",
    "customXmlMoveToRangeEnd",
];

/// Local names of the WordprocessingML elements that hold deleted or
/// moved-away revisions (ECMA-376 Part 1, §17.13.5): content that is no
/// longer document text.
const REMOVED: [&str; 2] = ["del", "moveFrom"];

/// Whether `element` is a WordprocessingML element named by one of
/// `names`.
fn is_w(element: &Element, names: &[&str]) -> bool {
    element.ns() == Ns::W && names.contains(&element.local_name())
}

/// Whether `element` holds no document text: properties (see
/// [`PROPERTIES`]) or deleted or moved-away revisions (see [`REMOVED`]).
/// Its subtree is never searched nor edited.
fn is_hidden(element: &Element) -> bool {
    is_w(element, &PROPERTIES) || is_w(element, &REMOVED)
}

/// Whether `element` is a zero-width marker, see [`MARKERS`].
fn is_marker(element: &Element) -> bool {
    is_w(element, &MARKERS)
}

/// What an element inside a paragraph, outside runs, is to the text model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Inline {
    /// A run, whose `w:t` children are text atoms.
    Run,
    /// A nested paragraph: a barrier, and a paragraph of its own.
    Paragraph,
    /// A wrapper, whose runs belong to the paragraph, see [`WRAPPERS`].
    Wrapper,
    /// `mc:AlternateContent`: its branches are separated by barriers.
    Alternate,
    /// A zero-width marker, which is ignored, see [`MARKERS`].
    Marker,
    /// Properties, without content, see [`PROPERTIES`].
    Properties,
    /// A deleted or moved-away revision, see [`REMOVED`]: not text, but
    /// content a table taking the paragraph's place would lose.
    Removed,
    /// Anything else: a barrier, whose runs are not part of the paragraph.
    Barrier,
}

impl Inline {
    /// Classify `element`.
    fn of(element: &Element) -> Self {
        if element.is(Ns::Mc, "AlternateContent") {
            return Self::Alternate;
        }
        if element.ns() != Ns::W {
            return Self::Barrier;
        }
        match element.local_name() {
            "r" => Self::Run,
            "p" => Self::Paragraph,
            local if REMOVED.contains(&local) => Self::Removed,
            local if WRAPPERS.contains(&local) => Self::Wrapper,
            local if MARKERS.contains(&local) => Self::Marker,
            local if PROPERTIES.contains(&local) => Self::Properties,
            _ => Self::Barrier,
        }
    }
}

/// What a child element of a run is to the text model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunChild {
    /// A `w:t` holding only text: a text atom. A `w:t` with child
    /// elements, which the schema doesn't allow, is a barrier instead, so
    /// rewriting its text never drops them.
    Text,
    /// Run properties and other elements without content, see
    /// [`PROPERTIES`].
    Ignored,
    /// Anything else: a barrier.
    Barrier,
}

impl RunChild {
    /// Classify `element`.
    fn of(element: &Element) -> Self {
        if element.is(Ns::W, "t") && element.child_elements().next().is_none() {
            Self::Text
        } else if is_w(element, &PROPERTIES) {
            Self::Ignored
        } else {
            Self::Barrier
        }
    }
}

/// Return the part of `text`, the text of the `w:t` element `t`, that
/// Word displays: without `xml:space="preserve"`, Word drops leading and
/// trailing whitespace, as XML 1.0 §2.10 lets applications do.
fn shown_text<'a>(t: &Element, text: &'a str) -> &'a str {
    if t.attr("xml:space").is_some_and(|space| space == "preserve") {
        text
    } else {
        text.trim_matches([' ', '\t', '\n', '\r'])
    }
}

/// The text of one paragraph, as matching sees it: its atoms and barriers,
/// nested paragraphs excluded.
#[derive(Debug, Default)]
struct TextModel {
    /// The text of every atom, unescaped, in document order.
    text: String,
    /// The byte range of each atom in `text`, in document order.
    atoms: Vec<Range<usize>>,
    /// The barrier-free stretches of `text`, in order; none is empty.
    segments: Vec<Range<usize>>,
    /// Where the segment being collected starts in `text`.
    segment_start: usize,
    /// Whether the paragraph holds more than text, properties and markers:
    /// a barrier, a content control, alternate content or a tracked
    /// deletion, which a table taking its place would lose.
    more_than_text: bool,
}

impl TextModel {
    /// Build the text model of `paragraph`.
    fn new(paragraph: &Element) -> Self {
        let mut model = Self::default();
        model.collect_children(paragraph);
        model.end_segment();
        model
    }

    /// Collect the children of `container`, see [`collect`](Self::collect).
    fn collect_children(&mut self, container: &Element) {
        for child in container.child_elements() {
            self.collect(child);
        }
    }

    /// Collect the atoms and barriers at or under `element`, which is part
    /// of the paragraph. [`Replacer::walk`] must meet the atoms in the same
    /// order.
    fn collect(&mut self, element: &Element) {
        match Inline::of(element) {
            Inline::Run => self.collect_run(element),
            Inline::Wrapper => {
                // A content control would go with the paragraph.
                if is_w(element, &["sdt", "sdtContent"]) {
                    self.more_than_text = true;
                }
                self.collect_children(element);
            }
            Inline::Alternate => {
                for branch in element.child_elements() {
                    self.add_barrier();
                    self.collect_children(branch);
                }
                self.add_barrier();
            }
            Inline::Marker | Inline::Properties => {}
            Inline::Removed => self.more_than_text = true,
            Inline::Paragraph | Inline::Barrier => self.add_barrier(),
        }
    }

    /// Collect the atoms and barriers of `run`.
    fn collect_run(&mut self, run: &Element) {
        for child in run.child_elements() {
            match RunChild::of(child) {
                RunChild::Text => {
                    let start = self.text.len();
                    self.text.push_str(shown_text(child, &child.text()));
                    self.atoms.push(start..self.text.len());
                }
                RunChild::Ignored => {}
                RunChild::Barrier => self.add_barrier(),
            }
        }
    }

    /// Record a barrier, which ends the current segment.
    fn add_barrier(&mut self) {
        self.more_than_text = true;
        self.end_segment();
    }

    /// End the current segment, keeping it if it isn't empty.
    fn end_segment(&mut self) {
        if self.text.len() > self.segment_start {
            self.segments.push(self.segment_start..self.text.len());
        }
        self.segment_start = self.text.len();
    }

    /// Iterate over the text of the segments.
    fn segment_texts(&self) -> impl Iterator<Item = (usize, &str)> {
        self.segments.iter().filter_map(|segment| {
            let text = self.text.get(segment.clone())?;
            Some((segment.start, text))
        })
    }

    /// Return the occurrences of `keys`, in order, see [`find_in`].
    fn find(&self, keys: &[&str]) -> Vec<Match> {
        let mut matches = Vec::new();
        let mut candidates = Vec::new();
        for (offset, text) in self.segment_texts() {
            find_in(text, keys, offset, &mut candidates, &mut matches);
        }
        matches
    }

    /// Whether `placeholder` occurs in the paragraph.
    fn contains(&self, placeholder: &str) -> bool {
        self.segment_texts()
            .any(|(_, text)| text.contains(placeholder))
    }

    /// Whether the paragraph holds nothing but `placeholder`: text that is
    /// the placeholder once trimmed of whitespace, and nothing more than
    /// text (see [`more_than_text`](Self::more_than_text)).
    fn is_only(&self, placeholder: &str) -> bool {
        !self.more_than_text && self.text.trim() == placeholder
    }
}

// ── Matching ─────────────────────────────────────────────────────────────────

/// An occurrence of a key in the text of a paragraph.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Match {
    /// The byte range of the occurrence in [`TextModel::text`].
    range: Range<usize>,
    /// The index of the key.
    key: usize,
}

/// A key that occurs in a segment, while [`find_in`] searches it.
#[derive(Clone, Copy, Debug)]
struct Candidate {
    /// The index of the key.
    key: usize,
    /// The length of the key.
    len: usize,
    /// Where the occurrence of the key found last starts; it is searched
    /// again once a match ends past it.
    start: usize,
}

/// Text to remove from one atom, and whether the content replacing an
/// occurrence goes in its place.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Cut {
    /// The ordinal of the atom in the paragraph.
    atom: usize,
    /// The byte range to remove from the text of the atom.
    range: Range<usize>,
    /// The key whose replacement goes where the range starts, on the cut
    /// that holds the first character of an occurrence.
    key: Option<usize>,
    /// The index of the occurrence the cut belongs to.
    occurrence: usize,
}

/// Append the occurrences of `keys` in `text` to `matches`, their ranges
/// shifted by `offset`; `candidates` is scratch space.
///
/// At the leftmost position where any key occurs, the longest key wins
/// (the first listed on a tie), and the search resumes after it, so
/// occurrences never overlap. A key is searched again only once a match
/// ends past the occurrence it had found, from there on, so every key
/// scans the text about once. Keys longer than the text, and empty keys,
/// are not searched at all: setting up a search costs about as much as a
/// short one, and a paragraph may hold many short segments.
fn find_in(
    text: &str,
    keys: &[&str],
    offset: usize,
    candidates: &mut Vec<Candidate>,
    matches: &mut Vec<Match>,
) {
    candidates.clear();
    for (index, key) in keys.iter().enumerate() {
        if key.is_empty() || key.len() > text.len() {
            continue;
        }
        if let Some(start) = text.find(key) {
            candidates.push(Candidate {
                key: index,
                len: key.len(),
                start,
            });
        }
    }

    let mut from = 0;
    loop {
        candidates.retain_mut(|candidate| {
            if candidate.start >= from {
                return true;
            }
            let found = keys
                .get(candidate.key)
                .and_then(|key| text.get(from..)?.find(key));
            match found {
                Some(start) => {
                    candidate.start = from + start;
                    true
                }
                None => false,
            }
        });
        let Some(best) = candidates
            .iter()
            .min_by_key(|candidate| (candidate.start, Reverse(candidate.len), candidate.key))
        else {
            break;
        };
        let start = best.start;
        from = start + best.len;
        matches.push(Match {
            range: offset + start..offset + from,
            key: best.key,
        });
    }
}

/// Turn `matches`, sorted and disjoint, into the cuts they make in the
/// `atoms` holding them, sorted by atom and position.
fn cuts(atoms: &[Range<usize>], matches: &[Match]) -> Vec<Cut> {
    let mut cuts = Vec::new();
    // The first atom that can hold the start of the next occurrence.
    let mut first = 0;
    for (occurrence, found) in matches.iter().enumerate() {
        while atoms
            .get(first)
            .is_some_and(|atom| atom.end <= found.range.start)
        {
            first += 1;
        }
        let mut key = Some(found.key);
        for (index, atom) in atoms.iter().enumerate().skip(first) {
            if atom.start >= found.range.end {
                break;
            }
            let start = atom.start.max(found.range.start);
            let end = atom.end.min(found.range.end);
            // Empty atoms hold nothing to cut.
            if start < end {
                cuts.push(Cut {
                    atom: index,
                    range: start - atom.start..end - atom.start,
                    key: key.take(),
                    occurrence,
                });
            }
        }
    }
    cuts
}

// ── Limits ───────────────────────────────────────────────────────────────────

/// Where an element is in its tree, as far as the parser's limits go: how
/// deep, and how many namespace declarations are in scope there, its own
/// included. What goes under it must stay within [`MAX_DEPTH`] and
/// [`MAX_NAMESPACE_BINDINGS`], or the part could not be parsed again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Room {
    /// The depth of the element, the root's being 1.
    depth: usize,
    /// The namespace declarations of the element and its ancestors.
    bindings: usize,
}

impl Room {
    /// Return the room of `root`, the root element of its tree.
    fn of_root(root: &Element) -> Self {
        Self {
            depth: 1,
            bindings: root.namespace_declarations(),
        }
    }

    /// Return the room of `child`, a child of the element this is the room
    /// of.
    fn child(self, child: &Element) -> Self {
        Self {
            depth: self.depth.saturating_add(1),
            bindings: self.bindings.saturating_add(child.namespace_declarations()),
        }
    }

    /// Whether a tree of `shape` fits under the element this is the room
    /// of, as one of its children.
    fn fits(self, shape: Shape) -> bool {
        self.depth.saturating_add(shape.height) <= MAX_DEPTH
            && self.bindings.saturating_add(shape.declarations) <= MAX_NAMESPACE_BINDINGS
    }
}

/// How far a tree of elements reaches towards the parser's limits once it
/// is inserted somewhere, see [`Room::fits`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shape {
    /// How many levels deep the tree is, its root counting as one.
    height: usize,
    /// The most namespace declarations on a path from its root down.
    declarations: usize,
}

impl Shape {
    /// Measure `tree`.
    fn of(tree: &Element) -> Self {
        let mut shape = Self {
            height: 0,
            declarations: 0,
        };
        // An explicit stack: the tree comes from outside the engine.
        let mut stack = vec![(tree, 1_usize, tree.namespace_declarations())];
        while let Some((element, height, declarations)) = stack.pop() {
            shape.height = shape.height.max(height);
            shape.declarations = shape.declarations.max(declarations);
            for child in element.child_elements() {
                stack.push((
                    child,
                    height.saturating_add(1),
                    declarations.saturating_add(child.namespace_declarations()),
                ));
            }
        }
        shape
    }
}

// ── Splicing ─────────────────────────────────────────────────────────────────

/// One piece of new run content.
#[derive(Debug)]
enum Piece {
    /// Text, unescaped, for a `w:t`.
    Text(String),
    /// A line break, `w:br`.
    Break,
    /// A tab, `w:tab`.
    Tab,
    /// Any other run content, such as a `w:drawing`.
    Element(Element),
}

/// The new content of an edited atom, in order. Adjacent text is merged,
/// so it fills a single `w:t`.
#[derive(Debug, Default)]
struct Pieces(Vec<Piece>);

impl Pieces {
    /// Append `text`, merged with the text before it.
    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(Piece::Text(last)) = self.0.last_mut() {
            last.push_str(text);
        } else {
            self.0.push(Piece::Text(text.to_owned()));
        }
    }

    /// Append the plain text `value`, its line breaks and tabs turned into
    /// `w:br` and `w:tab`.
    fn push_value(&mut self, value: &str) {
        for piece in split_run_text(value) {
            match piece {
                RunText::Text(text) => self.push_text(text),
                RunText::Break => self.0.push(Piece::Break),
                RunText::Tab => self.0.push(Piece::Tab),
            }
        }
    }

    /// Append `element`.
    fn push_element(&mut self, element: Element) {
        self.0.push(Piece::Element(element));
    }

    /// Append the pieces to `out`, as the run content that takes the place
    /// of the edited atom `atom`, in a run whose prefix is `prefix`.
    ///
    /// The first text reuses `atom`, so it keeps its attributes; further
    /// text goes into new `w:t` elements. Text made only of characters XML
    /// cannot represent is dropped with them, and `atom` is dropped too if
    /// no text is left.
    fn splice_into(self, atom: Element, prefix: Option<&str>, out: &mut Vec<Node>) {
        let mut atom = Some(atom);
        for piece in self.0 {
            let element = match piece {
                Piece::Text(text) => {
                    // Escaping borrows only text it leaves as it is, which
                    // is then moved rather than copied.
                    let text = match escape_text(&text) {
                        Cow::Borrowed(_) => text,
                        Cow::Owned(escaped) => escaped,
                    };
                    if text.is_empty() {
                        continue;
                    }
                    let mut element = atom
                        .take()
                        .unwrap_or_else(|| Element::new(qualified_name(prefix, "t"), Ns::W));
                    element.children = vec![Node::Text(text)];
                    // Otherwise leading and trailing spaces would be dropped
                    // (see `shown_text`).
                    element.set_attr("xml:space", "preserve");
                    element
                }
                Piece::Break => Element::new(qualified_name(prefix, "br"), Ns::W),
                Piece::Tab => Element::new(qualified_name(prefix, "tab"), Ns::W),
                Piece::Element(element) => element,
            };
            out.push(Node::Element(element));
        }
    }
}

/// Where the splicing walk of a paragraph is: the next atom, and the cuts
/// not applied yet.
#[derive(Debug)]
struct Cursor<'a> {
    /// The text model of the paragraph.
    model: &'a TextModel,
    /// The cuts still to apply, sorted by atom.
    cuts: &'a [Cut],
    /// The ordinal of the next atom.
    atom: usize,
    /// Whether each occurrence was skipped, its content not fitting where
    /// it would go: its text stays where it is.
    skipped: Vec<bool>,
}

impl<'a> Cursor<'a> {
    /// Move past the next atom; return its text and its cuts.
    fn next_atom(&mut self) -> (&'a str, &'a [Cut]) {
        let atom = self.atom;
        self.atom += 1;
        let model = self.model;
        let text = model
            .atoms
            .get(atom)
            .and_then(|range| model.text.get(range.clone()))
            .unwrap_or_default();
        let count = self.cuts.iter().take_while(|cut| cut.atom == atom).count();
        let (cuts, rest) = self.cuts.split_at_checked(count).unwrap_or_default();
        self.cuts = rest;
        (text, cuts)
    }

    /// Leave the occurrence `occurrence` as it is.
    fn skip(&mut self, occurrence: usize) {
        if let Some(skipped) = self.skipped.get_mut(occurrence) {
            *skipped = true;
        }
    }

    /// Whether the occurrence `occurrence` is left as it is.
    fn is_skipped(&self, occurrence: usize) -> bool {
        self.skipped.get(occurrence).copied().unwrap_or(false)
    }
}

/// A replacement pass over a tree: the keys to find, and what replaces
/// them.
struct Replacer<'k, F> {
    /// The keys to find; none is empty.
    keys: Vec<&'k str>,
    /// Append the content replacing an occurrence of the key at the given
    /// index to the pieces, for a run with the given room; return whether
    /// it did, rather than append nothing as the content would not fit.
    content: F,
    /// The number of occurrences replaced so far.
    count: usize,
}

impl<'k, F: FnMut(usize, Room, &mut Pieces) -> bool> Replacer<'k, F> {
    /// Create a pass that replaces the occurrences of `keys` with what
    /// `content` appends.
    fn new(keys: Vec<&'k str>, content: F) -> Self {
        debug_assert!(keys.iter().all(|key| !key.is_empty()), "empty key");
        Self {
            keys,
            content,
            count: 0,
        }
    }

    /// Replace the keys in every paragraph at or under `root`; return the
    /// number of occurrences replaced.
    fn run(mut self, root: &mut Element) -> usize {
        if !self.keys.is_empty() {
            self.visit(root, Room::of_root(root));
        }
        self.count
    }

    /// Process every paragraph at or under `element`, whose room is `room`,
    /// hidden content aside.
    fn visit(&mut self, element: &mut Element, room: Room) {
        if is_hidden(element) {
            return;
        }
        if element.is(Ns::W, "p") {
            self.paragraph(element, room);
        } else {
            for child in element.child_elements_mut() {
                let room = room.child(child);
                self.visit(child, room);
            }
        }
    }

    /// Replace the keys in `paragraph` and in the paragraphs nested in it.
    ///
    /// The occurrences in `paragraph` are all found first, on its original
    /// text; then a walk in document order applies them and processes the
    /// nested paragraphs as it reaches them.
    fn paragraph(&mut self, paragraph: &mut Element, room: Room) {
        let model = TextModel::new(paragraph);
        let matches = model.find(&self.keys);
        let cuts = cuts(&model.atoms, &matches);
        let mut cursor = Cursor {
            model: &model,
            cuts: &cuts,
            atom: 0,
            skipped: vec![false; matches.len()],
        };
        self.walk_children(paragraph, room, &mut cursor);
        debug_assert!(
            cursor.atom == model.atoms.len() && cursor.cuts.is_empty(),
            "the walk met other atoms than the text model"
        );
    }

    /// Walk the children of `container`, whose room is `room`, see
    /// [`walk`](Self::walk).
    fn walk_children(&mut self, container: &mut Element, room: Room, cursor: &mut Cursor<'_>) {
        for child in container.child_elements_mut() {
            let room = room.child(child);
            self.walk(child, room, cursor);
        }
    }

    /// Apply the cuts to the atoms at or under `element`, whose room is
    /// `room` and which is part of the paragraph `cursor` walks, and
    /// process the paragraphs nested there. Meets the atoms in the order of
    /// [`TextModel::collect`].
    fn walk(&mut self, element: &mut Element, room: Room, cursor: &mut Cursor<'_>) {
        match Inline::of(element) {
            Inline::Run => self.walk_run(element, room, cursor),
            Inline::Paragraph => self.paragraph(element, room),
            Inline::Wrapper => self.walk_children(element, room, cursor),
            Inline::Alternate => {
                for branch in element.child_elements_mut() {
                    let room = room.child(branch);
                    self.walk_children(branch, room, cursor);
                }
            }
            Inline::Properties | Inline::Removed => {}
            // Not text, but it may hold paragraphs, such as a text box.
            Inline::Marker | Inline::Barrier => self.visit(element, room),
        }
    }

    /// Apply the cuts to the atoms of `run`, whose room is `room`, and
    /// process the paragraphs nested in its barriers.
    fn walk_run(&mut self, run: &mut Element, room: Room, cursor: &mut Cursor<'_>) {
        // The new content is made in document order, and spliced in at the
        // end in a single pass over the children.
        let mut edits = Vec::new();
        for (index, node) in run.children.iter_mut().enumerate() {
            let Node::Element(child) = node else {
                continue;
            };
            match RunChild::of(child) {
                RunChild::Text => {
                    let (text, cuts) = cursor.next_atom();
                    if !cuts.is_empty()
                        && let Some(pieces) = self.pieces(text, cuts, room, cursor)
                    {
                        edits.push((index, pieces));
                    }
                }
                RunChild::Ignored => {}
                RunChild::Barrier => {
                    let room = room.child(child);
                    self.visit(child, room);
                }
            }
        }
        if edits.is_empty() {
            return;
        }

        let children = std::mem::take(&mut run.children);
        let prefix = run.prefix();
        let mut spliced = Vec::with_capacity(children.len() + edits.len());
        let mut edits = edits.into_iter().peekable();
        for (index, node) in children.into_iter().enumerate() {
            match (node, edits.next_if(|&(at, _)| at == index)) {
                (Node::Element(atom), Some((_, pieces))) => {
                    pieces.splice_into(atom, prefix, &mut spliced);
                }
                (node, _) => spliced.push(node),
            }
        }
        run.children = spliced;
    }

    /// Return the new content of an atom whose text is `text`, in a run
    /// whose room is `room`, once `cuts` are applied: the text outside the
    /// cuts, with the replacement content where cuts with a key start.
    ///
    /// An occurrence whose content does not fit is skipped: its text
    /// stays, here and in the atoms after. Returns `None` when no cut
    /// applies, so that the atom keeps its bytes.
    fn pieces(
        &mut self,
        text: &str,
        cuts: &[Cut],
        room: Room,
        cursor: &mut Cursor<'_>,
    ) -> Option<Pieces> {
        let mut pieces = Pieces::default();
        let mut changed = false;
        let mut kept = 0;
        for cut in cuts {
            pieces.push_text(text.get(kept..cut.range.start).unwrap_or_default());
            let applies = match cut.key {
                Some(key) => {
                    let replaced = (self.content)(key, room, &mut pieces);
                    if replaced {
                        self.count += 1;
                    } else {
                        cursor.skip(cut.occurrence);
                    }
                    replaced
                }
                None => !cursor.is_skipped(cut.occurrence),
            };
            if applies {
                changed = true;
            } else {
                pieces.push_text(text.get(cut.range.clone()).unwrap_or_default());
            }
            kept = cut.range.end;
        }
        pieces.push_text(text.get(kept..).unwrap_or_default());
        changed.then_some(pieces)
    }
}

// ── Tables ───────────────────────────────────────────────────────────────────

/// Return whether `check` holds for every paragraph under `element`,
/// nested paragraphs included and hidden content excluded.
fn every_paragraph(element: &Element, check: &mut impl FnMut(&Element) -> bool) -> bool {
    element
        .child_elements()
        .filter(|child| !is_hidden(child))
        .all(|child| (!child.is(Ns::W, "p") || check(child)) && every_paragraph(child, check))
}

/// A paragraph to replace with a table.
#[derive(Clone, Copy, Debug)]
struct Placeholder {
    /// The index of the paragraph among the children of its parent.
    index: usize,
    /// Whether the next block, zero-width markers aside, is a paragraph.
    before_paragraph: bool,
}

/// A table to put in place of the paragraphs that hold nothing but a
/// placeholder, see [`replace_paragraphs_with_table`].
#[derive(Debug)]
struct TableReplacement<'a> {
    /// The placeholder, trimmed of whitespace and not empty.
    placeholder: &'a str,
    /// The table, copied in place of each paragraph.
    table: &'a Element,
    /// The shape of `table`, to know where it fits.
    shape: Shape,
}

impl TableReplacement<'_> {
    /// Replace the paragraphs under `parent`, whose room is `room`, that
    /// hold nothing but the placeholder; return how many.
    fn replace_in(&self, parent: &mut Element, room: Room) -> usize {
        let mut count = 0;
        let mut found: Vec<Placeholder> = Vec::new();
        // Whether the last placeholder found waits for the next block.
        let mut awaiting_block = false;
        for (index, node) in parent.children.iter_mut().enumerate() {
            let Node::Element(child) = node else {
                continue;
            };
            if !is_marker(child) {
                if awaiting_block && let Some(last) = found.last_mut() {
                    last.before_paragraph = child.is(Ns::W, "p");
                }
                awaiting_block = false;
            }
            if is_hidden(child) {
                continue;
            }
            if child.is(Ns::W, "p") && TextModel::new(child).is_only(self.placeholder) {
                // The table goes under `parent`, where the paragraph is. The
                // paragraphs and markers that go with it stay at that depth,
                // with no more declarations in scope than the paragraph had.
                if room.fits(self.shape) {
                    found.push(Placeholder {
                        index,
                        before_paragraph: false,
                    });
                    awaiting_block = true;
                }
            } else {
                let room = room.child(child);
                count += self.replace_in(child, room);
            }
        }
        if !found.is_empty() {
            count += found.len();
            let children = std::mem::take(&mut parent.children);
            parent.children = splice_tables(children, &found, self.table);
        }
        count
    }
}

/// Return `children` with the paragraphs `found` (sorted by index)
/// replaced by copies of `table`, with the paragraphs Word needs around
/// them and the range markers they held (see
/// [`replace_paragraphs_with_table`]).
fn splice_tables(children: Vec<Node>, found: &[Placeholder], table: &Element) -> Vec<Node> {
    let mut spliced = Vec::with_capacity(children.len() + 2 * found.len());
    let mut found = found.iter().peekable();
    // Whether the last block in `spliced`, zero-width markers aside, is a
    // table.
    let mut after_table = false;
    for (index, node) in children.into_iter().enumerate() {
        match (
            node,
            found.next_if(|placeholder| placeholder.index == index),
        ) {
            (Node::Element(mut paragraph), Some(placeholder)) => {
                let keep = ends_section(&paragraph) || !placeholder.before_paragraph;
                // The paragraph keeps only its properties. The ranges around
                // the placeholder go on around the table: what starts them
                // goes before it, and what ends them stays after it, in the
                // paragraph if it stays.
                let mut markers = Vec::new();
                collect_range_markers(take_content(&mut paragraph), None, &mut markers);
                let (leaving, staying): (Vec<Element>, Vec<Element>) = markers
                    .into_iter()
                    .partition(|marker| !keep || starts_range(marker));
                if after_table {
                    spliced.push(Node::Element(empty_paragraph_like(&paragraph)));
                }
                let scope = Scope {
                    element: &paragraph,
                    outer: None,
                };
                for mut marker in leaving {
                    keep_prefix_bound(&mut marker, Some(&scope));
                    spliced.push(Node::Element(marker));
                }
                spliced.push(Node::Element(table.clone()));
                if keep {
                    paragraph
                        .children
                        .extend(staying.into_iter().map(Node::Element));
                    spliced.push(Node::Element(paragraph));
                    after_table = false;
                } else {
                    after_table = table.is(Ns::W, "tbl");
                }
            }
            (node, _) => {
                if let Node::Element(element) = &node
                    && !is_marker(element)
                {
                    after_table = element.is(Ns::W, "tbl");
                }
                spliced.push(node);
            }
        }
    }
    spliced
}

/// Whether `paragraph` ends a section: its `w:pPr` holds the section's
/// `w:sectPr`.
fn ends_section(paragraph: &Element) -> bool {
    paragraph
        .find_child(Ns::W, "pPr")
        .is_some_and(|properties| properties.find_child(Ns::W, "sectPr").is_some())
}

/// Return an empty paragraph to go next to `paragraph`: same qualified
/// name, plus the declaration of its prefix if `paragraph` carries it, so
/// the prefix is bound wherever `paragraph` could be.
fn empty_paragraph_like(paragraph: &Element) -> Element {
    let mut empty = Element::new(paragraph.name(), Ns::W);
    let declaration = declaration_name(paragraph.prefix());
    if let Some(uri) = paragraph.attr(&declaration) {
        empty.set_attr(&declaration, &uri);
    }
    empty
}

/// Take everything but its properties (`w:pPr`) out of the placeholder
/// `paragraph`, and return it.
fn take_content(paragraph: &mut Element) -> Vec<Node> {
    let (properties, content): (Vec<Node>, Vec<Node>) = std::mem::take(&mut paragraph.children)
        .into_iter()
        .partition(|node| {
            node.as_element()
                .is_some_and(|element| element.is(Ns::W, "pPr"))
        });
    paragraph.children = properties;
    content
}

/// Whether `marker`, a range marker, starts its range (`w:bookmarkStart`,
/// `w:commentRangeStart`, ...) rather than ending it.
fn starts_range(marker: &Element) -> bool {
    marker.local_name().ends_with("Start")
}

/// An element that range markers are taken out of, and the elements
/// around it that they leave too: their namespace declarations stop
/// applying to the markers.
#[derive(Debug)]
struct Scope<'a> {
    /// The element.
    element: &'a Element,
    /// The scope of the element around it, if the markers leave it too.
    outer: Option<&'a Scope<'a>>,
}

/// Append the range markers in `nodes`, the content of the placeholder
/// paragraph or of the wrapper in it that `scope` holds, to `markers` in
/// document order, looking into wrappers. Range markup is valid between
/// blocks as well as in paragraphs (`EG_RangeMarkupElements` in
/// `EG_BlockLevelChunkElts`, ECMA-376 Part 1, §17.13); proofing marks
/// (`w:proofErr`) go with the text they flagged.
///
/// Markers taken out of wrappers keep their prefix bound (see
/// [`keep_prefix_bound`]); binding it for those that leave the paragraph
/// too is up to the caller.
fn collect_range_markers(nodes: Vec<Node>, scope: Option<&Scope<'_>>, markers: &mut Vec<Element>) {
    for node in nodes {
        let Node::Element(mut element) = node else {
            continue;
        };
        match Inline::of(&element) {
            Inline::Marker if !element.is(Ns::W, "proofErr") => {
                keep_prefix_bound(&mut element, scope);
                markers.push(element);
            }
            Inline::Wrapper => {
                let children = std::mem::take(&mut element.children);
                let scope = Scope {
                    element: &element,
                    outer: scope,
                };
                collect_range_markers(children, Some(&scope), markers);
            }
            _ => {}
        }
    }
}

/// Copy onto `marker` the innermost declaration of its prefix that an
/// element of `scope` carries, unless `marker` carries one itself, so the
/// prefix stays bound once `marker` leaves them.
fn keep_prefix_bound(marker: &mut Element, mut scope: Option<&Scope<'_>>) {
    let declaration = declaration_name(marker.prefix());
    if marker.attr(&declaration).is_some() {
        return;
    }
    while let Some(current) = scope {
        if let Some(uri) = current.element.attr(&declaration) {
            marker.set_attr(&declaration, &uri);
            return;
        }
        scope = current.outer;
    }
}

/// Return the name of the attribute that declares `prefix`, or the
/// default namespace for `None`.
fn declaration_name(prefix: Option<&str>) -> String {
    match prefix {
        Some(prefix) => format!("xmlns:{prefix}"),
        None => "xmlns".to_owned(),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;
    use crate::{
        image::Image,
        table::Table,
        xml::{MAX_NAME_LEN, XmlDocument},
    };

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// The WordprocessingML namespace URI.
    const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

    /// What precedes the body content of a test document: the root start
    /// tag with the namespace declarations Word writes, then `<w:body>`.
    const START: &str = concat!(
        "<w:document",
        " xmlns:wpc=\"http://schemas.microsoft.com/office/word/2010/wordprocessingCanvas\"",
        " xmlns:mc=\"http://schemas.openxmlformats.org/markup-compatibility/2006\"",
        " xmlns:o=\"urn:schemas-microsoft-com:office:office\"",
        " xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"",
        " xmlns:m=\"http://schemas.openxmlformats.org/officeDocument/2006/math\"",
        " xmlns:v=\"urn:schemas-microsoft-com:vml\"",
        " xmlns:wp14=\"http://schemas.microsoft.com/office/word/2010/wordprocessingDrawing\"",
        " xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\"",
        " xmlns:w10=\"urn:schemas-microsoft-com:office:word\"",
        " xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"",
        " xmlns:w14=\"http://schemas.microsoft.com/office/word/2010/wordml\"",
        " xmlns:w15=\"http://schemas.microsoft.com/office/word/2012/wordml\"",
        " xmlns:wpg=\"http://schemas.microsoft.com/office/word/2010/wordprocessingGroup\"",
        " xmlns:wps=\"http://schemas.microsoft.com/office/word/2010/wordprocessingShape\"",
        " mc:Ignorable=\"w14 w15 wp14\"><w:body>",
    );

    /// What follows the body content of a test document.
    const END: &str = "</w:body></w:document>";

    /// A run holding a text box, as Word writes it: a `wps` shape, then a
    /// VML copy of it as fallback. [`text_box`] puts the content of the
    /// text box between the parts.
    const TEXT_BOX: [&str; 3] = [
        concat!(
            "<w:r><w:rPr><w:noProof/></w:rPr><mc:AlternateContent>",
            "<mc:Choice Requires=\"wps\"><w:drawing>",
            "<wp:anchor distT=\"45720\" distB=\"45720\" distL=\"114300\" distR=\"114300\"",
            " simplePos=\"0\" relativeHeight=\"251659264\" behindDoc=\"0\" locked=\"0\"",
            " layoutInCell=\"1\" allowOverlap=\"1\"><wp:simplePos x=\"0\" y=\"0\"/>",
            "<wp:positionH relativeFrom=\"column\"><wp:posOffset>0</wp:posOffset>",
            "</wp:positionH><wp:positionV relativeFrom=\"paragraph\">",
            "<wp:posOffset>0</wp:posOffset></wp:positionV>",
            "<wp:extent cx=\"2360930\" cy=\"1404620\"/>",
            "<wp:effectExtent l=\"0\" t=\"0\" r=\"22860\" b=\"11430\"/>",
            "<wp:wrapSquare wrapText=\"bothSides\"/><wp:docPr id=\"217\" name=\"Text Box 2\"/>",
            "<wp:cNvGraphicFramePr/>",
            "<a:graphic xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">",
            "<a:graphicData",
            " uri=\"http://schemas.microsoft.com/office/word/2010/wordprocessingShape\">",
            "<wps:wsp><wps:cNvSpPr txBox=\"1\"><a:spLocks noChangeArrowheads=\"1\"/>",
            "</wps:cNvSpPr><wps:spPr bwMode=\"auto\"><a:xfrm><a:off x=\"0\" y=\"0\"/>",
            "<a:ext cx=\"2360930\" cy=\"1404620\"/></a:xfrm>",
            "<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></wps:spPr>",
            "<wps:txbx><w:txbxContent>",
        ),
        concat!(
            "</w:txbxContent></wps:txbx>",
            "<wps:bodyPr rot=\"0\" vert=\"horz\" wrap=\"square\" lIns=\"91440\" tIns=\"45720\"",
            " rIns=\"91440\" bIns=\"45720\" anchor=\"t\" anchorCtr=\"0\"><a:spAutoFit/>",
            "</wps:bodyPr></wps:wsp></a:graphicData></a:graphic></wp:anchor></w:drawing>",
            "</mc:Choice><mc:Fallback><w:pict>",
            "<v:shapetype id=\"_x0000_t202\" coordsize=\"21600,21600\" o:spt=\"202\"",
            " path=\"m,l,21600r21600,l21600,xe\"><v:stroke joinstyle=\"miter\"/>",
            "<v:path gradientshapeok=\"t\" o:connecttype=\"rect\"/></v:shapetype>",
            "<v:shape id=\"Text Box 2\" o:spid=\"_x0000_s1026\" type=\"#_x0000_t202\"",
            " style=\"position:absolute;margin-left:0;margin-top:0;width:185.9pt;",
            "height:110.6pt;z-index:251659264\"><v:textbox style=\"mso-fit-shape-to-text:t\">",
            "<w:txbxContent>",
        ),
        concat!(
            "</w:txbxContent></v:textbox></v:shape></w:pict></mc:Fallback>",
            "</mc:AlternateContent></w:r>",
        ),
    ];

    /// The table the table tests insert. Like the ones [`Table`] makes, it
    /// declares its namespace; its style marks it as inserted.
    const TABLE: &str = concat!(
        "<w:tbl xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">",
        "<w:tblPr><w:tblStyle w:val=\"Inserted\"/><w:tblW w:w=\"0\" w:type=\"auto\"/>",
        "</w:tblPr><w:tblGrid><w:gridCol w:w=\"9638\"/></w:tblGrid>",
        "<w:tr><w:tc><w:tcPr><w:tcW w:w=\"9638\" w:type=\"dxa\"/></w:tcPr>",
        "<w:p><w:r><w:t>cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl>",
    );

    /// Real LibreOffice 25.2 output, `word/document.xml`.
    const LO_DOCUMENT: &[u8] = include_bytes!("testdata/libreoffice/document.xml");
    /// LibreOffice `word/document.xml` with a picture.
    const LO_DOCUMENT_IMAGE: &[u8] = include_bytes!("testdata/libreoffice/document_image.xml");
    /// LibreOffice `word/styles.xml`.
    const LO_STYLES: &[u8] = include_bytes!("testdata/libreoffice/styles.xml");
    /// LibreOffice `[Content_Types].xml`.
    const LO_CONTENT_TYPES: &[u8] = include_bytes!("testdata/libreoffice/content_types.xml");
    /// The 3x2 PNG of the image tests.
    const PNG_3X2: &[u8] = include_bytes!("testdata/3x2.png");

    /// Return a run holding a text box with `content` in both of its
    /// copies, see [`TEXT_BOX`].
    fn text_box(content: &str) -> String {
        let [choice, fallback, end] = TEXT_BOX;
        [choice, content, fallback, content, end].concat()
    }

    /// Return the body of a Word template: a title split across runs by
    /// proofing marks, a hyperlink, tracked changes, a tab, a merge field,
    /// a footnote reference, a content control, a table, a text box and
    /// the section properties.
    fn word_body() -> String {
        [
            concat!(
                "<w:p w14:paraId=\"0A1B2C3D\" w14:textId=\"77777777\" w:rsidR=\"00B57C6E\"",
                " w:rsidRDefault=\"00B57C6E\" w:rsidP=\"00B57C6E\">",
                "<w:pPr><w:pStyle w:val=\"Title\"/><w:jc w:val=\"center\"/></w:pPr>",
                "<w:r><w:rPr><w:b/><w:bCs/></w:rPr><w:t xml:space=\"preserve\">Invoice {{</w:t>",
                "</w:r><w:proofErr w:type=\"spellStart\"/>",
                "<w:r w:rsidRPr=\"00A13F2E\"><w:rPr><w:b/></w:rPr><w:t>number</w:t></w:r>",
                "<w:proofErr w:type=\"spellEnd\"/>",
                "<w:r><w:rPr><w:b/></w:rPr><w:t>}}</w:t></w:r></w:p>",
                "<w:p w:rsidR=\"00B57C6E\" w:rsidRDefault=\"00B57C6E\">",
                "<w:r><w:t xml:space=\"preserve\">Dear </w:t></w:r>",
                "<w:bookmarkStart w:id=\"0\" w:name=\"_GoBack\"/><w:bookmarkEnd w:id=\"0\"/>",
                "<w:hyperlink r:id=\"rId5\" w:history=\"1\"><w:r><w:rPr>",
                "<w:rStyle w:val=\"Hyperlink\"/></w:rPr><w:t>{{customer}}</w:t></w:r>",
                "</w:hyperlink><w:del w:id=\"1\" w:author=\"Ada\" w:date=\"2024-05-01T10:00:00Z\">",
                "<w:r w:rsidDel=\"00C1\"><w:delText>{{customer}}</w:delText></w:r></w:del>",
                "<w:ins w:id=\"2\" w:author=\"Ada\" w:date=\"2024-05-01T10:00:00Z\">",
                "<w:r><w:t>, you owe {{total}}</w:t></w:r></w:ins>",
                "<w:r><w:tab/><w:t xml:space=\"preserve\">due {{date}} </w:t></w:r>",
                "<w:r><w:fldChar w:fldCharType=\"begin\"/></w:r>",
                "<w:r><w:instrText xml:space=\"preserve\"> MERGEFIELD {{customer}} </w:instrText>",
                "</w:r><w:r><w:fldChar w:fldCharType=\"separate\"/></w:r>",
                "<w:r><w:t>\u{ab}{{customer}}\u{bb}</w:t></w:r>",
                "<w:r><w:fldChar w:fldCharType=\"end\"/></w:r>",
                "<w:r><w:rPr><w:rStyle w:val=\"FootnoteReference\"/></w:rPr>",
                "<w:footnoteReference w:id=\"1\"/></w:r></w:p>",
                "<w:sdt><w:sdtPr><w:alias w:val=\"Notes\"/><w:tag w:val=\"{{notes}}\"/>",
                "<w:id w:val=\"-1234567\"/></w:sdtPr><w:sdtContent>",
                "<w:p><w:r><w:t>{{notes}}</w:t></w:r></w:p></w:sdtContent></w:sdt>",
                "<w:tbl><w:tblPr><w:tblStyle w:val=\"TableGrid\"/>",
                "<w:tblW w:w=\"0\" w:type=\"auto\"/></w:tblPr><w:tblGrid>",
                "<w:gridCol w:w=\"4819\"/><w:gridCol w:w=\"4819\"/></w:tblGrid>",
                "<w:tr><w:tc><w:tcPr><w:tcW w:w=\"4819\" w:type=\"dxa\"/></w:tcPr>",
                "<w:p><w:r><w:t>{{item}}</w:t></w:r></w:p></w:tc>",
                "<w:tc><w:tcPr><w:tcW w:w=\"4819\" w:type=\"dxa\"/></w:tcPr>",
                "<w:p><w:r><w:t>{{rows}}</w:t></w:r></w:p></w:tc></w:tr></w:tbl>",
                "<w:p><w:r><w:t xml:space=\"preserve\">See the box: </w:t></w:r>",
            ),
            text_box("<w:p><w:r><w:t>{{customer}} box</w:t></w:r></w:p>").as_str(),
            concat!(
                "</w:p>",
                "<w:p><w:pPr><w:sectPr><w:pgSz w:w=\"11906\" w:h=\"16838\"/></w:sectPr>",
                "</w:pPr><w:r><w:t>{{rows}}</w:t></w:r></w:p>",
                "<w:p><w:r><w:t>Thanks</w:t></w:r></w:p>",
                "<w:sectPr w:rsidR=\"00B57C6E\"><w:pgSz w:w=\"11906\" w:h=\"16838\"/>",
                "<w:pgMar w:top=\"1417\" w:right=\"1134\" w:bottom=\"1134\" w:left=\"1134\"",
                " w:header=\"708\" w:footer=\"708\" w:gutter=\"0\"/><w:cols w:space=\"708\"/>",
                "<w:docGrid w:linePitch=\"360\"/></w:sectPr>",
            ),
        ]
        .concat()
    }

    /// Parse a test document whose body content is `body`.
    fn document(body: &str) -> Element {
        let xml = [START, body, END].concat();
        Element::parse_fragment(&xml).unwrap_or_else(|err| panic!("{err}: {xml}"))
    }

    /// Serialize `root`, checking that the output is well-formed with
    /// every prefix bound: it parses back to the same bytes.
    fn serialize(root: &Element) -> String {
        let xml = root.to_xml();
        match Element::parse_fragment(&xml) {
            Ok(parsed) => assert_eq!(parsed.to_xml(), xml),
            Err(err) => panic!("{err}: {xml}"),
        }
        xml
    }

    /// Return the body content of a test document, see [`serialize`].
    fn body(document: &Element) -> String {
        let xml = serialize(document);
        xml.strip_prefix(START)
            .and_then(|xml| xml.strip_suffix(END))
            .unwrap_or_else(|| panic!("not a test document: {xml}"))
            .to_owned()
    }

    /// Replace `pairs` in a test document whose body content is `body_xml`;
    /// return the number of occurrences replaced and the new body content.
    fn replaced(body_xml: &str, pairs: &[(&str, &str)]) -> (usize, String) {
        let mut doc = document(body_xml);
        let count = replace_text(&mut doc, pairs);
        (count, body(&doc))
    }

    /// Replace the paragraphs holding only `placeholder` with [`TABLE`] in
    /// a test document whose body content is `body_xml`; return how many,
    /// and the new body content with `[T]` for each table.
    fn tabled(body_xml: &str, placeholder: &str) -> (usize, String) {
        let mut doc = document(body_xml);
        let table = Element::parse_fragment(TABLE).unwrap();
        let count = replace_paragraphs_with_table(&mut doc, placeholder, &table);
        (count, body(&doc).replace(TABLE, "[T]"))
    }

    /// Return the segments of every paragraph under `root`, in document
    /// order: outer paragraphs before the ones nested in them.
    fn segments(root: &Element) -> Vec<Vec<String>> {
        let mut all = Vec::new();
        every_paragraph(root, &mut |paragraph| {
            let model = TextModel::new(paragraph);
            all.push(
                model
                    .segment_texts()
                    .map(|(_, text)| text.to_owned())
                    .collect(),
            );
            true
        });
        all
    }

    /// Return the text of every paragraph under `root`, see [`segments`],
    /// with `|` between segments.
    fn paragraphs(root: &Element) -> Vec<String> {
        segments(root)
            .into_iter()
            .map(|segments| segments.join("|"))
            .collect()
    }

    /// Return a `w:drawing` like the ones `Image` makes, whose `wp:docPr`
    /// id is `id`.
    fn drawing(id: u32) -> Element {
        let xml = format!(
            "<w:drawing xmlns:w=\"{W}\" xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/\
             2006/wordprocessingDrawing\"><wp:inline distT=\"0\" distB=\"0\" distL=\"0\" \
             distR=\"0\"><wp:extent cx=\"952500\" cy=\"952500\"/><wp:docPr id=\"{id}\" \
             name=\"Picture {id}\"/></wp:inline></w:drawing>"
        );
        Element::parse_fragment(&xml).unwrap()
    }

    /// Placeholders that occur in the samples, or in parts of them.
    const PLACEHOLDERS: &[&str] = &[
        "{{a}}",
        "{{customer}}",
        "{{item}}",
        "{{logo}}",
        "Invoice {{number}}",
        "{{",
        "e",
        " ",
        "\u{fc}",
    ];

    /// Run every operation on a copy of `root` with each of `placeholders`,
    /// checking that the results stay well-formed and the counts agree.
    fn exercise(root: &Element, table: &Element, placeholders: &[&str]) {
        for &placeholder in placeholders {
            let mut doc = root.clone();
            replace_text(
                &mut doc,
                &[(placeholder, "x\ny\tz"), ("{{b}}", ""), ("}}", "{{")],
            );
            let mut made = 0;
            let count = replace_with_element(&mut doc, placeholder, || {
                made += 1;
                drawing(made)
            });
            assert_eq!(usize::try_from(made).unwrap(), count);
            table_placeholders_are_alone(&doc, placeholder);
            replace_paragraphs_with_table(&mut doc, placeholder, table);
            serialize(&doc);
        }
    }

    // ── Run text ─────────────────────────────────────────────────────────────

    #[test]
    fn run_text_splits_at_line_breaks_and_tabs() {
        use RunText::{Break, Tab, Text};
        let split = |text| split_run_text(text).collect::<Vec<_>>();
        assert_eq!(
            split("a\r\nb\rc\nd\te"),
            [
                Text("a"),
                Break,
                Text("b"),
                Break,
                Text("c"),
                Break,
                Text("d"),
                Tab,
                Text("e")
            ]
        );
        assert_eq!(split("\r\r\n\n"), [Break, Break, Break]);
        assert_eq!(split("\t\u{fc}\t"), [Tab, Text("\u{fc}"), Tab]);
        assert_eq!(split(" two  spaces "), [Text(" two  spaces ")]);
        assert_eq!(split(""), []);
    }

    // ── Text model ───────────────────────────────────────────────────────────

    #[test]
    fn text_model_skips_hidden_content_and_splits_at_barriers() {
        let doc = document(concat!(
            "<w:p><w:pPr><w:rPr><w:t>pPr</w:t></w:rPr></w:pPr>",
            "<w:r><w:rPr><w:t>rPr</w:t></w:rPr><w:t>a</w:t><w:lastRenderedPageBreak/>",
            "<w:t>b</w:t></w:r><w:proofErr w:type=\"gramStart\"/>",
            "<w:del w:id=\"1\"><w:r><w:t>del</w:t></w:r></w:del>",
            "<w:hyperlink w:anchor=\"x\"><w:r><w:t>c</w:t></w:r></w:hyperlink>",
            "<w:sdt><w:sdtPr><w:r><w:t>sdtPr</w:t></w:r></w:sdtPr><w:sdtContent><w:r>",
            "<w:t>d</w:t></w:r></w:sdtContent><w:sdtEndPr><w:r><w:t>sdtEndPr</w:t></w:r>",
            "</w:sdtEndPr></w:sdt><w:r><w:tab/><w:t>e</w:t><w:t>{{<w:x/>}}</w:t><w:t>f</w:t></w:r>",
            "<w:moveFrom w:id=\"2\"><w:r><w:t>moved</w:t></w:r></w:moveFrom>",
            "<w:p><w:r><w:t>nested</w:t></w:r></w:p><w:r><w:t>g</w:t></w:r>",
            "<mc:AlternateContent><mc:Choice Requires=\"w14\"><w:r><w:t>h</w:t></w:r>",
            "</mc:Choice><mc:Fallback><w:r><w:t>i</w:t></w:r></mc:Fallback>",
            "</mc:AlternateContent><w:r><w:t/><w:t>j</w:t></w:r></w:p>",
        ));
        let paragraph = doc.descendants().find(|e| e.is(Ns::W, "p")).unwrap();
        let model = TextModel::new(paragraph);
        let segments: Vec<_> = model.segment_texts().map(|(_, text)| text).collect();
        assert_eq!(segments, ["abcd", "e", "f", "g", "h", "i", "j"]);
        assert_eq!(model.atoms.len(), 11, "the empty w:t is an atom");
        assert!(model.more_than_text);
        assert!(model.contains("bc") && !model.contains("de") && !model.contains("ij"));

        let doc = document(concat!(
            "<w:p><w:pPr><w:jc w:val=\"center\"/></w:pPr>",
            "<w:r><w:t xml:space=\"preserve\"> {{</w:t></w:r>",
            "<w:bookmarkStart w:id=\"0\" w:name=\"t\"/><w:r><w:t>t}}\u{a0}</w:t></w:r></w:p>",
        ));
        let model = TextModel::new(doc.descendants().find(|e| e.is(Ns::W, "p")).unwrap());
        assert!(!model.more_than_text);
        assert!(model.is_only("{{t}}") && !model.is_only("{{t}}\u{a0}"));
    }

    // ── Matching ─────────────────────────────────────────────────────────────

    /// Return the occurrences of `keys` in `text` as (start, end, key).
    fn find(text: &str, keys: &[&str]) -> Vec<(usize, usize, usize)> {
        let mut matches = Vec::new();
        find_in(text, keys, 0, &mut Vec::new(), &mut matches);
        matches
            .into_iter()
            .map(|found| (found.range.start, found.range.end, found.key))
            .collect()
    }

    #[test]
    fn leftmost_occurrence_wins_then_the_longest_key_then_the_first() {
        assert_eq!(find("xabcd", &["bc", "abc", "ab", "abc"]), [(1, 4, 1)]);
        assert_eq!(find("aaaa", &["aa"]), [(0, 2, 0), (2, 4, 0)]);
        assert_eq!(find("aaa", &["aa"]), [(0, 2, 0)]);
        // "aa" first occurs at 1, inside "ba": it is searched again from 2.
        assert_eq!(find("baaa", &["aa", "ba"]), [(0, 2, 1), (2, 4, 0)]);
        // Keys sharing leading bytes with the text never match mid-character.
        assert_eq!(
            find("\u{e9}\u{e8}\u{e9}", &["\u{e8}", "\u{e9}"]),
            [(0, 2, 1), (2, 4, 0), (4, 6, 1)]
        );
        assert_eq!(find("abc", &["", "b"]), [(1, 2, 1)]);
        assert_eq!(find("abc", &[]), []);
        assert_eq!(find("", &["a"]), []);
    }

    #[test]
    fn occurrences_cut_every_atom_they_span() {
        // "Hello {{fir" "st}} and {{" "" "last}}!"
        let atoms = [0..11, 11..22, 22..22, 22..29];
        let matches = [
            Match {
                range: 6..15,
                key: 0,
            },
            Match {
                range: 20..28,
                key: 1,
            },
        ];
        let cut = |atom, range, key, occurrence| Cut {
            atom,
            range,
            key,
            occurrence,
        };
        assert_eq!(
            cuts(&atoms, &matches),
            [
                cut(0, 6..11, Some(0), 0),
                cut(1, 0..4, None, 0),
                cut(1, 9..11, Some(1), 1),
                cut(3, 0..6, None, 1),
            ]
        );
    }

    // ── Text replacement ─────────────────────────────────────────────────────

    #[test]
    fn placeholder_in_one_text_is_replaced() {
        assert_eq!(
            replaced(
                concat!(
                    "<w:p w:rsidR=\"00B5\"><w:r><w:rPr><w:b/></w:rPr><w:t>Dear {{name}},</w:t>",
                    "</w:r></w:p>",
                ),
                &[("{{name}}", "Ada")],
            ),
            (
                1,
                concat!(
                    "<w:p w:rsidR=\"00B5\"><w:r><w:rPr><w:b/></w:rPr>",
                    "<w:t xml:space=\"preserve\">Dear Ada,</w:t></w:r></w:p>",
                )
                .to_owned()
            )
        );
    }

    #[test]
    fn placeholder_split_across_runs_takes_the_first_run_formatting() {
        assert_eq!(
            replaced(
                concat!(
                    "<w:p w:rsidR=\"00B57C6E\" w:rsidRDefault=\"00B57C6E\">",
                    "<w:r><w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">Dear {{</w:t></w:r>",
                    "<w:proofErr w:type=\"spellStart\"/>",
                    "<w:r w:rsidRPr=\"00A13F2E\"><w:rPr><w:i/></w:rPr><w:t>cust</w:t></w:r>",
                    "<w:bookmarkStart w:id=\"0\" w:name=\"_GoBack\"/>",
                    "<w:proofErr w:type=\"spellEnd\"/>",
                    "<w:r><w:rPr><w:u w:val=\"single\"/></w:rPr><w:t>omer}}, welcome</w:t></w:r>",
                    "<w:bookmarkEnd w:id=\"0\"/></w:p>",
                ),
                &[("{{customer}}", "ACME")],
            ),
            (
                1,
                concat!(
                    "<w:p w:rsidR=\"00B57C6E\" w:rsidRDefault=\"00B57C6E\">",
                    "<w:r><w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">Dear ACME</w:t></w:r>",
                    "<w:proofErr w:type=\"spellStart\"/>",
                    "<w:r w:rsidRPr=\"00A13F2E\"><w:rPr><w:i/></w:rPr></w:r>",
                    "<w:bookmarkStart w:id=\"0\" w:name=\"_GoBack\"/>",
                    "<w:proofErr w:type=\"spellEnd\"/>",
                    "<w:r><w:rPr><w:u w:val=\"single\"/></w:rPr>",
                    "<w:t xml:space=\"preserve\">, welcome</w:t></w:r>",
                    "<w:bookmarkEnd w:id=\"0\"/></w:p>",
                )
                .to_owned()
            )
        );
    }

    #[test]
    fn every_occurrence_in_a_text_and_in_a_paragraph_is_replaced() {
        assert_eq!(
            replaced(
                "<w:p><w:r><w:t>{{a}} and {{a}}, then {{b}}{{a}}</w:t></w:r></w:p>",
                &[("{{a}}", "1"), ("{{b}}", "22")],
            ),
            (
                4,
                "<w:p><w:r><w:t xml:space=\"preserve\">1 and 1, then 221</w:t></w:r></w:p>"
                    .to_owned()
            )
        );
        // Each value takes the formatting of the run its key starts in.
        assert_eq!(
            replaced(
                concat!(
                    "<w:p><w:r><w:t xml:space=\"preserve\">Hello {{fir</w:t></w:r>",
                    "<w:r><w:rPr><w:i/></w:rPr><w:t xml:space=\"preserve\">st}} and {{</w:t></w:r>",
                    "<w:r><w:t/></w:r><w:r><w:t>last}}!</w:t></w:r></w:p>",
                ),
                &[("{{first}}", "Ada"), ("{{last}}", "Lovelace")],
            ),
            (
                2,
                concat!(
                    "<w:p><w:r><w:t xml:space=\"preserve\">Hello Ada</w:t></w:r>",
                    "<w:r><w:rPr><w:i/></w:rPr><w:t xml:space=\"preserve\"> and Lovelace</w:t>",
                    "</w:r><w:r><w:t/></w:r><w:r><w:t xml:space=\"preserve\">!</w:t></w:r></w:p>",
                )
                .to_owned()
            )
        );
    }

    #[test]
    fn longest_key_wins_and_values_are_not_searched() {
        assert_eq!(
            replaced(
                "<w:p><w:r><w:t>{{a}}x {{a}} {{b}}</w:t></w:r></w:p>",
                &[
                    ("{{a}}", "{{b}}"),
                    ("{{a}}x", "AX"),
                    ("{{b}}", "B"),
                    ("a}} {{", "never"),
                    ("", "ignored"),
                ],
            ),
            (
                3,
                "<w:p><w:r><w:t xml:space=\"preserve\">AX {{b}} B</w:t></w:r></w:p>".to_owned()
            )
        );
        // The first of two pairs with the same key wins.
        assert_eq!(
            replaced(
                "<w:p><w:r><w:t>k</w:t></w:r></w:p>",
                &[("k", "1"), ("k", "2")]
            ),
            (
                1,
                "<w:p><w:r><w:t xml:space=\"preserve\">1</w:t></w:r></w:p>".to_owned()
            )
        );
    }

    #[test]
    fn barriers_split_the_text_of_a_paragraph() {
        let unchanged = concat!(
            "<w:p><w:r><w:t>{{na</w:t></w:r><w:r><w:tab/></w:r><w:r><w:t>me}}</w:t></w:r></w:p>",
            "<w:p><w:r><w:t>{{na</w:t><w:br/><w:t>me}}</w:t></w:r></w:p>",
            "<w:p><w:r><w:t>{{na</w:t></w:r><w:r><w:fldChar w:fldCharType=\"begin\"/></w:r>",
            "<w:r><w:instrText xml:space=\"preserve\"> PAGE </w:instrText></w:r>",
            "<w:r><w:fldChar w:fldCharType=\"separate\"/></w:r><w:r><w:t>1</w:t></w:r>",
            "<w:r><w:fldChar w:fldCharType=\"end\"/></w:r><w:r><w:t>me}}</w:t></w:r></w:p>",
            "<w:p><w:r><w:t>{{na</w:t><w:sym w:font=\"Wingdings\" w:char=\"F04A\"/>",
            "<w:t>me}}</w:t></w:r></w:p>",
        );
        assert_eq!(
            replaced(unchanged, &[("{{name}}", "Ada")]),
            (0, unchanged.to_owned())
        );
        // Between barriers, and across a w:lastRenderedPageBreak.
        assert_eq!(
            replaced(
                concat!(
                    "<w:p><w:r><w:tab/><w:t>{{name}}</w:t><w:br/></w:r>",
                    "<w:r><w:t>{{na</w:t><w:lastRenderedPageBreak/><w:t>me}}</w:t></w:r></w:p>",
                ),
                &[("{{name}}", "Ada")],
            ),
            (
                2,
                concat!(
                    "<w:p><w:r><w:tab/><w:t xml:space=\"preserve\">Ada</w:t><w:br/></w:r>",
                    "<w:r><w:t xml:space=\"preserve\">Ada</w:t><w:lastRenderedPageBreak/></w:r>",
                    "</w:p>",
                )
                .to_owned()
            )
        );
    }

    #[test]
    fn deleted_text_is_ignored_and_inserted_text_is_matched() {
        let date = "w:author=\"Ada\" w:date=\"2024-05-01T10:00:00Z\"";
        let body_xml = [
            "<w:p><w:r><w:t>{{na</w:t></w:r>",
            &format!("<w:del w:id=\"1\" {date}><w:r><w:delText>XX</w:delText></w:r></w:del>"),
            &format!("<w:ins w:id=\"2\" {date}><w:r><w:t>me}}}}!</w:t></w:r></w:ins></w:p>"),
            &format!("<w:p><w:del w:id=\"3\" {date}><w:r><w:t>{{{{name}}}}</w:t></w:r></w:del>"),
            &format!("<w:moveFrom w:id=\"4\" {date}><w:r><w:t>{{{{name}}}}</w:t></w:r>"),
            &format!("</w:moveFrom><w:moveTo w:id=\"5\" {date}><w:r><w:t>{{{{name}}}}</w:t>"),
            "</w:r></w:moveTo></w:p>",
        ]
        .concat();
        let expected = body_xml
            .replacen(
                "<w:t>{{na</w:t>",
                "<w:t xml:space=\"preserve\">Ada</w:t>",
                1,
            )
            .replacen("<w:t>me}}!</w:t>", "<w:t xml:space=\"preserve\">!</w:t>", 1)
            .replacen(
                "<w:r><w:t>{{name}}</w:t></w:r></w:moveTo>",
                "<w:r><w:t xml:space=\"preserve\">Ada</w:t></w:r></w:moveTo>",
                1,
            );
        assert_eq!(replaced(&body_xml, &[("{{name}}", "Ada")]), (2, expected));
    }

    #[test]
    fn runs_in_wrappers_are_matched() {
        let (count, xml) = replaced(
            concat!(
                "<w:p><w:hyperlink r:id=\"rId5\" w:history=\"1\"><w:r><w:rPr>",
                "<w:rStyle w:val=\"Hyperlink\"/></w:rPr><w:t>{{a}}</w:t></w:r></w:hyperlink>",
                "<w:sdt><w:sdtPr><w:alias w:val=\"{{a}}\"/><w:id w:val=\"-1\"/><w:showingPlcHdr/>",
                "</w:sdtPr><w:sdtContent><w:r><w:t>{{a}}</w:t></w:r></w:sdtContent></w:sdt>",
                "<w:smartTag w:uri=\"urn:schemas-microsoft-com:office:smarttags\"",
                " w:element=\"place\"><w:smartTagPr><w:attr w:name=\"n\" w:val=\"v\"/>",
                "</w:smartTagPr><w:r><w:t>{{a}}</w:t></w:r></w:smartTag>",
                "<w:fldSimple w:instr=\" MERGEFIELD a \"><w:r><w:t>{{a}}</w:t></w:r></w:fldSimple>",
                "<w:customXml w:element=\"c\"><w:r><w:t>{{a}}</w:t></w:r></w:customXml>",
                "<w:dir w:val=\"rtl\"><w:bdo w:val=\"ltr\"><w:r><w:t>{{a}}</w:t></w:r></w:bdo>",
                "</w:dir></w:p>",
                "<w:p><w:r><w:t>{{sp</w:t></w:r><w:hyperlink w:anchor=\"x\"><w:r><w:t>li</w:t>",
                "</w:r></w:hyperlink><w:r><w:t>t}}</w:t></w:r></w:p>",
            ),
            &[("{{a}}", "A"), ("{{split}}", "S")],
        );
        assert_eq!(count, 7);
        assert!(xml.contains("<w:alias w:val=\"{{a}}\"/>"), "{xml}");
        assert_eq!(paragraphs(&document(&xml)), ["AAAAAA", "S"]);
        assert!(xml.ends_with(concat!(
            "<w:p><w:r><w:t xml:space=\"preserve\">S</w:t></w:r><w:hyperlink w:anchor=\"x\">",
            "<w:r></w:r></w:hyperlink><w:r></w:r></w:p>",
        )));
    }

    #[test]
    fn both_copies_of_a_text_box_are_replaced_apart_from_their_paragraph() {
        let box_paragraph = "<w:p><w:r><w:t>Box {{a}}</w:t></w:r></w:p>";
        let body_xml = [
            "<w:p><w:r><w:t xml:space=\"preserve\">Before {{a}} {{</w:t></w:r>",
            &text_box(box_paragraph),
            "<w:r><w:t xml:space=\"preserve\">a}} after {{a}}</w:t></w:r></w:p>",
        ]
        .concat();
        let (count, xml) = replaced(&body_xml, &[("{{a}}", "X")]);
        assert_eq!(count, 4);
        assert_eq!(
            paragraphs(&document(&xml)),
            ["Before X {{|a}} after X", "Box X", "Box X"]
        );
        let replaced_box = "<w:p><w:r><w:t xml:space=\"preserve\">Box X</w:t></w:r></w:p>";
        assert_eq!(
            xml,
            body_xml
                .replace(box_paragraph, replaced_box)
                .replace("Before {{a}} {{", "Before X {{")
                .replace("a}} after {{a}}", "a}} after X")
        );
    }

    #[test]
    fn line_breaks_and_tabs_in_values_become_elements() {
        assert_eq!(
            replaced(
                "<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>A{{v}}B</w:t></w:r></w:p>",
                &[("{{v}}", "1\n2\r\n3\r4\t5")],
            ),
            (
                1,
                concat!(
                    "<w:p><w:r><w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">A1</w:t><w:br/>",
                    "<w:t xml:space=\"preserve\">2</w:t><w:br/><w:t xml:space=\"preserve\">3</w:t>",
                    "<w:br/><w:t xml:space=\"preserve\">4</w:t><w:tab/>",
                    "<w:t xml:space=\"preserve\">5B</w:t></w:r></w:p>",
                )
                .to_owned()
            )
        );
        // The first text keeps the original w:t, wherever it falls.
        assert_eq!(
            replaced(
                "<w:p><w:r><w:t>{{v}}</w:t></w:r></w:p>",
                &[("{{v}}", "\tX\n")],
            ),
            (
                1,
                "<w:p><w:r><w:tab/><w:t xml:space=\"preserve\">X</w:t><w:br/></w:r></w:p>"
                    .to_owned()
            )
        );
    }

    #[test]
    fn spaces_are_preserved_and_untouched_texts_keep_their_bytes() {
        assert_eq!(
            replaced(
                concat!(
                    "<w:p><w:r><w:t xml:space=\"preserve\">a {{v}}</w:t></w:r>",
                    "<w:r><w:t xml:space='default'>{{v}} b</w:t></w:r>",
                    "<w:r><w:t>keep&#x20;me</w:t></w:r><w:r><w:t>&#x41;{{v}}</w:t></w:r></w:p>",
                ),
                &[("{{v}}", "  two  spaces  ")],
            ),
            (
                3,
                concat!(
                    "<w:p><w:r><w:t xml:space=\"preserve\">a   two  spaces  </w:t></w:r>",
                    "<w:r><w:t xml:space='preserve'>  two  spaces   b</w:t></w:r>",
                    "<w:r><w:t>keep&#x20;me</w:t></w:r>",
                    "<w:r><w:t xml:space=\"preserve\">A  two  spaces  </w:t></w:r></w:p>",
                )
                .to_owned()
            )
        );
    }

    #[test]
    fn empty_values_remove_the_placeholder_and_emptied_texts() {
        assert_eq!(
            replaced(
                concat!(
                    "<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>{{v}}</w:t></w:r>",
                    "<w:r><w:t>x {{v}}</w:t></w:r><w:r><w:t>{{w}}</w:t></w:r></w:p>",
                ),
                &[("{{v}}", ""), ("{{w}}", "\u{1}\u{fffe}")],
            ),
            (
                3,
                concat!(
                    "<w:p><w:r><w:rPr><w:b/></w:rPr></w:r>",
                    "<w:r><w:t xml:space=\"preserve\">x </w:t></w:r><w:r></w:r></w:p>",
                )
                .to_owned()
            )
        );
    }

    #[test]
    fn markup_characters_and_multi_byte_text_are_handled() {
        assert_eq!(
            replaced(
                concat!(
                    "<w:p><w:r><w:t>\u{fc}n\u{ef} {{n\u{e4}</w:t></w:r><w:r><w:t>me}}\u{2713} ",
                    "\u{65e5}\u{672c}{{\u{540d}}}\u{8a9e} a &lt; b &amp; c</w:t></w:r></w:p>",
                ),
                &[
                    ("{{n\u{e4}me}}", "Zo\u{eb}"),
                    ("{{\u{540d}}}", "\u{540d}\u{524d}"),
                    ("a < b & c", "<x> & \"y\""),
                ],
            ),
            (
                3,
                concat!(
                    "<w:p><w:r><w:t xml:space=\"preserve\">\u{fc}n\u{ef} Zo\u{eb}</w:t></w:r>",
                    "<w:r><w:t xml:space=\"preserve\">\u{2713} \u{65e5}\u{672c}\u{540d}\u{524d}",
                    "\u{8a9e} &lt;x&gt; &amp; \"y\"</w:t></w:r></w:p>",
                )
                .to_owned()
            )
        );
    }

    #[test]
    fn new_elements_take_the_prefix_of_their_run() {
        let value = [("{{name}}", "A\tB\nC")];
        let mut doc = Element::parse_fragment(&format!(
            "<ns0:document xmlns:ns0=\"{W}\"><ns0:body><ns0:p><ns0:r><ns0:rPr><ns0:b/>\
             </ns0:rPr><ns0:t>Hi {{{{name}}}}!</ns0:t></ns0:r><x:r xmlns:x=\"{W}\">\
             <x:t>{{{{name}}}}</x:t></x:r></ns0:p></ns0:body></ns0:document>"
        ))
        .unwrap();
        assert_eq!(replace_text(&mut doc, &value), 2);
        assert_eq!(
            serialize(&doc),
            format!(
                "<ns0:document xmlns:ns0=\"{W}\"><ns0:body><ns0:p><ns0:r><ns0:rPr><ns0:b/>\
                 </ns0:rPr><ns0:t xml:space=\"preserve\">Hi A</ns0:t><ns0:tab/>\
                 <ns0:t xml:space=\"preserve\">B</ns0:t><ns0:br/>\
                 <ns0:t xml:space=\"preserve\">C!</ns0:t></ns0:r><x:r xmlns:x=\"{W}\">\
                 <x:t xml:space=\"preserve\">A</x:t><x:tab/><x:t xml:space=\"preserve\">B</x:t>\
                 <x:br/><x:t xml:space=\"preserve\">C</x:t></x:r></ns0:p></ns0:body>\
                 </ns0:document>"
            )
        );
        assert!(
            doc.descendants()
                .filter(|e| e.local_name() != "document")
                .all(|e| e.ns() == Ns::W)
        );

        // WordprocessingML as the default namespace.
        let mut doc = Element::parse_fragment(&format!(
            "<document xmlns=\"{W}\"><body><p><r><t>x{{{{name}}}}</t></r></p></body></document>"
        ))
        .unwrap();
        assert_eq!(replace_text(&mut doc, &value), 1);
        assert_eq!(
            serialize(&doc),
            format!(
                "<document xmlns=\"{W}\"><body><p><r><t xml:space=\"preserve\">xA</t><tab/>\
                 <t xml:space=\"preserve\">B</t><br/><t xml:space=\"preserve\">C</t></r></p>\
                 </body></document>"
            )
        );
    }

    #[test]
    fn word_template_placeholders_are_replaced() {
        let mut doc = document(&word_body());
        let count = replace_text(
            &mut doc,
            &[
                ("{{number}}", "2024-001"),
                ("{{customer}}", "ACME & Co"),
                ("{{total}}", "\u{20ac} 1.000"),
                ("{{date}}", "1 May"),
                ("{{notes}}", "Line 1\nLine 2"),
                ("{{item}}", "Widget"),
            ],
        );
        assert_eq!(count, 9);
        assert_eq!(
            paragraphs(&doc),
            [
                "Invoice 2024-001",
                "Dear ACME & Co, you owe \u{20ac} 1.000|due 1 May |\u{ab}ACME & Co\u{bb}",
                "Line 1|Line 2",
                "Widget",
                "{{rows}}",
                "See the box: ",
                "ACME & Co box",
                "ACME & Co box",
                "{{rows}}",
                "Thanks",
            ]
        );
        let xml = body(&doc);
        for untouched in [
            "<w:delText>{{customer}}</w:delText>",
            "<w:instrText xml:space=\"preserve\"> MERGEFIELD {{customer}} </w:instrText>",
            "<w:tag w:val=\"{{notes}}\"/>",
        ] {
            assert!(xml.contains(untouched), "{untouched}");
        }
    }

    #[test]
    fn libreoffice_documents_change_only_where_replaced() {
        let mut doc = XmlDocument::parse(LO_DOCUMENT).unwrap();
        let count = replace_text(
            &mut doc.root,
            &[
                ("{{number}}", "42"),
                ("{{customer}}", "ACME"),
                ("{{item}}", "Widget"),
            ],
        );
        assert_eq!(count, 3);
        let expected = std::str::from_utf8(LO_DOCUMENT)
            .unwrap()
            .replace(
                "<w:t>Invoice {{number}}</w:t>",
                "<w:t xml:space=\"preserve\">Invoice 42</w:t>",
            )
            .replace(
                "<w:t>{{customer}}</w:t>",
                "<w:t xml:space=\"preserve\">ACME</w:t>",
            )
            .replace(
                "<w:t>{{item}}</w:t>",
                "<w:t xml:space=\"preserve\">Widget</w:t>",
            );
        assert_eq!(String::from_utf8(doc.to_bytes()).unwrap(), expected);
    }

    #[test]
    fn documents_without_occurrences_keep_every_byte() {
        let table = Element::parse_fragment(TABLE).unwrap();
        let word = [START, &word_body(), END].concat();
        for xml in [word.as_bytes(), LO_DOCUMENT, LO_DOCUMENT_IMAGE, LO_STYLES] {
            let mut doc = XmlDocument::parse(xml).unwrap();
            let root = &mut doc.root;
            let pairs = [("{{absent}}", "x"), ("", "y"), ("zzz", "")];
            assert_eq!(replace_text(root, &pairs), 0);
            assert_eq!(replace_text(root, &[]), 0);
            assert_eq!(replace_with_element(root, "{{absent}}", || drawing(1)), 0);
            assert_eq!(replace_with_element(root, "", || drawing(1)), 0);
            assert!(table_placeholders_are_alone(root, "{{absent}}"));
            assert_eq!(replace_paragraphs_with_table(root, "{{absent}}", &table), 0);
            assert_eq!(doc.to_bytes(), xml);
        }
    }

    #[test]
    fn equations_and_unknown_elements_are_barriers() {
        let equation = "<m:oMath><m:r><m:t>v</m:t></m:r></m:oMath>";
        let unchanged = [
            "<w:p><w:r><w:t>{{na</w:t></w:r>",
            equation,
            "<w:r><w:t>me}}</w:t></w:r></w:p>",
            "<w:p><w:r><w:t xml:space=\"preserve\">Speed is </w:t></w:r>",
            equation,
            "<w:r><w:t xml:space=\"preserve\"> exactly here.</w:t></w:r></w:p>",
            "<w:p><w:r><w:t>{{na</w:t></w:r><w:subDoc r:id=\"rId9\"/>",
            "<w:r><w:t>me}}</w:t></w:r></w:p>",
            // The runs of an unknown element are not part of the paragraph.
            "<w:p><w:r><w:t>{{na</w:t></w:r><w14:ext><w:r><w:t>me}} {{name}}</w:t></w:r>",
            "</w14:ext></w:p><w:p><m:oMathPara>",
            equation,
            "</m:oMathPara></w:p>",
        ]
        .concat();
        assert_eq!(
            replaced(&unchanged, &[("{{name}}", "Ada"), ("is  exactly", "QQQ")]),
            (0, unchanged)
        );

        // Text next to them is replaced, and the text boxes in them too.
        let body_xml = [
            "<w:p><w:r><w:t>{{name}}</w:t></w:r>",
            equation,
            "<w14:ext>",
            &text_box("<w:p><w:r><w:t>{{name}}</w:t></w:r></w:p>"),
            "</w14:ext></w:p>",
        ]
        .concat();
        let (count, xml) = replaced(&body_xml, &[("{{name}}", "Ada")]);
        assert_eq!(count, 3);
        assert_eq!(paragraphs(&document(&xml)), ["Ada", "Ada", "Ada"]);
    }

    #[test]
    fn properties_of_wrappers_do_not_split_occurrences() {
        let (count, xml) = replaced(
            concat!(
                "<w:p><w:r><w:t>{{na</w:t></w:r><w:smartTag w:element=\"place\"><w:smartTagPr>",
                "<w:attr w:name=\"n\" w:val=\"v\"/></w:smartTagPr><w:r><w:t>m</w:t></w:r>",
                "</w:smartTag><w:customXml w:element=\"c\"><w:customXmlPr>",
                "<w:attr w:name=\"n\" w:val=\"v\"/></w:customXmlPr><w:r><w:t>e</w:t></w:r>",
                "</w:customXml><w:fldSimple w:instr=\" MERGEFIELD x \">",
                "<w:fldData>ZGF0YQ==</w:fldData><w:r><w:t>}}</w:t></w:r></w:fldSimple></w:p>",
            ),
            &[("{{name}}", "Ada")],
        );
        assert_eq!(count, 1);
        assert_eq!(paragraphs(&document(&xml)), ["Ada"]);
    }

    #[test]
    fn keys_longer_than_a_segment_are_not_searched() {
        // Twenty thousand one-character segments and two hundred longer
        // keys: each segment is only searched for the keys that fit in it.
        let body_xml = format!(
            "<w:p><w:r>{}</w:r></w:p>",
            "<w:t>a</w:t><w:tab/>".repeat(20_000)
        );
        let keys: Vec<String> = (0..200)
            .map(|index| format!("{{{{key {index}}}}}"))
            .collect();
        let mut pairs: Vec<(&str, &str)> = keys.iter().map(|key| (key.as_str(), "x")).collect();
        pairs.push(("a", "b"));
        let mut doc = document(&body_xml);
        assert_eq!(replace_text(&mut doc, &pairs), 20_000);
        assert_eq!(paragraphs(&doc), [vec!["b"; 20_000].join("|")]);

        assert_eq!(find("ab", &["abc", "b", "ab"]), [(0, 2, 2)]);
        assert_eq!(find("a", &["ab", "abc"]), []);
    }

    #[test]
    fn whitespace_word_ignores_stays_out_of_sight() {
        // Without xml:space="preserve", Word ignores leading and trailing
        // whitespace: so does matching, and an edited w:t keeps it hidden.
        assert_eq!(
            replaced(
                concat!(
                    "<w:p><w:r><w:t> {{a}}</w:t></w:r></w:p>",
                    "<w:p><w:r><w:t>Hello </w:t></w:r>",
                    "<w:r><w:t xml:space=\"default\">{{a}} </w:t></w:r></w:p>",
                    "<w:p>\n  <w:r>\n    <w:t>\n      {{a}} and {{a}}\n",
                    "    </w:t>\n  </w:r>\n</w:p>",
                    "<w:p><w:r><w:t xml:space=\"preserve\"> {{a}} </w:t></w:r></w:p>",
                ),
                &[("{{a}}", "X")],
            ),
            (
                5,
                concat!(
                    "<w:p><w:r><w:t xml:space=\"preserve\">X</w:t></w:r></w:p>",
                    "<w:p><w:r><w:t>Hello </w:t></w:r>",
                    "<w:r><w:t xml:space=\"preserve\">X</w:t></w:r></w:p>",
                    "<w:p>\n  <w:r>\n    <w:t xml:space=\"preserve\">X and X</w:t>\n",
                    "  </w:r>\n</w:p>",
                    "<w:p><w:r><w:t xml:space=\"preserve\"> X </w:t></w:r></w:p>",
                )
                .to_owned()
            )
        );
        // What Word displays is what matches: "Hello" and "{{a}}" touch.
        let mut doc =
            document("<w:p><w:r><w:t>Hello </w:t></w:r><w:r><w:t>{{a}}</w:t></w:r></w:p>");
        assert_eq!(replace_text(&mut doc, &[("Hello{{a}}", "Hi")]), 1);
        assert_eq!(paragraphs(&doc), ["Hi"]);
    }

    // ── Element replacement ──────────────────────────────────────────────────

    #[test]
    fn element_goes_where_the_occurrence_starts_and_is_made_in_document_order() {
        let body_xml = [
            "<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>A {{img}} B {{im</w:t></w:r>",
            "<w:proofErr w:type=\"spellStart\"/><w:r><w:t>g}} C</w:t></w:r>",
            &text_box("<w:p><w:r><w:t>{{img}}</w:t></w:r></w:p>"),
            "<w:r><w:t>{{img}}</w:t></w:r></w:p>",
        ]
        .concat();
        let mut doc = document(&body_xml);
        let mut made = 0;
        let count = replace_with_element(&mut doc, "{{img}}", || {
            made += 1;
            drawing(made)
        });
        assert_eq!((count, made), (5, 5));

        let [choice, fallback, end] = TEXT_BOX;
        let xml = |id| drawing(id).to_xml();
        let expected = [
            "<w:p><w:r><w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">A </w:t>",
            xml(1).as_str(),
            "<w:t xml:space=\"preserve\"> B </w:t>",
            xml(2).as_str(),
            "</w:r><w:proofErr w:type=\"spellStart\"/>",
            "<w:r><w:t xml:space=\"preserve\"> C</w:t></w:r>",
            choice,
            "<w:p><w:r>",
            xml(3).as_str(),
            "</w:r></w:p>",
            fallback,
            "<w:p><w:r>",
            xml(4).as_str(),
            "</w:r></w:p>",
            end,
            "<w:r>",
            xml(5).as_str(),
            "</w:r></w:p>",
        ]
        .concat();
        assert_eq!(body(&doc), expected);
    }

    #[test]
    fn libreoffice_picture_placeholder_is_replaced() {
        let mut doc = XmlDocument::parse(LO_DOCUMENT_IMAGE).unwrap();
        assert_eq!(
            replace_with_element(&mut doc.root, "{{logo}}", || drawing(7)),
            1
        );
        let expected = std::str::from_utf8(LO_DOCUMENT_IMAGE).unwrap().replace(
            "<w:t>Before image {{logo}}</w:t>",
            &format!(
                "<w:t xml:space=\"preserve\">Before image </w:t>{}",
                drawing(7).to_xml()
            ),
        );
        assert_eq!(String::from_utf8(doc.to_bytes()).unwrap(), expected);
    }

    // ── Tables ───────────────────────────────────────────────────────────────

    #[test]
    fn placeholder_paragraph_becomes_a_table() {
        assert_eq!(
            tabled(
                concat!(
                    "<w:p><w:r><w:t>Intro</w:t></w:r></w:p>",
                    "<w:p w:rsidR=\"00A1\"><w:pPr><w:pStyle w:val=\"Body\"/></w:pPr>",
                    "<w:r><w:t xml:space=\"preserve\"> {{</w:t></w:r>",
                    "<w:proofErr w:type=\"spellStart\"/><w:r><w:rPr><w:b/></w:rPr>",
                    "<w:t>table}}</w:t></w:r><w:bookmarkStart w:id=\"0\" w:name=\"t\"/></w:p>",
                    "<w:p><w:r><w:t>Outro</w:t></w:r></w:p><w:sectPr/>",
                ),
                "{{table}}",
            ),
            (
                1,
                concat!(
                    // The bookmark stays, right before the table; the
                    // proofing mark goes with the text it flagged.
                    "<w:p><w:r><w:t>Intro</w:t></w:r></w:p>",
                    "<w:bookmarkStart w:id=\"0\" w:name=\"t\"/>",
                    "[T]<w:p><w:r><w:t>Outro</w:t></w:r></w:p><w:sectPr/>",
                )
                .to_owned()
            )
        );
    }

    #[test]
    fn paragraph_ending_a_section_stays_after_the_table() {
        assert_eq!(
            tabled(
                concat!(
                    "<w:p><w:pPr><w:pStyle w:val=\"Body\"/><w:sectPr>",
                    "<w:pgSz w:w=\"11906\" w:h=\"16838\"/></w:sectPr></w:pPr>",
                    "<w:r><w:t>{{table}}</w:t></w:r></w:p>",
                    "<w:p><w:r><w:t>Next section</w:t></w:r></w:p>",
                ),
                "{{table}}",
            ),
            (
                1,
                concat!(
                    "[T]<w:p><w:pPr><w:pStyle w:val=\"Body\"/><w:sectPr>",
                    "<w:pgSz w:w=\"11906\" w:h=\"16838\"/></w:sectPr></w:pPr></w:p>",
                    "<w:p><w:r><w:t>Next section</w:t></w:r></w:p>",
                )
                .to_owned()
            )
        );
    }

    #[test]
    fn paragraph_stays_after_the_table_when_no_paragraph_follows() {
        // Last in a table cell.
        assert_eq!(
            tabled(
                concat!(
                    "<w:tbl><w:tr><w:tc><w:tcPr><w:tcW w:w=\"0\" w:type=\"auto\"/></w:tcPr>",
                    "<w:p><w:pPr><w:jc w:val=\"center\"/></w:pPr><w:r><w:t>{{table}}</w:t>",
                    "</w:r></w:p></w:tc></w:tr></w:tbl><w:p/>",
                ),
                "{{table}}",
            ),
            (
                1,
                concat!(
                    "<w:tbl><w:tr><w:tc><w:tcPr><w:tcW w:w=\"0\" w:type=\"auto\"/></w:tcPr>",
                    "[T]<w:p><w:pPr><w:jc w:val=\"center\"/></w:pPr></w:p>",
                    "</w:tc></w:tr></w:tbl><w:p/>",
                )
                .to_owned()
            )
        );
        // Before a table, before the section properties, last in a
        // content control.
        assert_eq!(
            tabled(
                concat!(
                    "<w:p><w:r><w:t>{{table}}</w:t></w:r></w:p>",
                    "<w:tbl><w:tr><w:tc><w:p/></w:tc></w:tr></w:tbl>",
                    "<w:sdt><w:sdtContent><w:p><w:r><w:t>{{table}}</w:t></w:r></w:p>",
                    "</w:sdtContent></w:sdt>",
                    "<w:p><w:r><w:t>{{table}}</w:t></w:r></w:p><w:sectPr/>",
                ),
                "{{table}}",
            ),
            (
                3,
                concat!(
                    "[T]<w:p></w:p><w:tbl><w:tr><w:tc><w:p/></w:tc></w:tr></w:tbl>",
                    "<w:sdt><w:sdtContent>[T]<w:p></w:p></w:sdtContent></w:sdt>",
                    "[T]<w:p></w:p><w:sectPr/>",
                )
                .to_owned()
            )
        );
    }

    #[test]
    fn empty_paragraph_separates_the_table_from_a_table_before_it() {
        assert_eq!(
            tabled(
                concat!(
                    "<w:tbl><w:tr><w:tc><w:p/></w:tc></w:tr></w:tbl>",
                    "<w:p w:rsidR=\"00A1\"><w:r><w:t>{{table}}</w:t></w:r></w:p>",
                    "<w:p><w:r><w:t>after</w:t></w:r></w:p>",
                ),
                "{{table}}",
            ),
            (
                1,
                concat!(
                    "<w:tbl><w:tr><w:tc><w:p/></w:tc></w:tr></w:tbl><w:p/>[T]",
                    "<w:p><w:r><w:t>after</w:t></w:r></w:p>",
                )
                .to_owned()
            )
        );
    }

    #[test]
    fn consecutive_placeholder_paragraphs_become_separate_tables() {
        assert_eq!(
            tabled(
                concat!(
                    "<w:p><w:r><w:t>{{table}}</w:t></w:r></w:p>",
                    "<w:p><w:r><w:t>{{table}}</w:t></w:r></w:p>",
                    "\n<!-- between -->\n",
                    "<w:p><w:r><w:t>{{table}}</w:t></w:r></w:p><w:sectPr/>",
                ),
                "{{table}}",
            ),
            (
                3,
                "[T]<w:p/>[T]\n<!-- between -->\n<w:p/>[T]<w:p></w:p><w:sectPr/>".to_owned()
            )
        );
    }

    #[test]
    fn placeholder_paragraphs_in_text_boxes_become_tables() {
        let body_xml = [
            "<w:p><w:r><w:t>Box:</w:t></w:r>",
            &text_box("<w:p><w:r><w:t>{{table}}</w:t></w:r></w:p>"),
            "</w:p><w:sectPr/>",
        ]
        .concat();
        assert!(table_placeholders_are_alone(
            &document(&body_xml),
            "{{table}}"
        ));
        let expected = [
            "<w:p><w:r><w:t>Box:</w:t></w:r>",
            &text_box("[T]<w:p></w:p>"),
            "</w:p><w:sectPr/>",
        ]
        .concat();
        assert_eq!(tabled(&body_xml, "{{table}}"), (2, expected));
    }

    #[test]
    fn placeholders_sharing_their_paragraph_are_not_alone() {
        let with_text_box = [
            "<w:p><w:r><w:t>{{table}}</w:t></w:r>",
            &text_box("<w:p/>"),
            "</w:p>",
        ]
        .concat();
        for body_xml in [
            "<w:p><w:r><w:t>See {{table}} below</w:t></w:r></w:p>",
            "<w:p><w:r><w:tab/><w:t>{{table}}</w:t></w:r></w:p>",
            "<w:p><w:r><w:t>{{table}}</w:t></w:r><w:r><w:br w:type=\"page\"/></w:r></w:p>",
            &with_text_box,
            concat!(
                "<w:p><w:r><w:t>{{table}}</w:t></w:r></w:p>",
                "<w:p><w:hyperlink w:anchor=\"x\"><w:r><w:t>{{table}}</w:t></w:r>",
                "</w:hyperlink><w:r><w:t>!</w:t></w:r></w:p>",
            ),
            // The tracked deletion would be lost with the paragraph.
            concat!(
                "<w:p><w:del w:id=\"1\"><w:r><w:delText>x</w:delText></w:r></w:del>",
                "<w:r><w:t>{{table}}</w:t></w:r></w:p>",
            ),
        ] {
            assert!(
                !table_placeholders_are_alone(&document(body_xml), "{{table}}"),
                "{body_xml}"
            );
        }
        for body_xml in [
            "",
            "<w:p><w:r><w:t>{{table}}</w:t></w:r></w:p>",
            "<w:p><w:r><w:t>{{other}}</w:t></w:r></w:p>",
            // Split by a barrier, the placeholder is in no paragraph.
            "<w:p><w:r><w:t>{{tab</w:t><w:tab/><w:t>le}}</w:t></w:r></w:p>",
        ] {
            assert!(
                table_placeholders_are_alone(&document(body_xml), "{{table}}"),
                "{body_xml}"
            );
        }

        // Only the paragraphs holding nothing else are replaced.
        assert_eq!(
            tabled(
                concat!(
                    "<w:p><w:r><w:t>See {{table}}</w:t></w:r></w:p>",
                    "<w:p><w:r><w:t>{{table}}</w:t></w:r></w:p><w:p/>",
                ),
                "{{table}}",
            ),
            (
                1,
                "<w:p><w:r><w:t>See {{table}}</w:t></w:r></w:p>[T]<w:p/>".to_owned()
            )
        );

        // An empty placeholder is in no paragraph.
        let mut doc = document("<w:p/>");
        let table = Element::parse_fragment(TABLE).unwrap();
        assert!(table_placeholders_are_alone(&doc, ""));
        assert_eq!(replace_paragraphs_with_table(&mut doc, "", &table), 0);
    }

    #[test]
    fn paragraphs_holding_more_than_text_are_not_alone() {
        let tracked = "w:id=\"1\" w:author=\"Ada\" w:date=\"2024-05-01T10:00:00Z\"";
        for content in [
            // An inline equation, a sub-document or an unknown element would
            // be deleted with the paragraph.
            "<w:r><w:t>{{t}}</w:t></w:r><m:oMath><m:r><m:t>v</m:t></m:r></m:oMath>".to_owned(),
            "<w:r><w:t>{{t}}</w:t></w:r><w:subDoc r:id=\"rId9\"/>".to_owned(),
            "<w:r><w:t>{{t}}</w:t></w:r><w14:ext/>".to_owned(),
            // So would a content control, even one holding the placeholder.
            concat!(
                "<w:sdt><w:sdtPr><w:id w:val=\"5\"/></w:sdtPr><w:sdtContent>",
                "<w:r><w:t>{{t}}</w:t></w:r></w:sdtContent></w:sdt>",
            )
            .to_owned(),
            // And tracked deletions, from the revisions.
            format!(
                "<w:del {tracked}><w:r><w:delText>x</w:delText></w:r></w:del>\
                 <w:r><w:t>{{{{t}}}}</w:t></w:r>"
            ),
            format!(
                "<w:r><w:t>{{{{t}}}}</w:t></w:r>\
                 <w:moveFrom {tracked}><w:r><w:t>x</w:t></w:r></w:moveFrom>"
            ),
            concat!(
                "<w:r><w:t>{{t}}</w:t></w:r><mc:AlternateContent>",
                "<mc:Choice Requires=\"w14\"><w:r><w:t/></w:r></mc:Choice></mc:AlternateContent>",
            )
            .to_owned(),
        ] {
            let body_xml = format!("<w:p>{content}</w:p><w:p/>");
            assert!(
                !table_placeholders_are_alone(&document(&body_xml), "{{t}}"),
                "{content}"
            );
            assert_eq!(tabled(&body_xml, "{{t}}"), (0, body_xml.clone()));
        }

        // Text in wrappers other than content controls, between properties
        // and markers, goes with its paragraph.
        let only_text = concat!(
            "<w:p><w:pPr><w:jc w:val=\"center\"/></w:pPr><w:proofErr w:type=\"spellStart\"/>",
            "<w:hyperlink w:anchor=\"x\"><w:r><w:t>{</w:t></w:r></w:hyperlink>",
            "<w:ins w:id=\"2\" w:author=\"Ada\"><w:r><w:t>{</w:t></w:r></w:ins>",
            "<w:smartTag w:element=\"place\"><w:smartTagPr/><w:r><w:t>t</w:t></w:r></w:smartTag>",
            "<w:customXml w:element=\"c\"><w:customXmlPr/><w:fldSimple w:instr=\" PAGE \">",
            "<w:fldData>ZA==</w:fldData><w:dir w:val=\"rtl\"><w:bdo w:val=\"ltr\"><w:r><w:rPr>",
            "<w:b/></w:rPr><w:t>}</w:t><w:lastRenderedPageBreak/></w:r></w:bdo></w:dir>",
            "</w:fldSimple></w:customXml><w:moveTo w:id=\"3\" w:author=\"Ada\"><w:r>",
            "<w:t>}</w:t></w:r></w:moveTo><w:proofErr w:type=\"spellEnd\"/></w:p>",
        );
        assert!(table_placeholders_are_alone(&document(only_text), "{{t}}"));
        assert_eq!(
            tabled(only_text, "{{t}}"),
            (
                1,
                "[T]<w:p><w:pPr><w:jc w:val=\"center\"/></w:pPr></w:p>".to_owned()
            )
        );
    }

    /// Assert that every range under `root` that starts also ends, and the
    /// other way around: bookmarks, comments, permissions, ...
    fn assert_ranges_whole(root: &Element) {
        let mut starts = Vec::new();
        let mut ends = Vec::new();
        for marker in root.descendants().filter(|element| is_marker(element)) {
            let id = marker.attr("w:id").map(String::from);
            let name = marker.local_name();
            if let Some(kind) = name.strip_suffix("Start") {
                starts.push((kind.to_owned(), id));
            } else if let Some(kind) = name.strip_suffix("End") {
                ends.push((kind.to_owned(), id));
            }
        }
        starts.sort();
        ends.sort();
        assert_eq!(starts, ends, "{root:?}");
    }

    #[test]
    fn range_markers_of_a_removed_placeholder_paragraph_move_before_the_table() {
        let (count, xml) = tabled(
            concat!(
                "<w:p><w:r><w:t>Intro</w:t></w:r>",
                "<w:bookmarkStart w:id=\"1\" w:name=\"Before\"/></w:p>",
                "<w:p><w:bookmarkStart w:id=\"0\" w:name=\"_GoBack\"/>",
                "<w:commentRangeStart w:id=\"7\"/><w:permStart w:id=\"3\" w:edGrp=\"everyone\"/>",
                "<w:proofErr w:type=\"spellStart\"/><w:r><w:t>{{t</w:t></w:r>",
                "<w:proofErr w:type=\"spellEnd\"/><w:hyperlink w:anchor=\"x\">",
                "<w:bookmarkEnd w:id=\"1\"/><w:r><w:t>}}</w:t></w:r></w:hyperlink>",
                "<w:bookmarkEnd w:id=\"0\"/></w:p>",
                "<w:p><w:r><w:t>Outro</w:t></w:r><w:permEnd w:id=\"3\"/>",
                "<w:commentRangeEnd w:id=\"7\"/><w:r><w:rPr><w:rStyle w:val=\"CommentReference\"/>",
                "</w:rPr><w:commentReference w:id=\"7\"/></w:r></w:p>",
            ),
            "{{t}}",
        );
        assert_eq!(count, 1);
        assert_eq!(
            xml,
            concat!(
                "<w:p><w:r><w:t>Intro</w:t></w:r>",
                "<w:bookmarkStart w:id=\"1\" w:name=\"Before\"/></w:p>",
                "<w:bookmarkStart w:id=\"0\" w:name=\"_GoBack\"/>",
                "<w:commentRangeStart w:id=\"7\"/><w:permStart w:id=\"3\" w:edGrp=\"everyone\"/>",
                "<w:bookmarkEnd w:id=\"1\"/><w:bookmarkEnd w:id=\"0\"/>[T]",
                "<w:p><w:r><w:t>Outro</w:t></w:r><w:permEnd w:id=\"3\"/>",
                "<w:commentRangeEnd w:id=\"7\"/><w:r><w:rPr><w:rStyle w:val=\"CommentReference\"/>",
                "</w:rPr><w:commentReference w:id=\"7\"/></w:r></w:p>",
            )
        );
        assert_ranges_whole(&document(&xml.replace("[T]", TABLE)));
    }

    #[test]
    fn range_starts_precede_the_table_and_ends_stay_in_a_kept_paragraph() {
        // Last in its cell, the paragraph stays after the table: the
        // bookmark around the placeholder now holds the table.
        assert_eq!(
            tabled(
                concat!(
                    "<w:tbl><w:tr><w:tc><w:tcPr/><w:p><w:pPr><w:jc w:val=\"center\"/></w:pPr>",
                    "<w:bookmarkStart w:id=\"0\" w:name=\"Cell\"/>",
                    "<w:proofErr w:type=\"gramStart\"/>",
                    "<w:r><w:t>{{t}}</w:t></w:r><w:proofErr w:type=\"gramEnd\"/>",
                    "<w:bookmarkEnd w:id=\"0\"/></w:p></w:tc></w:tr></w:tbl><w:p/>",
                ),
                "{{t}}",
            ),
            (
                1,
                concat!(
                    "<w:tbl><w:tr><w:tc><w:tcPr/><w:bookmarkStart w:id=\"0\" w:name=\"Cell\"/>",
                    "[T]<w:p><w:pPr><w:jc w:val=\"center\"/></w:pPr>",
                    "<w:bookmarkEnd w:id=\"0\"/></w:p></w:tc></w:tr></w:tbl><w:p/>",
                )
                .to_owned()
            )
        );

        // Before a table that holds the ends of its ranges: a page
        // reference to "ItemsTable" must find the new table.
        let template_table = concat!(
            "<w:tbl><w:tr><w:tc><w:p><w:r><w:t>Total</w:t></w:r><w:bookmarkEnd w:id=\"5\"/>",
            "<w:commentRangeEnd w:id=\"7\"/></w:p></w:tc></w:tr></w:tbl>",
        );
        let (count, xml) = tabled(
            &[
                "<w:p><w:pPr><w:spacing w:after=\"120\"/></w:pPr>",
                "<w:bookmarkStart w:id=\"5\" w:name=\"ItemsTable\"/>",
                "<w:commentRangeStart w:id=\"7\"/>",
                "<w:r><w:t xml:space=\"preserve\"> {{</w:t></w:r><w:r><w:t>t}}</w:t></w:r></w:p>",
                template_table,
                "<w:p/>",
            ]
            .concat(),
            "{{t}}",
        );
        assert_eq!(count, 1);
        assert_eq!(
            xml,
            [
                "<w:bookmarkStart w:id=\"5\" w:name=\"ItemsTable\"/>",
                "<w:commentRangeStart w:id=\"7\"/>",
                "[T]<w:p><w:pPr><w:spacing w:after=\"120\"/></w:pPr></w:p>",
                template_table,
                "<w:p/>",
            ]
            .concat()
        );
        assert_ranges_whole(&document(&xml.replace("[T]", TABLE)));

        // Ending a section: a range that ended in the paragraph now ends
        // after the table, one that started in it starts before.
        let (count, xml) = tabled(
            concat!(
                "<w:p><w:r><w:t>Intro</w:t></w:r><w:commentRangeStart w:id=\"1\"/></w:p>",
                "<w:p><w:pPr><w:sectPr/></w:pPr><w:commentRangeEnd w:id=\"1\"/>",
                "<w:permStart w:id=\"2\" w:edGrp=\"everyone\"/><w:r><w:t>{{t}}</w:t></w:r>",
                "<w:permEnd w:id=\"2\"/></w:p><w:p><w:r><w:t>Next</w:t></w:r></w:p>",
            ),
            "{{t}}",
        );
        assert_eq!(count, 1);
        assert_eq!(
            xml,
            concat!(
                "<w:p><w:r><w:t>Intro</w:t></w:r><w:commentRangeStart w:id=\"1\"/></w:p>",
                "<w:permStart w:id=\"2\" w:edGrp=\"everyone\"/>[T]",
                "<w:p><w:pPr><w:sectPr/></w:pPr><w:commentRangeEnd w:id=\"1\"/>",
                "<w:permEnd w:id=\"2\"/></w:p><w:p><w:r><w:t>Next</w:t></w:r></w:p>",
            )
        );
        assert_ranges_whole(&document(&xml.replace("[T]", TABLE)));
    }

    #[test]
    fn moved_range_markers_keep_their_prefix_bound() {
        let mut doc = Element::parse_fragment(&format!(
            "<w:document xmlns:w=\"{W}\"><w:body><x:p xmlns:x=\"{W}\">\
             <x:hyperlink xmlns:y=\"{W}\"><y:bookmarkStart y:id=\"0\" y:name=\"b\"/>\
             <x:r><x:t>{{{{t}}}}</x:t></x:r></x:hyperlink><x:bookmarkEnd x:id=\"0\"/></x:p>\
             <w:p/><x:p xmlns:x=\"{W}\"><x:bookmarkStart x:id=\"1\" x:name=\"k\"/>\
             <x:r><x:t>{{{{t}}}}</x:t></x:r><x:bookmarkEnd x:id=\"1\"/></x:p></w:body></w:document>"
        ))
        .unwrap();
        let table = Element::parse_fragment(TABLE).unwrap();
        assert_eq!(replace_paragraphs_with_table(&mut doc, "{{t}}", &table), 2);
        // The end that stays in the kept paragraph needs no declaration.
        assert_eq!(
            serialize(&doc).replace(TABLE, "[T]"),
            format!(
                "<w:document xmlns:w=\"{W}\"><w:body>\
                 <y:bookmarkStart y:id=\"0\" y:name=\"b\" xmlns:y=\"{W}\"/>\
                 <x:bookmarkEnd x:id=\"0\" xmlns:x=\"{W}\"/>[T]<w:p/>\
                 <x:bookmarkStart x:id=\"1\" x:name=\"k\" xmlns:x=\"{W}\"/>[T]\
                 <x:p xmlns:x=\"{W}\"><x:bookmarkEnd x:id=\"1\"/></x:p></w:body></w:document>"
            )
        );
    }

    #[test]
    fn zero_width_markers_between_blocks_are_looked_past() {
        let table = "<w:tbl><w:tr><w:tc><w:p/></w:tc></w:tr></w:tbl>";
        let placeholder = "<w:p><w:r><w:t>{{t}}</w:t></w:r></w:p>";
        // After a table behind a marker: a paragraph keeps the two apart.
        assert_eq!(
            tabled(
                &[table, "<w:bookmarkEnd w:id=\"0\"/>", placeholder, "<w:p/>"].concat(),
                "{{t}}"
            ),
            (
                1,
                [table, "<w:bookmarkEnd w:id=\"0\"/><w:p/>[T]<w:p/>"].concat()
            )
        );
        // Before a table behind markers: the emptied paragraph stays.
        let markers = "<w:proofErr w:type=\"gramEnd\"/><w:bookmarkStart w:id=\"1\" w:name=\"b\"/>";
        assert_eq!(
            tabled(&[placeholder, markers, table, "<w:p/>"].concat(), "{{t}}"),
            (1, ["[T]<w:p></w:p>", markers, table, "<w:p/>"].concat())
        );
        // Before a paragraph behind a marker: it goes.
        let after = "<w:bookmarkEnd w:id=\"1\"/><w:p><w:r><w:t>after</w:t></w:r></w:p>";
        assert_eq!(
            tabled(&[placeholder, after].concat(), "{{t}}"),
            (1, ["[T]", after].concat())
        );
    }

    #[test]
    fn table_placeholders_are_trimmed_too() {
        let body_xml = "<w:p><w:r><w:t xml:space=\"preserve\">  {{t}}</w:t></w:r></w:p><w:p/>";
        assert!(table_placeholders_are_alone(
            &document(body_xml),
            " {{t}}\t"
        ));
        let shared = document("<w:p><w:r><w:t>a {{t}}</w:t></w:r></w:p>");
        assert!(!table_placeholders_are_alone(&shared, "\n{{t}} "));
        assert_eq!(tabled(body_xml, " {{t}} "), (1, "[T]<w:p/>".to_owned()));

        // Nothing but whitespace is no placeholder: it is in no paragraph.
        let blank = "<w:p><w:r><w:t xml:space=\"preserve\"> </w:t></w:r></w:p><w:p/>";
        assert!(table_placeholders_are_alone(&document(blank), " \t"));
        assert_eq!(tabled(blank, " \t"), (0, blank.to_owned()));
    }

    #[test]
    fn new_paragraphs_take_the_prefix_of_the_placeholder_paragraph() {
        let mut doc = Element::parse_fragment(&format!(
            "<ns0:document xmlns:ns0=\"{W}\"><ns0:body>\
             <ns0:tbl><ns0:tr><ns0:tc><ns0:p/></ns0:tc></ns0:tr></ns0:tbl>\
             <ns0:p><ns0:r><ns0:t>{{{{table}}}}</ns0:t></ns0:r></ns0:p>\
             <ns0:tbl/><x:p xmlns:x=\"{W}\"><x:r><x:t>{{{{table}}}}</x:t></x:r></x:p>\
             </ns0:body></ns0:document>"
        ))
        .unwrap();
        let table = Element::parse_fragment(TABLE).unwrap();
        assert_eq!(
            replace_paragraphs_with_table(&mut doc, "{{table}}", &table),
            2
        );
        assert_eq!(
            serialize(&doc).replace(TABLE, "[T]"),
            format!(
                "<ns0:document xmlns:ns0=\"{W}\"><ns0:body>\
                 <ns0:tbl><ns0:tr><ns0:tc><ns0:p/></ns0:tc></ns0:tr></ns0:tbl>\
                 <ns0:p/>[T]<ns0:p></ns0:p><ns0:tbl/><x:p xmlns:x=\"{W}\"/>[T]\
                 <x:p xmlns:x=\"{W}\"></x:p></ns0:body></ns0:document>"
            )
        );
    }

    #[test]
    fn generated_tables_are_inserted_and_never_searched() {
        let table = Table::new([["Item", "{{rows}}"], ["Apple", "3"]]).header(true);
        let table = Element::parse_fragment(&table.to_xml()).unwrap();
        let mut doc = document("<w:p><w:r><w:t>{{rows}}</w:t></w:r></w:p><w:sectPr/>");
        assert_eq!(
            replace_paragraphs_with_table(&mut doc, "{{rows}}", &table),
            1
        );
        assert_eq!(
            body(&doc),
            format!("{}<w:p></w:p><w:sectPr/>", table.to_xml())
        );
        // The copy holds the placeholder alone in a cell: only a second
        // pass replaces it.
        assert!(table_placeholders_are_alone(&doc, "{{rows}}"));
        assert_eq!(
            replace_paragraphs_with_table(&mut doc, "{{rows}}", &table),
            1
        );
    }

    #[test]
    fn word_template_tables_replace_their_placeholders() {
        let table = Element::parse_fragment(TABLE).unwrap();
        let mut doc = document(&word_body());
        assert!(table_placeholders_are_alone(&doc, "{{rows}}"));
        assert!(table_placeholders_are_alone(&doc, "{{item}}"));
        assert!(!table_placeholders_are_alone(&doc, "{{customer}}"));
        assert_eq!(
            replace_paragraphs_with_table(&mut doc, "{{rows}}", &table),
            2
        );
        let xml = body(&doc).replace(TABLE, "[T]");
        assert!(xml.contains("</w:tcPr>[T]<w:p></w:p></w:tc>"), "{xml}");
        assert!(xml.contains(concat!(
            "[T]<w:p><w:pPr><w:sectPr><w:pgSz w:w=\"11906\" w:h=\"16838\"/></w:sectPr>",
            "</w:pPr></w:p><w:p><w:r><w:t>Thanks</w:t>"
        )));
    }

    // ── Random documents ─────────────────────────────────────────────────────

    /// A deterministic pseudo-random generator (xorshift64*), so the
    /// random documents are the same on every run.
    struct Rng(u64);

    impl Rng {
        /// Return the next pseudo-random number.
        fn next_u64(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        /// Return a number below `n`.
        fn below(&mut self, n: usize) -> usize {
            usize::try_from(self.next_u64() % u64::try_from(n).unwrap()).unwrap()
        }

        /// Return one of `items`.
        fn pick<T: Copy>(&mut self, items: &[T]) -> T {
            items[self.below(items.len())]
        }
    }

    /// Raw `w:t` content for random documents: placeholder fragments,
    /// references, CDATA and multi-byte characters.
    const TEXTS: &[&str] = &[
        "{{",
        "}}",
        "a",
        "b",
        "{{a}}",
        "{{b}}",
        " ",
        "\u{e9}",
        "\u{65e5}",
        "&amp;",
        "&#x7B;",
        "<![CDATA[}]]>",
    ];

    /// Start and end tags of elements wrapping paragraph content, hidden
    /// ones included.
    const WRAPPER_TAGS: &[(&str, &str)] = &[
        ("<w:hyperlink r:id=\"rId1\">", "</w:hyperlink>"),
        ("<w:ins w:id=\"3\" w:author=\"A\">", "</w:ins>"),
        ("<w:moveTo w:id=\"4\" w:author=\"A\">", "</w:moveTo>"),
        ("<w:moveFrom w:id=\"5\" w:author=\"A\">", "</w:moveFrom>"),
        (
            "<w:smartTag w:element=\"place\"><w:smartTagPr/>",
            "</w:smartTag>",
        ),
        (
            "<w:sdt><w:sdtPr><w:id w:val=\"6\"/></w:sdtPr><w:sdtContent>",
            "</w:sdtContent></w:sdt>",
        ),
        (
            "<w:fldSimple w:instr=\" PAGE \"><w:fldData>ZA==</w:fldData>",
            "</w:fldSimple>",
        ),
        (
            "<w:customXml w:element=\"x\"><w:customXmlPr/>",
            "</w:customXml>",
        ),
        (
            "<w:dir w:val=\"rtl\"><w:bdo w:val=\"ltr\">",
            "</w:bdo></w:dir>",
        ),
        // Not a wrapper: a barrier, whose runs are not text.
        ("<w14:ext>", "</w14:ext>"),
    ];

    /// Zero-width markers, between runs or between blocks.
    const MARKUP: &[&str] = &[
        "<w:proofErr w:type=\"spellStart\"/>",
        "<w:bookmarkStart w:id=\"0\" w:name=\"b\"/>",
        "<w:bookmarkEnd w:id=\"0\"/>",
        "<w:commentRangeStart w:id=\"1\"/>",
        "<w:commentRangeEnd w:id=\"1\"/>",
        "<w:permStart w:id=\"2\" w:edGrp=\"everyone\"/>",
        "<w:permEnd w:id=\"2\"/>",
    ];

    /// Return random raw `w:t` content.
    fn random_text(rng: &mut Rng) -> String {
        (0..rng.below(4)).map(|_| rng.pick(TEXTS)).collect()
    }

    /// Return a random run.
    fn random_run(rng: &mut Rng) -> String {
        let mut run = String::from(rng.pick(&["<w:r>", "<w:r><w:rPr><w:i/></w:rPr>"]));
        for _ in 0..rng.below(4) {
            match rng.below(10) {
                0..=4 => {
                    run.push_str(rng.pick(&["<w:t>", "<w:t xml:space=\"preserve\">"]));
                    run.push_str(&random_text(rng));
                    run.push_str("</w:t>");
                }
                5 => run.push_str(rng.pick(&["<w:tab/>", "<w:br/>", "<w:sym w:char=\"F04A\"/>"])),
                6 => run.push_str("<w:lastRenderedPageBreak/>"),
                7 => run.push_str("<w:t/>"),
                8 => run.push_str("<w:t>{{<w:x/>a}}</w:t>"),
                _ => run.push_str("\n "),
            }
        }
        run.push_str("</w:r>");
        run
    }

    /// Return random paragraph content; `depth` bounds its nesting.
    fn random_inline(rng: &mut Rng, depth: usize) -> String {
        let children = |rng: &mut Rng| -> String {
            (0..=rng.below(3))
                .map(|_| random_inline(rng, depth - 1))
                .collect()
        };
        match rng.below(19) {
            8 => rng.pick(MARKUP).to_owned(),
            9 => format!(
                "<w:del w:id=\"2\" w:author=\"A\"><w:r><w:delText>{{{{a}}}}</w:delText></w:r>\
                 <w:r><w:t>{}</w:t></w:r></w:del>",
                random_text(rng)
            ),
            10 if depth > 0 => {
                let (start, end) = rng.pick(WRAPPER_TAGS);
                [start, &children(rng), end].concat()
            }
            11 if depth > 0 => format!(
                "<mc:AlternateContent><mc:Choice Requires=\"w14\">{}</mc:Choice>\
                 <mc:Fallback>{}</mc:Fallback></mc:AlternateContent>",
                children(rng),
                children(rng)
            ),
            12 if depth > 0 => text_box(&random_paragraph(rng, depth - 1)),
            13 if depth > 0 => random_paragraph(rng, depth - 1),
            14 => "<m:oMath><m:r><m:t>{{a}}</m:t></m:r></m:oMath>".to_owned(),
            15 => "<w:subDoc r:id=\"rId9\"/>\n  ".to_owned(),
            16 if depth > 0 => ["<w14:ext>", &children(rng), "</w14:ext>"].concat(),
            _ => random_run(rng),
        }
    }

    /// Return the start tag of a random paragraph, with a random
    /// `w14:paraId` to tell it from the others.
    fn paragraph_start(rng: &mut Rng) -> String {
        format!("<w:p w14:paraId=\"{:016X}\">", rng.next_u64())
    }

    /// Return a random paragraph; `depth` bounds the nesting of its
    /// content.
    fn random_paragraph(rng: &mut Rng, depth: usize) -> String {
        let mut paragraph = paragraph_start(rng);
        if rng.below(3) == 0 {
            paragraph.push_str(rng.pick(&[
                "<w:pPr><w:pStyle w:val=\"Body\"/></w:pPr>",
                "<w:pPr><w:sectPr/></w:pPr>",
                "<w:pPr><w:rPr><w:b/></w:rPr></w:pPr>",
            ]));
        }
        for _ in 0..rng.below(6) {
            paragraph.push_str(&random_inline(rng, depth));
        }
        paragraph.push_str("</w:p>");
        paragraph
    }

    /// Return a paragraph whose text is `{{a}}`, perhaps with spaces
    /// around it, split into runs at random, with markers between them.
    fn placeholder_paragraph(rng: &mut Rng) -> String {
        let mut rest = rng.pick(&["{{a}}", " {{a}}", "{{a}}  ", "\u{a0}{{a}} "]);
        let mut paragraph = paragraph_start(rng);
        while !rest.is_empty() {
            let ends: Vec<usize> = rest
                .char_indices()
                .map(|(start, c)| start + c.len_utf8())
                .take(3)
                .collect();
            let (atom, tail) = rest.split_at(rng.pick(&ends));
            let run = ["<w:r><w:t xml:space=\"preserve\">", atom, "</w:t></w:r>"].concat();
            if rng.below(4) == 0 {
                let (start, end) = rng.pick(&WRAPPER_TAGS[..2]);
                paragraph.push_str(&[start, &run, end].concat());
            } else {
                paragraph.push_str(&run);
            }
            if rng.below(2) == 0 {
                paragraph.push_str(rng.pick(MARKUP));
            }
            rest = tail;
        }
        paragraph.push_str("</w:p>");
        paragraph
    }

    /// Return a random block: a paragraph, a table, a content control or
    /// a text box, many of them holding nothing but `{{a}}`.
    fn random_block(rng: &mut Rng) -> String {
        let paragraph = |rng: &mut Rng| {
            if rng.below(2) == 0 {
                placeholder_paragraph(rng)
            } else {
                random_paragraph(rng, 2)
            }
        };
        match rng.below(6) {
            0 => {
                let mut table = String::from("<w:tbl><w:tr>");
                for _ in 0..=rng.below(2) {
                    table.push_str("<w:tc><w:tcPr/>");
                    for _ in 0..=rng.below(2) {
                        table.push_str(&paragraph(rng));
                    }
                    table.push_str("</w:tc>");
                }
                table.push_str("</w:tr></w:tbl>");
                table
            }
            1 => format!(
                "<w:sdt><w:sdtContent>{}</w:sdtContent></w:sdt>",
                paragraph(rng)
            ),
            2 => format!("<w:p>{}</w:p>", text_box(&paragraph(rng))),
            _ => paragraph(rng),
        }
    }

    /// Return a random body.
    fn random_body(rng: &mut Rng) -> String {
        let mut body = String::new();
        for _ in 0..=rng.below(6) {
            if rng.below(3) == 0 {
                body.push_str(rng.pick(MARKUP));
            }
            body.push_str(&random_block(rng));
        }
        if rng.below(2) == 0 {
            body.push_str("<w:sectPr/>");
        }
        body
    }

    /// Replace `pairs` in `text` the slow and obvious way: at each
    /// position, the longest key starting there (the first listed on a
    /// tie) is replaced and the scan goes on after it. Return the result
    /// and the number of replacements.
    fn naive_replace(text: &str, pairs: &[(&str, &str)]) -> (String, usize) {
        let mut out = String::new();
        let mut count = 0;
        let mut rest = text;
        while let Some(c) = rest.chars().next() {
            let mut longest: Option<(&str, &str)> = None;
            for &(key, value) in pairs {
                if !key.is_empty()
                    && rest.starts_with(key)
                    && longest.is_none_or(|(best, _)| key.len() > best.len())
                {
                    longest = Some((key, value));
                }
            }
            if let Some((key, value)) = longest {
                out.push_str(value);
                rest = &rest[key.len()..];
                count += 1;
            } else {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
        (out, count)
    }

    /// Return the segments of paragraphs whose segments were `before` once
    /// `pairs` are replaced the naive way, tabs and line breaks in values
    /// becoming barriers, and the number of replacements.
    fn naive_segments(before: &[Vec<String>], pairs: &[(&str, &str)]) -> (Vec<Vec<String>>, usize) {
        let mut count = 0;
        let mut after = Vec::new();
        for paragraph in before {
            let mut segments = Vec::new();
            for segment in paragraph {
                let (text, replaced) = naive_replace(segment, pairs);
                count += replaced;
                segments.extend(
                    text.split(['\t', '\n'])
                        .filter(|text| !text.is_empty())
                        .map(str::to_owned),
                );
            }
            after.push(segments);
        }
        (after, count)
    }

    /// Return the hidden content under `root` (see [`is_hidden`]),
    /// serialized, in document order.
    fn hidden(root: &Element) -> Vec<String> {
        let mut found = Vec::new();
        for child in root.child_elements() {
            if is_hidden(child) {
                found.push(child.to_xml());
            } else {
                found.extend(hidden(child));
            }
        }
        found
    }

    /// Return the number of paragraphs under `root` that hold nothing but
    /// `{{a}}`.
    fn only_placeholder(root: &Element) -> usize {
        let mut count = 0;
        every_paragraph(root, &mut |paragraph| {
            count += usize::from(TextModel::new(paragraph).is_only("{{a}}"));
            true
        });
        count
    }

    /// Return the range markers under `root`, serialized and sorted.
    fn range_markers(root: &Element) -> Vec<String> {
        let mut markers: Vec<String> = root
            .descendants()
            .filter(|element| is_marker(element) && !element.is(Ns::W, "proofErr"))
            .map(Element::to_xml)
            .collect();
        markers.sort();
        markers
    }

    /// Whether `element` is a copy of [`TABLE`].
    fn is_inserted(element: &Element) -> bool {
        element
            .find_child(Ns::W, "tblPr")
            .and_then(|properties| properties.find_child(Ns::W, "tblStyle"))
            .is_some_and(|style| style.attr("w:val").as_deref() == Some("Inserted"))
    }

    /// Check that every copy of [`TABLE`] under `root` has a paragraph
    /// right after it and no table right before it, zero-width markers
    /// aside.
    fn check_inserted_tables(root: &Element) {
        let children: Vec<&Element> = root
            .child_elements()
            .filter(|child| !is_marker(child))
            .collect();
        for (index, child) in children.iter().enumerate() {
            if is_inserted(child) {
                let next = children.get(index + 1);
                let previous = index.checked_sub(1).and_then(|index| children.get(index));
                assert!(next.is_some_and(|next| next.is(Ns::W, "p")), "{root:?}");
                assert!(
                    !previous.is_some_and(|previous| previous.is(Ns::W, "tbl")),
                    "{root:?}"
                );
            }
            check_inserted_tables(child);
        }
    }

    /// A paragraph that holds nothing but `{{a}}`, as
    /// [`check_range_placement`] sees it.
    struct PlaceholderRanges {
        /// Its `w14:paraId`.
        id: Option<String>,
        /// Its range markers, serialized, each with whether it starts its
        /// range.
        markers: Vec<(String, bool)>,
    }

    /// Return the paragraphs under `root` holding nothing but `{{a}}`, in
    /// document order.
    fn placeholder_ranges(root: &Element) -> Vec<PlaceholderRanges> {
        let mut placeholders = Vec::new();
        every_paragraph(root, &mut |paragraph| {
            if TextModel::new(paragraph).is_only("{{a}}") {
                placeholders.push(PlaceholderRanges {
                    id: paragraph.attr("w14:paraId").map(String::from),
                    markers: paragraph
                        .descendants()
                        .filter(|element| is_marker(element) && !element.is(Ns::W, "proofErr"))
                        .map(|marker| (marker.to_xml(), starts_range(marker)))
                        .collect(),
                });
            }
            true
        });
        placeholders
    }

    /// Append the copies of [`TABLE`] under `root` to `tables` in document
    /// order, each as the range markers right before it, serialized, and
    /// the element right after it.
    fn inserted_tables<'a>(
        root: &'a Element,
        tables: &mut Vec<(Vec<String>, Option<&'a Element>)>,
    ) {
        let children: Vec<&Element> = root.child_elements().collect();
        for (index, child) in children.iter().enumerate() {
            if is_inserted(child) {
                let mut before: Vec<String> = children[..index]
                    .iter()
                    .rev()
                    .take_while(|sibling| is_marker(sibling))
                    .map(|marker| marker.to_xml())
                    .collect();
                before.reverse();
                tables.push((before, children.get(index + 1).copied()));
            }
            inserted_tables(child, tables);
        }
    }

    /// Check that the range markers of each paragraph of `original` that
    /// `replaced` has a table for surround that table: all of them right
    /// before it when the paragraph went; the starts right before it and
    /// the ends in the paragraph, right after the table, when it stayed.
    fn check_range_placement(original: &Element, replaced: &Element) {
        let placeholders = placeholder_ranges(original);
        let mut tables = Vec::new();
        inserted_tables(replaced, &mut tables);
        assert_eq!(placeholders.len(), tables.len());
        for (placeholder, (before, after)) in placeholders.iter().zip(&tables) {
            let id = placeholder.id.as_deref();
            let kept = after.filter(|after| {
                after.is(Ns::W, "p") && id.is_some() && after.attr("w14:paraId").as_deref() == id
            });
            let mut leaving = Vec::new();
            let mut staying = Vec::new();
            for (xml, starts) in &placeholder.markers {
                if kept.is_none() || *starts {
                    leaving.push(xml.clone());
                } else {
                    staying.push(xml.clone());
                }
            }
            assert!(before.ends_with(&leaving), "{before:?} {leaving:?}");
            if let Some(kept) = kept {
                let inside: Vec<String> = kept
                    .child_elements()
                    .filter(|element| is_marker(element))
                    .map(Element::to_xml)
                    .collect();
                assert_eq!(inside, staying, "{kept:?}");
            }
        }
    }

    #[test]
    fn random_documents_agree_with_a_naive_implementation() {
        let pairs = [
            ("{{a}}", "A\tz"),
            ("{{b}}", ""),
            ("a}}{{", "X"),
            ("\u{65e5} ", "\n"),
            ("{{", "<{{>"),
            ("\u{e9}", "\u{e9}\u{e9}"),
        ];
        let table = Element::parse_fragment(TABLE).unwrap();
        for seed in 1..=400_u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let body_xml = random_body(&mut rng);
            let original = document(&body_xml);
            let before = segments(&original);
            let hidden_before = hidden(&original);

            // Text replacement works on each segment on its own; the tabs
            // and line breaks of values become barriers.
            let (expected, count) = naive_segments(&before, &pairs);
            let mut doc = original.clone();
            assert_eq!(replace_text(&mut doc, &pairs), count, "{body_xml}");
            let doc = document(&body(&doc));
            assert_eq!(segments(&doc), expected, "{body_xml}");
            assert_eq!(hidden(&doc), hidden_before, "{body_xml}");

            // An inserted element is a barrier where the occurrence was.
            let (expected, count) = naive_segments(&before, &[("{{a}}", "\t")]);
            let mut doc = original.clone();
            let mut made = 0;
            let replaced = replace_with_element(&mut doc, "{{a}}", || {
                made += 1;
                drawing(made)
            });
            assert_eq!((replaced, usize::try_from(made).unwrap()), (count, count));
            let doc = document(&body(&doc));
            assert_eq!(segments(&doc), expected, "{body_xml}");
            assert_eq!(hidden(&doc), hidden_before, "{body_xml}");

            // Every paragraph holding only the placeholder gets a table,
            // with a paragraph after it and no table before it.
            // Their range markers stay in the document, around the tables.
            let mut doc = original.clone();
            let replaced = replace_paragraphs_with_table(&mut doc, "{{a}}", &table);
            assert_eq!(replaced, only_placeholder(&original), "{body_xml}");
            let doc = document(&body(&doc));
            assert_eq!(only_placeholder(&doc), 0, "{body_xml}");
            check_inserted_tables(&doc);
            check_range_placement(&original, &doc);
            assert_eq!(range_markers(&doc), range_markers(&original), "{body_xml}");
        }
    }

    // ── Limits ───────────────────────────────────────────────────────────────

    /// Return the drawing [`Image`] makes, whose `wp:docPr` id is `id`.
    fn image_drawing(id: u32) -> Element {
        let image = Image::from_bytes(PNG_3X2).unwrap();
        Element::parse_fragment(&image.drawing_xml("rId9", id)).unwrap()
    }

    /// Return the table [`Table`] makes for two rows of two cells.
    fn generated_table() -> Element {
        Element::parse_fragment(&Table::new([["a", "b"], ["c", "d"]]).to_xml()).unwrap()
    }

    /// Return a document whose root declares `declarations` namespaces in
    /// all, and whose body holds `body` in `wrappers` levels of elements
    /// without a namespace.
    fn limits_document(wrappers: usize, declarations: usize, body: &str) -> Element {
        let mut more = String::new();
        for index in 1..declarations {
            write!(more, " xmlns:n{index}=\"urn:n\"").unwrap();
        }
        let xml = format!(
            "<w:document xmlns:w=\"{W}\"{more}><w:body>{}{body}{}</w:body></w:document>",
            "<a>".repeat(wrappers),
            "</a>".repeat(wrappers)
        );
        Element::parse_fragment(&xml).unwrap()
    }

    /// Replace `{{img}}` with drawings from [`image_drawing`] in `doc`;
    /// return how many, and how many drawings were made.
    fn imaged(doc: &mut Element) -> (usize, u32) {
        let mut made = 0;
        let count = replace_with_element(doc, "{{img}}", || {
            made += 1;
            image_drawing(made)
        });
        (count, made)
    }

    #[test]
    fn generated_snippets_have_the_shapes_the_limits_expect() {
        assert_eq!(
            Shape::of(&image_drawing(1)),
            Shape {
                height: 8,
                declarations: 5
            }
        );
        assert_eq!(
            Shape::of(&generated_table()),
            Shape {
                height: 6,
                declarations: 1
            }
        );
        // Declarations add up along each path from the root.
        let tree = Element::parse_fragment(
            "<a xmlns:x=\"u\"><b xmlns:y=\"u\" xmlns:z=\"u\"/><c xmlns:y=\"u\"><d/></c></a>",
        )
        .unwrap();
        assert_eq!(
            Shape::of(&tree),
            Shape {
                height: 3,
                declarations: 3
            }
        );
    }

    #[test]
    fn images_too_deep_to_parse_again_are_left_in_place() {
        // The w:t holding the placeholder is 5 levels below the wrappers:
        // the 8 levels of a drawing next to it end at depth 512 for a w:t
        // at 505, at 513 for one at 506.
        let placeholder = "<w:p><w:r><w:t>{{img}} and {{img}}</w:t></w:r></w:p>";
        let mut doc = limits_document(500, 1, placeholder);
        assert_eq!(imaged(&mut doc), (2, 2));
        serialize(&doc);

        let mut doc = limits_document(501, 1, placeholder);
        let before = doc.to_xml();
        // `make` runs once, to learn how deep a drawing is.
        assert_eq!(imaged(&mut doc), (0, 1));
        assert_eq!(doc.to_xml(), before);

        // In a hyperlink, one level deeper, the first occurrence doesn't fit
        // and stays whole, the second does.
        let mut doc = limits_document(
            500,
            1,
            concat!(
                "<w:p><w:hyperlink w:anchor=\"x\"><w:r><w:t>{{im</w:t></w:r></w:hyperlink>",
                "<w:r><w:t>g}} {{img}}</w:t></w:r></w:p>",
            ),
        );
        assert_eq!(imaged(&mut doc), (1, 2));
        assert_eq!(paragraphs(&doc), ["{{img}} "]);
        let xml = serialize(&doc);
        assert!(
            xml.contains("<w:hyperlink w:anchor=\"x\"><w:r><w:t>{{im</w:t></w:r></w:hyperlink>")
        );
        assert!(xml.contains(&image_drawing(2).to_xml()));
    }

    #[test]
    fn tables_too_deep_to_parse_again_are_left_in_place() {
        // The paragraph is 1 level below the wrappers: the 6 levels of a
        // table in its place end at depth 512 for a paragraph at 507, at 513
        // for one at 508.
        let table = generated_table();
        let placeholder = "<w:p><w:r><w:t>{{tbl}}</w:t></w:r></w:p>";
        let mut doc = limits_document(504, 1, placeholder);
        assert_eq!(
            replace_paragraphs_with_table(&mut doc, "{{tbl}}", &table),
            1
        );
        serialize(&doc);

        let mut doc = limits_document(505, 1, placeholder);
        let before = doc.to_xml();
        assert!(table_placeholders_are_alone(&doc, "{{tbl}}"));
        assert_eq!(
            replace_paragraphs_with_table(&mut doc, "{{tbl}}", &table),
            0
        );
        assert_eq!(doc.to_xml(), before);
    }

    #[test]
    fn insertions_under_too_many_namespace_declarations_are_left_in_place() {
        // A drawing declares 5 namespaces: 123 in scope leave room for it,
        // 124 don't.
        let placeholder = "<w:p><w:r><w:t>{{img}}</w:t></w:r></w:p>";
        let mut doc = limits_document(0, 123, placeholder);
        assert_eq!(imaged(&mut doc), (1, 1));
        serialize(&doc);
        let mut doc = limits_document(0, 124, placeholder);
        let before = doc.to_xml();
        assert_eq!(imaged(&mut doc), (0, 1));
        assert_eq!(doc.to_xml(), before);
        // Declarations below the root are in scope too.
        let mut doc = limits_document(
            0,
            120,
            concat!(
                "<w:p xmlns:p1=\"urn:p\" xmlns:p2=\"urn:p\"><w:r xmlns:r1=\"urn:r\"",
                " xmlns:r2=\"urn:r\"><w:t>{{img}}</w:t></w:r></w:p>",
            ),
        );
        assert_eq!(imaged(&mut doc).0, 0);

        // A table declares one: 127 leave room for it, 128 don't.
        let table = generated_table();
        let placeholder = "<w:p><w:r><w:t>{{tbl}}</w:t></w:r></w:p>";
        let mut doc = limits_document(0, 127, placeholder);
        assert_eq!(
            replace_paragraphs_with_table(&mut doc, "{{tbl}}", &table),
            1
        );
        serialize(&doc);
        let mut doc = limits_document(0, 128, placeholder);
        let before = doc.to_xml();
        assert_eq!(
            replace_paragraphs_with_table(&mut doc, "{{tbl}}", &table),
            0
        );
        assert_eq!(doc.to_xml(), before);
    }

    #[test]
    fn tables_nested_over_and_over_stop_at_the_limits_on_a_small_stack() {
        // Each table holds the placeholder again, so each call puts a table
        // in the cell of the last one: 3 levels deeper, and under one more
        // declaration of `w`. With only the root's declaration, 127 tables
        // fit in the 128 declarations allowed. The tree stays shallow
        // enough for the recursive walks on a quarter of the 2 MiB that
        // spawned threads get by default.
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(|| {
                let table = Element::parse_fragment(&Table::new([["{{t}}"]]).to_xml()).unwrap();
                let mut doc = limits_document(0, 1, "<w:p><w:r><w:t>{{t}}</w:t></w:r></w:p>");
                let mut calls = 0;
                while replace_paragraphs_with_table(&mut doc, "{{t}}", &table) == 1 {
                    calls += 1;
                    assert!(calls <= 127, "nesting doesn't stop");
                }
                assert_eq!(calls, 127);
                assert!(table_placeholders_are_alone(&doc, "{{t}}"));
                serialize(&doc);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn the_longest_prefix_leaves_room_for_breaks_and_tabs() {
        // A prefix is declared by an attribute named `xmlns:` and the
        // prefix, which can't be longer than 256 bytes: `{prefix}:tab`, the
        // longest name a text replacement adds, fits the same limit.
        let prefix = "p".repeat(MAX_NAME_LEN - "xmlns:".len());
        let mut doc = Element::parse_fragment(&format!(
            "<w:document xmlns:w=\"{W}\"><w:body><w:p><{prefix}:r xmlns:{prefix}=\"{W}\">\
             <{prefix}:t>{{{{a}}}}</{prefix}:t></{prefix}:r></w:p></w:body></w:document>"
        ))
        .unwrap();
        assert_eq!(replace_text(&mut doc, &[("{{a}}", "1\t2\n3")]), 1);
        let xml = serialize(&doc);
        assert!(xml.contains(&format!("<{prefix}:tab/>")), "{xml}");
        assert!(xml.contains(&format!("<{prefix}:br/>")), "{xml}");
    }

    // ── Robustness ───────────────────────────────────────────────────────────

    #[test]
    fn samples_survive_every_operation() {
        let table = Element::parse_fragment(TABLE).unwrap();
        let word = [START, &word_body(), END].concat();
        for xml in [
            word.as_bytes(),
            LO_DOCUMENT,
            LO_DOCUMENT_IMAGE,
            LO_STYLES,
            LO_CONTENT_TYPES,
        ] {
            exercise(&XmlDocument::parse(xml).unwrap().root, &table, PLACEHOLDERS);
        }
    }

    #[test]
    fn text_holding_an_element_is_a_barrier() {
        let pairs = [("{{a}}", "b")];
        // A w:t holding an element is a barrier, never rewritten.
        assert_eq!(
            replaced(
                "<w:p><w:r><w:t>{{a}}<w:x/></w:t><w:t>{{a}}</w:t></w:r></w:p>",
                &pairs
            ),
            (
                1,
                "<w:p><w:r><w:t>{{a}}<w:x/></w:t><w:t xml:space=\"preserve\">b</w:t></w:r></w:p>"
                    .to_owned()
            )
        );
    }

    #[test]
    fn text_outside_paragraph_runs_is_not_replaced() {
        let pairs = [("{{a}}", "b")];
        // Runs and text outside paragraphs, text outside w:t, a run in a
        // run, DrawingML text and a w:p in another namespace are not
        // paragraph text.
        let unchanged = concat!(
            "<w:r><w:t>{{a}}</w:t></w:r><w:t>{{a}}</w:t>{{a}}",
            "<w:p>{{a}}<w:r>{{a}}<w:r><w:t>{{a}}</w:t></w:r></w:r>",
            "<w:r><w:drawing>",
            "<a:p xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">",
            "<a:r><a:t>{{a}}</a:t></a:r></a:p></w:drawing></w:r></w:p>",
            "<w:p xmlns:w=\"urn:not-word\"><w:r><w:t>{{a}}</w:t></w:r></w:p>",
        );
        assert_eq!(replaced(unchanged, &pairs), (0, unchanged.to_owned()));
    }

    #[test]
    fn nested_paragraphs_and_alternate_branches_are_replaced_on_their_own() {
        let pairs = [("{{a}}", "b")];
        // A paragraph in a paragraph is a barrier there, and replaced on
        // its own; so are the branches of alternate content.
        assert_eq!(
            replaced(
                concat!(
                    "<w:p><w:r><w:t>{{</w:t></w:r><w:p><w:r><w:t>{{a}}</w:t></w:r></w:p>",
                    "<w:r><w:t>a}}</w:t></w:r></w:p>",
                    "<w:p><w:r><w:t>{{</w:t></w:r><mc:AlternateContent>",
                    "<mc:Choice Requires=\"w14\"><w:r><w:t>a}}{{a}}</w:t></w:r></mc:Choice>",
                    "<mc:Fallback><w:r><w:t>{{a}}</w:t></w:r></mc:Fallback>",
                    "</mc:AlternateContent></w:p>",
                ),
                &pairs,
            ),
            (
                3,
                concat!(
                    "<w:p><w:r><w:t>{{</w:t></w:r><w:p><w:r><w:t xml:space=\"preserve\">b</w:t>",
                    "</w:r></w:p><w:r><w:t>a}}</w:t></w:r></w:p>",
                    "<w:p><w:r><w:t>{{</w:t></w:r><mc:AlternateContent>",
                    "<mc:Choice Requires=\"w14\"><w:r><w:t xml:space=\"preserve\">a}}b</w:t></w:r>",
                    "</mc:Choice><mc:Fallback><w:r><w:t xml:space=\"preserve\">b</w:t></w:r>",
                    "</mc:Fallback></mc:AlternateContent></w:p>",
                )
                .to_owned()
            )
        );
    }

    #[test]
    fn cdata_comments_and_references_in_text_are_matched() {
        let pairs = [("{{a}}", "b")];
        // CDATA, comments and character references in a w:t.
        assert_eq!(
            replaced(
                "<w:p><w:r><w:t>&#x7B;{<![CDATA[a]]>}}<!-- c --></w:t></w:r></w:p>",
                &pairs
            ),
            (
                1,
                "<w:p><w:r><w:t xml:space=\"preserve\">b</w:t></w:r></w:p>".to_owned()
            )
        );
    }

    #[test]
    fn paragraphs_are_found_under_any_root() {
        let pairs = [("{{a}}", "b")];
        // Empty documents, other story roots, a paragraph as the root.
        let table = Element::parse_fragment(TABLE).unwrap();
        for (xml, count) in [
            (format!("<w:document xmlns:w=\"{W}\"/>"), 0),
            (
                format!("<w:document xmlns:w=\"{W}\"><w:body/></w:document>"),
                0,
            ),
            (
                format!("<w:p xmlns:w=\"{W}\"><w:r><w:t>{{{{a}}}}</w:t></w:r></w:p>"),
                1,
            ),
            (
                format!(
                    "<w:footnotes xmlns:w=\"{W}\"><w:footnote w:type=\"separator\" w:id=\"-1\">\
                     <w:p><w:r><w:separator/></w:r></w:p></w:footnote><w:footnote w:id=\"1\">\
                     <w:p><w:r><w:t>{{{{a}}}}</w:t></w:r></w:p></w:footnote></w:footnotes>"
                ),
                1,
            ),
        ] {
            let root = Element::parse_fragment(&xml).unwrap();
            let mut doc = root.clone();
            assert_eq!(replace_text(&mut doc, &pairs), count, "{xml}");
            exercise(&root, &table, PLACEHOLDERS);
        }
    }

    #[test]
    fn mutated_documents_survive_every_operation() {
        // Every byte of a small story replaced in turn by markup and name
        // characters: whatever still parses goes through every operation.
        let table = Element::parse_fragment(TABLE).unwrap();
        let xml = format!(
            "<w:document xmlns:w=\"{W}\" xmlns:mc=\"urn:mc\" xmlns:v=\"urn:v\"><w:body>\
             <w:p><w:r><w:t>{{{{</w:t></w:r><w:proofErr/><w:r><w:t>a}}}}</w:t></w:r>\
             <w:r><mc:AlternateContent><mc:Choice><w:drawing><w:txbxContent><w:p><w:r>\
             <w:t>{{{{a}}}}</w:t></w:r></w:p></w:txbxContent></w:drawing></mc:Choice>\
             <mc:Fallback><w:pict><v:textbox><w:txbxContent><w:p><w:r><w:t>{{{{a}}}}</w:t>\
             </w:r></w:p></w:txbxContent></v:textbox></w:pict></mc:Fallback>\
             </mc:AlternateContent></w:r><w:r><w:tab/><w:t>{{{{b}}}}</w:t></w:r></w:p>\
             <w:tbl><w:tr><w:tc><w:p><w:r><w:t>{{{{a}}}}</w:t></w:r></w:p></w:tc></w:tr></w:tbl>\
             <w:p><w:pPr><w:sectPr/></w:pPr><w:r><w:t>{{{{a}}}}</w:t></w:r></w:p><w:sectPr/>\
             </w:body></w:document>"
        );
        let mut bytes = xml.into_bytes();
        for index in 0..bytes.len() {
            let original = bytes[index];
            for byte in *b"<>/\" wptr{}" {
                bytes[index] = byte;
                if let Ok(doc) = XmlDocument::parse(&bytes) {
                    exercise(&doc.root, &table, &["{{a}}", "a"]);
                }
            }
            bytes[index] = original;
        }
    }

    #[test]
    fn deep_trees_are_handled_on_a_small_stack() {
        // Text boxes nested 80 deep through their VML form (6 levels
        // each), a paragraph in 250 wrappers and content controls 120 deep:
        // all close to the 512 levels the parser allows.
        let mut nested = "<w:p><w:r><w:t>{{a}}</w:t></w:r></w:p>".to_owned();
        for _ in 0..80 {
            nested = format!(
                "<w:p><w:r><w:t>{{{{a}}}}</w:t></w:r><w:r><w:pict><v:shape><v:textbox>\
                 <w:txbxContent>{nested}</w:txbxContent></v:textbox></v:shape></w:pict></w:r></w:p>"
            );
        }
        let wrapped = format!(
            "<w:p>{}<w:r><w:t>{{{{a}}}}</w:t></w:r>{}</w:p>",
            "<w:customXml w:element=\"x\">".repeat(250),
            "</w:customXml>".repeat(250)
        );
        let controls = format!(
            "{}<w:p><w:r><w:t>{{{{a}}}}</w:t></w:r></w:p>{}",
            "<w:sdt><w:sdtContent>".repeat(120),
            "</w:sdtContent></w:sdt>".repeat(120)
        );
        let xml = [START, &nested, &wrapped, &controls, END].concat();
        // A quarter of the 2 MiB that spawned threads get by default.
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || {
                let root = XmlDocument::parse(xml.as_bytes()).unwrap().root;
                let table = Element::parse_fragment(TABLE).unwrap();
                let mut doc = root.clone();
                assert_eq!(replace_text(&mut doc, &[("{{a}}", "b")]), 83);
                let mut doc = root.clone();
                assert_eq!(replace_with_element(&mut doc, "{{a}}", || drawing(1)), 83);
                assert!(!table_placeholders_are_alone(&root, "{{a}}"));
                let mut doc = root.clone();
                assert_eq!(replace_paragraphs_with_table(&mut doc, "{{a}}", &table), 3);
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
