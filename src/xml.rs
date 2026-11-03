//! Lossless XML tree for package parts.
//!
//! [`XmlDocument::parse_with_budget`] turns a part into a tree that
//! remembers how each node was written: a start tag keeps its attribute
//! text verbatim (order, spacing, quotes and escapes), character data keeps
//! its escapes, an element remembers whether it was written `<a/>` or
//! `<a></a>`, and comments, processing instructions and the XML declaration
//! are stored as written. Serializing an unmodified tree reproduces the
//! input byte for byte, and an edit only changes the bytes of the nodes it
//! touches.
//!
//! Namespaces are resolved once, while parsing, into [`Ns`], so code that
//! walks the tree matches elements by namespace and local name, whatever
//! prefix the producer chose.
//!
//! Parsing, serialization, cloning, `Debug` and [`Element::descendants`]
//! walk the tree with an explicit stack. Everything else that walks it
//! recurses: the derived `Drop` and `PartialEq`, and the replacement
//! engine. Parsing rejects nesting deeper than [`MAX_DEPTH`] elements, and
//! the replacement engine keeps its edits within that depth, which bounds
//! that recursion.
//!
//! Parsing also takes a budget of nodes, shared by every part of a
//! document, which bounds the memory the trees take (see
//! [`NODE_BUDGET`]).
//!
//! The escaping helpers at the end are shared with the snippet generators.

use std::{
    borrow::Cow,
    fmt::{self, Debug},
    ops::Range,
    slice,
};

use quick_xml::{
    NsReader,
    events::{BytesDecl, BytesEnd, BytesStart, Event},
    name::{QName, ResolveResult},
};
use thiserror::Error;

// ── Limits and constants ──────────────────────────────────────────────────────

/// Deepest element nesting the parser accepts, counting the root element
/// as depth 1. The replacement engine keeps the trees it edits within it,
/// so that what it writes can be parsed again.
pub(crate) const MAX_DEPTH: usize = 512;

/// Most namespace declarations (`xmlns` and `xmlns:*` attributes) the
/// parser accepts in scope at once. It is quick-xml's default, set
/// explicitly so that the replacement engine checks its insertions against
/// the same number. Real documents declare a few dozen, on their root.
pub(crate) const MAX_NAMESPACE_BINDINGS: usize = 128;

/// Longest qualified name, of an element or an attribute, the parser
/// accepts, in bytes. The names of Office Open XML are shorter than 64
/// bytes. Without a limit, a huge prefix on a run would be copied into
/// every element a replacement adds to it: a value with many line breaks
/// would make a part gigabytes long. Since a prefix is declared by an
/// attribute named `xmlns:` and the prefix, no prefix in a parsed tree is
/// longer than 250 bytes.
pub(crate) const MAX_NAME_LEN: usize = 256;

/// Number of nodes that the parts parsed for one document may hold
/// together: elements, runs of character data, comments, processing
/// instructions and XML declarations.
///
/// A node takes about 100 to 130 bytes besides its text: its entry among
/// its parent's children, plus the allocations of its tag or text. Without
/// a budget, 64 MiB of the smallest nodes (`x<a/>` over and over) built
/// trees of 2.5 GB. With it, opening a document takes at most about
/// 650 MB, decompressed XML and the trees' text included (600 MB is the
/// most measured), while the XML of a real 64 MiB document has about 2.6
/// million nodes.
pub(crate) const NODE_BUDGET: usize = 4_000_000;

/// U+FEFF, the byte order mark, encoded as UTF-8.
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

/// The XML declaration of new parts, as Word writes it.
const DECLARATION: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#;

// ── Namespaces ────────────────────────────────────────────────────────────────

/// URI of the WordprocessingML main namespace, [`Ns::W`].
pub(crate) const W_URI: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

/// URI of the namespace of DrawingML drawings in WordprocessingML,
/// [`Ns::Wp`].
pub(crate) const WP_URI: &str =
    "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";

/// URI of the markup compatibility namespace, [`Ns::Mc`].
pub(crate) const MC_URI: &str = "http://schemas.openxmlformats.org/markup-compatibility/2006";

/// URI of the package relationships namespace, [`Ns::Rels`].
pub(crate) const RELS_URI: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

/// The namespace of an element, resolved from the namespace declarations
/// in scope where the element was parsed.
///
/// Only the namespaces this crate works with are told apart, in their
/// transitional OOXML form; every other namespace, including the Strict
/// OOXML ones, is [`Other`](Self::Other).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Ns {
    /// No namespace: an unprefixed name with no default namespace in scope.
    None,
    /// WordprocessingML main (`w:`).
    W,
    /// DrawingML drawings in WordprocessingML (`wp:`).
    Wp,
    /// Markup compatibility (`mc:`).
    Mc,
    /// Package relationships, the root namespace of `*.rels` parts.
    Rels,
    /// Any other namespace.
    Other,
}

impl Ns {
    /// The namespaces that have a URI, see [`uri`](Self::uri).
    const KNOWN: [Self; 4] = [Self::W, Self::Wp, Self::Mc, Self::Rels];

    /// Return the namespace URI, or `None` for [`Ns::None`] and
    /// [`Ns::Other`].
    pub(crate) const fn uri(self) -> Option<&'static str> {
        match self {
            Self::W => Some(W_URI),
            Self::Wp => Some(WP_URI),
            Self::Mc => Some(MC_URI),
            Self::Rels => Some(RELS_URI),
            Self::None | Self::Other => None,
        }
    }

    /// Return the namespace named by `uri`, the value of a namespace
    /// declaration as written.
    ///
    /// The value is compared as written, without expanding references:
    /// a URI written with escapes, which Word and LibreOffice never write,
    /// is [`Ns::Other`]. Unescaping would read the whole value again for
    /// every element in its scope, which takes quadratic time when a long
    /// URI is used by many elements.
    fn from_uri(uri: &str) -> Self {
        Self::KNOWN
            .into_iter()
            .find(|ns| ns.uri() == Some(uri))
            .unwrap_or(Self::Other)
    }
}

// ── Tree ──────────────────────────────────────────────────────────────────────

/// A parsed XML part.
///
/// [`to_bytes`](Self::to_bytes) on an unmodified document returns exactly
/// the bytes it was parsed from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct XmlDocument {
    /// Whether the part starts with a UTF-8 byte order mark, which is
    /// written back.
    bom: bool,
    /// The nodes before the root element: the XML declaration, comments,
    /// processing instructions and whitespace ([`Node::Text`]).
    prolog: Vec<Node>,
    /// The root element.
    pub(crate) root: Element,
    /// The nodes after the root element: comments, processing
    /// instructions and whitespace.
    epilog: Vec<Node>,
}

/// A node of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Node {
    /// An element.
    Element(Element),
    /// Character data, raw: escapes are kept as written (`a &amp; b`), so
    /// the string is always valid XML character data. Build one from plain
    /// text with [`escape_text`].
    Text(String),
    /// The content of a CDATA section, without the `<![CDATA[` and `]]>`
    /// delimiters. It must not contain `]]>`.
    CData(String),
    /// Markup kept verbatim, delimiters included: a comment, a processing
    /// instruction or the XML declaration.
    Other(String),
}

impl Node {
    /// Return the element if the node is one.
    pub(crate) const fn as_element(&self) -> Option<&Element> {
        match self {
            Self::Element(element) => Some(element),
            Self::Text(_) | Self::CData(_) | Self::Other(_) => None,
        }
    }

    /// Return the element if the node is one, mutably.
    pub(crate) const fn as_element_mut(&mut self) -> Option<&mut Element> {
        match self {
            Self::Element(element) => Some(element),
            Self::Text(_) | Self::CData(_) | Self::Other(_) => None,
        }
    }
}

/// An element: its start tag, children and end tag.
///
/// The start tag is stored as written, so attributes keep their order,
/// spacing, quotes and escapes; [`attr`](Self::attr) and
/// [`set_attr`](Self::set_attr) read and edit that text in place.
///
/// `Clone` and `Debug` (which prints the namespace and the serialized
/// element) use an explicit stack: a derived, recursive `Clone` needs over
/// 512 KiB of stack for the deepest trees in a debug build.
#[derive(PartialEq, Eq)]
pub(crate) struct Element {
    /// The start tag between `<` and `>` (or `/>`): the qualified name,
    /// then the attributes exactly as written, including whitespace before
    /// the `>`. The name ends at the first whitespace, if any.
    tag: Box<str>,
    /// Namespace of the element name.
    ns: Ns,
    /// Child nodes in document order.
    pub(crate) children: Vec<Node>,
    /// How the element is closed.
    close: Close,
}

/// How an element is closed when it is serialized.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Close {
    /// With an empty-element tag (`<a/>`) while the element has no
    /// children, otherwise with a plain end tag (`</a>`). Elements written
    /// `<a/>` in the source and new elements are closed this way.
    Auto,
    /// Always with an end tag, even without children (`<a></a>`, as
    /// LibreOffice writes empty `w:rPr`). The string is the whitespace
    /// between the name and the `>` of the end tag, usually empty.
    EndTag(Box<str>),
}

impl Clone for Element {
    fn clone(&self) -> Self {
        // `current` is the copy being filled; `stack` holds its unfinished
        // ancestors, each with the source children still to copy.
        let mut stack = Vec::new();
        let mut current = (self.children.iter(), self.clone_shallow());
        loop {
            match current.0.next() {
                Some(Node::Element(child)) => {
                    let copy = (child.children.iter(), child.clone_shallow());
                    stack.push(std::mem::replace(&mut current, copy));
                }
                Some(leaf) => current.1.children.push(leaf.clone()),
                None => match stack.pop() {
                    Some(parent) => {
                        let (_, done) = std::mem::replace(&mut current, parent);
                        current.1.children.push(Node::Element(done));
                    }
                    None => return current.1,
                },
            }
        }
    }
}

impl Debug for Element {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Element")
            .field(&self.ns)
            .field(&self.to_xml())
            .finish()
    }
}

impl XmlDocument {
    /// Create a document holding `root`, after the XML declaration Word
    /// writes (`<?xml version="1.0" encoding="UTF-8" standalone="yes"?>`)
    /// and a CRLF.
    pub(crate) fn new(root: Element) -> Self {
        Self {
            bom: false,
            prolog: vec![
                Node::Other(DECLARATION.to_owned()),
                Node::Text("\r\n".to_owned()),
            ],
            root,
            epilog: Vec::new(),
        }
    }

    /// Parse an XML part with at most `budget` nodes, and deduct the nodes
    /// of the tree from `budget`.
    ///
    /// The input must be UTF-8, optionally with a byte order mark, with no
    /// document type declaration (OPC forbids DTDs, ECMA-376 Part 2,
    /// §8.1.4), no entity references other than the five predefined ones,
    /// at most [`MAX_DEPTH`] levels of nested elements, at most
    /// [`MAX_NAMESPACE_BINDINGS`] namespace declarations in scope at once,
    /// and names of at most [`MAX_NAME_LEN`] bytes. OPC also allows UTF-16
    /// (ECMA-376 Part 2, §8.1.4), which would need transcoding; Word and
    /// LibreOffice write UTF-8.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidXml`] when the input is not well-formed XML 1.0
    /// with namespaces, is not UTF-8 or declares another encoding, contains
    /// a document type declaration, nests elements deeper than
    /// [`MAX_DEPTH`] levels, has more than [`MAX_NAMESPACE_BINDINGS`]
    /// namespace declarations in scope or a name longer than
    /// [`MAX_NAME_LEN`] bytes, or has more nodes than `budget`. `budget` is
    /// then left unchanged.
    pub(crate) fn parse_with_budget(input: &[u8], budget: &mut usize) -> Result<Self, InvalidXml> {
        let (bom, body) = match input.strip_prefix(UTF8_BOM) {
            Some(body) => (true, body),
            None => (false, input),
        };
        let base = if bom { UTF8_BOM.len() } else { 0 };
        let xml = std::str::from_utf8(body).map_err(|err| {
            InvalidXml(Reason::Utf8 {
                offset: base + err.valid_up_to(),
            })
        })?;
        let Tree {
            prolog,
            root,
            epilog,
        } = parse_tree(xml, base, false, budget).map_err(InvalidXml)?;
        Ok(Self {
            bom,
            prolog,
            root,
            epilog,
        })
    }

    /// Parse an XML part with no limit on its number of nodes (see
    /// [`parse_with_budget`](Self::parse_with_budget)).
    #[cfg(test)]
    pub(crate) fn parse(input: &[u8]) -> Result<Self, InvalidXml> {
        let mut unlimited = usize::MAX;
        Self::parse_with_budget(input, &mut unlimited)
    }

    /// Serialize the document.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = String::new();
        if self.bom {
            out.push('\u{FEFF}');
        }
        for node in &self.prolog {
            write_node(&mut out, node);
        }
        write_element(&mut out, &self.root);
        for node in &self.epilog {
            write_node(&mut out, node);
        }
        out.into_bytes()
    }
}

impl Element {
    /// Create an element with no attributes and no children.
    ///
    /// `name` is the qualified name (e.g. `w:t`) and `ns` the namespace its
    /// prefix is bound to where the element will be inserted: no namespace
    /// declaration is added, so the prefix must already be in scope there.
    /// An element without children is written as `<name/>`.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `name` is not a valid qualified name.
    pub(crate) fn new(name: impl Into<String>, ns: Ns) -> Self {
        let tag = name.into().into_boxed_str();
        debug_assert!(is_qname(&tag), "invalid element name {tag:?}");
        Self {
            tag,
            ns,
            children: Vec::new(),
            close: Close::Auto,
        }
    }

    /// Parse `xml`, which must hold exactly one element, such as a
    /// generated snippet. Whitespace around the element is ignored.
    ///
    /// The snippet is parsed on its own, so it must declare every namespace
    /// prefix it uses, normally on its root element; those declarations
    /// stay on the element when it is inserted into a document. Its nodes
    /// count against no budget: snippets are generated by this crate.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidXml`] when `xml` is not one well-formed element
    /// (see [`XmlDocument::parse_with_budget`]), or when anything but
    /// whitespace surrounds it, including an XML declaration or a comment.
    pub(crate) fn parse_fragment(xml: &str) -> Result<Self, InvalidXml> {
        let mut unlimited = usize::MAX;
        parse_tree(xml, 0, true, &mut unlimited)
            .map(|tree| tree.root)
            .map_err(InvalidXml)
    }

    /// Return how many namespace declarations (`xmlns` and `xmlns:*`
    /// attributes) the start tag holds, which add to those in scope for
    /// the element and everything below it.
    pub(crate) fn namespace_declarations(&self) -> usize {
        self.raw_attrs()
            .filter(|attr| attr.name == "xmlns" || attr.name.starts_with("xmlns:"))
            .count()
    }

    /// Return the qualified name, e.g. `w:p`.
    pub(crate) fn name(&self) -> &str {
        self.tag.get(..self.name_len()).unwrap_or_default()
    }

    /// Return the namespace prefix of the name (`w` for `w:p`), or `None`
    /// for an unprefixed name.
    pub(crate) fn prefix(&self) -> Option<&str> {
        self.name().split_once(':').map(|(prefix, _)| prefix)
    }

    /// Return the name without its prefix (`p` for `w:p`).
    pub(crate) fn local_name(&self) -> &str {
        let name = self.name();
        name.split_once(':').map_or(name, |(_, local)| local)
    }

    /// Return the namespace of the element.
    pub(crate) const fn ns(&self) -> Ns {
        self.ns
    }

    /// Whether the element is `local` in namespace `ns`, whatever its
    /// prefix.
    pub(crate) fn is(&self, ns: Ns, local: &str) -> bool {
        self.ns == ns && self.local_name() == local
    }

    /// Iterate over the qualified names of the attributes, as written and
    /// in order, namespace declarations included.
    pub(crate) fn attr_names(&self) -> impl Iterator<Item = &str> {
        self.raw_attrs().map(|attr| attr.name)
    }

    /// Return the value of the attribute named `name`, or `None` if the
    /// element has no such attribute.
    ///
    /// `name` is matched against the qualified name as written, prefix
    /// included (`xml:space`, `Id`, `r:embed`). The value is unescaped and
    /// normalized as an XML processor reports it (XML 1.0 §3.3.3): literal
    /// line breaks and tabs become spaces, while character references such
    /// as `&#10;` keep the character they stand for.
    pub(crate) fn attr(&self, name: &str) -> Option<Cow<'_, str>> {
        self.raw_attrs()
            .find(|attr| attr.name == name)
            .map(|attr| unescape(attr.value, Unescape::Attr))
    }

    /// Set the attribute named `name` to `value`, which is escaped with
    /// [`escape_attr`].
    ///
    /// An existing attribute keeps its position and quotes; a new one is
    /// appended after the last attribute as `name="value"`. A prefixed
    /// `name` must be bound to a namespace where the element ends up.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `name` is not a valid qualified name.
    pub(crate) fn set_attr(&mut self, name: &str, value: &str) {
        debug_assert!(is_qname(name), "invalid attribute name {name:?}");
        let value = escape_attr(value);
        let mut existing = None;
        let mut end = self.name_len();
        for attr in self.raw_attrs() {
            if attr.name == name {
                existing = Some(attr.value_range);
                break;
            }
            // Just after the closing quote.
            end = attr.value_range.end + 1;
        }
        // Both positions come from scanning this very tag, next to ASCII
        // delimiters, so they lie on character boundaries.
        let mut tag = String::from(std::mem::take(&mut self.tag));
        match existing {
            Some(range) => tag.replace_range(range, &value),
            None => tag.insert_str(end, &[" ", name, "=\"", &value, "\""].concat()),
        }
        self.tag = tag.into_boxed_str();
    }

    /// Return the character data of the element's children: their text
    /// and CDATA sections, unescaped and concatenated.
    ///
    /// Child elements are skipped, not descended into. Line breaks are
    /// normalized as an XML processor does (XML 1.0 §2.11): `\r\n` and a
    /// lone `\r` written literally become `\n`, while `&#13;` stays `\r`.
    pub(crate) fn text(&self) -> Cow<'_, str> {
        let mut parts = self.children.iter().filter_map(|node| match node {
            Node::Text(raw) => Some(unescape(raw, Unescape::Text)),
            Node::CData(content) => Some(unescape(content, Unescape::CData)),
            Node::Element(_) | Node::Other(_) => None,
        });
        let Some(mut text) = parts.next() else {
            return Cow::Borrowed("");
        };
        for part in parts {
            text.to_mut().push_str(&part);
        }
        text
    }

    /// Iterate over the child elements, skipping the other nodes.
    pub(crate) fn child_elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(Node::as_element)
    }

    /// Iterate mutably over the child elements, skipping the other nodes.
    pub(crate) fn child_elements_mut(&mut self) -> impl Iterator<Item = &mut Element> {
        self.children.iter_mut().filter_map(Node::as_element_mut)
    }

    /// Return the first child element `local` in namespace `ns`.
    pub(crate) fn find_child(&self, ns: Ns, local: &str) -> Option<&Element> {
        self.child_elements().find(|child| child.is(ns, local))
    }

    /// Iterate over all elements below this one, in document order
    /// (depth-first, parents before their children).
    pub(crate) fn descendants(&self) -> impl Iterator<Item = &Element> {
        let mut stack = vec![self.children.iter()];
        std::iter::from_fn(move || {
            while let Some(children) = stack.last_mut() {
                match children.next() {
                    Some(Node::Element(child)) => {
                        stack.push(child.children.iter());
                        return Some(child);
                    }
                    Some(_) => {}
                    None => {
                        stack.pop();
                    }
                }
            }
            None
        })
    }

    /// Serialize the element and everything below it.
    pub(crate) fn to_xml(&self) -> String {
        let mut out = String::new();
        write_element(&mut out, self);
        out
    }

    /// Copy the element without its children, with room for them.
    fn clone_shallow(&self) -> Self {
        Self {
            tag: self.tag.clone(),
            ns: self.ns,
            children: Vec::with_capacity(self.children.len()),
            close: self.close.clone(),
        }
    }

    /// Return the byte length of the name at the start of the tag.
    fn name_len(&self) -> usize {
        // Like quick-xml, which split the start tag the same way.
        self.tag
            .bytes()
            .position(is_whitespace)
            .unwrap_or(self.tag.len())
    }

    /// Iterate over the attributes of the start tag.
    fn raw_attrs(&self) -> impl Iterator<Item = RawAttr<'_>> {
        // Stored tags passed the same scan at parse time, so it cannot fail
        // here; stopping at an error keeps the accessors panic-free anyway.
        RawAttrs {
            tag: &self.tag,
            pos: self.name_len(),
        }
        .map_while(Result::ok)
    }
}

// ── Parsing ───────────────────────────────────────────────────────────────────

/// The parts of a parsed document.
struct Tree {
    /// See [`XmlDocument::prolog`].
    prolog: Vec<Node>,
    /// See [`XmlDocument::root`].
    root: Element,
    /// See [`XmlDocument::epilog`].
    epilog: Vec<Node>,
}

/// Parse `xml` into a [`Tree`].
///
/// `base` is added to every offset reported in errors: it is the length of
/// the byte order mark the caller stripped. With `fragment`, only
/// whitespace may surround the root element, and it is dropped. The nodes
/// of the tree are deducted from `budget`, which is left unchanged on
/// error.
fn parse_tree(xml: &str, base: usize, fragment: bool, budget: &mut usize) -> Result<Tree, Reason> {
    // quick-xml silently skips a byte order mark at the start of its input,
    // which would drop it from the round trip. Callers strip the one
    // allowed BOM, so another one is content before the root element.
    if xml.starts_with('\u{FEFF}') {
        return Err(Reason::OutsideRoot { offset: base });
    }

    let mut reader = NsReader::from_str(xml);
    let config = reader.config_mut();
    config.trim_text(false);
    config.expand_empty_elements = false;
    // `Builder::end` matches end tags against the open element itself, so
    // that the whitespace quick-xml would trim from `</a >` can be kept.
    config.check_end_names = false;
    config.trim_markup_names_in_closing_tags = false;
    reader
        .resolver_mut()
        .set_max_namespace_bindings(MAX_NAMESPACE_BINDINGS);

    let mut builder = Builder {
        reader,
        base,
        fragment,
        prolog: Vec::new(),
        root: None,
        epilog: Vec::new(),
        open: Vec::new(),
        text: None,
        nodes_left: *budget,
        names: Vec::new(),
    };
    loop {
        let offset = builder.offset(builder.reader.buffer_position());
        let event = match builder.reader.read_event() {
            Ok(event) => event,
            Err(source) => {
                // quick-xml checks the namespace declarations of a start tag
                // once it has read the tag, without moving the position it
                // reports errors at: they belong to the tag at `offset`.
                let offset = match source {
                    quick_xml::Error::Namespace(_) => offset,
                    _ => builder.offset(builder.reader.error_position()),
                };
                return Err(Reason::Syntax { offset, source });
            }
        };
        if !matches!(event, Event::Text(_) | Event::GeneralRef(_)) {
            builder.flush_text()?;
        }
        match event {
            Event::Text(text) => builder.push_text(offset, text.into_inner()),
            Event::GeneralRef(reference) => builder.push_reference(offset, &reference)?,
            Event::Start(start) => {
                let element = builder.start(offset, &start)?;
                builder.open.push(element);
            }
            Event::Empty(start) => {
                let element = builder.start(offset, &start)?;
                builder.attach(element);
            }
            Event::End(end) => builder.end(offset, &end)?,
            Event::CData(cdata) => {
                let content = cdata.into_inner().into_owned();
                builder.inside(offset, Node::CData(content))?;
            }
            Event::Comment(comment) => builder.misc(offset, ["<!--", &*comment, "-->"].concat())?,
            Event::PI(pi) => builder.misc(offset, ["<?", &*pi, "?>"].concat())?,
            Event::Decl(decl) => builder.declaration(offset, &decl)?,
            Event::DocType(_) => return Err(Reason::Doctype { offset }),
            Event::Eof => break,
        }
    }
    let nodes_left = builder.nodes_left;
    let tree = builder.finish()?;
    *budget = nodes_left;
    Ok(tree)
}

/// A tree under construction from quick-xml events, with its open
/// elements on an explicit stack.
struct Builder<'a> {
    /// The event source; it also tracks the namespace declarations in
    /// scope.
    reader: NsReader<&'a [u8]>,
    /// Added to reader positions to get offsets in the caller's input.
    base: usize,
    /// Whether the input is a fragment: nothing but whitespace may surround
    /// the root element.
    fragment: bool,
    /// See [`XmlDocument::prolog`].
    prolog: Vec<Node>,
    /// The root element, once its end tag has been read.
    root: Option<Element>,
    /// See [`XmlDocument::epilog`].
    epilog: Vec<Node>,
    /// Elements whose end tag has not been read yet, outermost first.
    open: Vec<Element>,
    /// Raw character data read since the last markup, with the offset where
    /// it starts. quick-xml splits text at every reference; the pieces are
    /// joined back into a single [`Node::Text`].
    text: Option<(usize, Cow<'a, str>)>,
    /// Nodes the tree may still get.
    nodes_left: usize,
    /// Attribute name ranges of the current start tag: a scratch buffer,
    /// kept to reuse its allocation.
    names: Vec<Range<usize>>,
}

impl<'a> Builder<'a> {
    /// Turn a reader position into an offset in the caller's input.
    fn offset(&self, position: u64) -> usize {
        usize::try_from(position).map_or(usize::MAX, |position| position.saturating_add(self.base))
    }

    /// Charge one node, at `offset`, to the budget.
    fn count_node(&mut self, offset: usize) -> Result<(), Reason> {
        self.nodes_left = self
            .nodes_left
            .checked_sub(1)
            .ok_or(Reason::TooManyNodes { offset })?;
        Ok(())
    }

    /// Record a piece of raw character data.
    fn push_text(&mut self, offset: usize, raw: Cow<'a, str>) {
        match &mut self.text {
            Some((_, pending)) => pending.to_mut().push_str(&raw),
            None => self.text = Some((offset, raw)),
        }
    }

    /// Validate the reference `&name;` and record it as raw character data.
    fn push_reference(&mut self, offset: usize, name: &str) -> Result<(), Reason> {
        if resolve_reference(name).is_none() {
            return Err(Reason::Reference { offset });
        }
        let (_, pending) = self.text.get_or_insert((offset, Cow::Borrowed("")));
        let pending = pending.to_mut();
        pending.push('&');
        pending.push_str(name);
        pending.push(';');
        Ok(())
    }

    /// Move the recorded character data into the tree.
    fn flush_text(&mut self) -> Result<(), Reason> {
        let Some((start, raw)) = self.text.take() else {
            return Ok(());
        };
        if !self.open.is_empty() {
            self.count_node(start)?;
            if let Some(parent) = self.open.last_mut() {
                parent.children.push(Node::Text(raw.into_owned()));
            }
        } else if !raw.bytes().all(is_whitespace) {
            // Only whitespace may surround the root element (XML 1.0 §2.8,
            // production `[27]`).
            return Err(Reason::OutsideRoot { offset: start });
        } else if !self.fragment {
            self.count_node(start)?;
            self.outside(Node::Text(raw.into_owned()));
        }
        Ok(())
    }

    /// Build an element from its start tag at `offset`.
    fn start(&mut self, offset: usize, start: &BytesStart<'_>) -> Result<Element, Reason> {
        if self.open.is_empty() && self.root.is_some() {
            return Err(Reason::MultipleRoots { offset });
        }
        if self.open.len() >= MAX_DEPTH {
            return Err(Reason::TooDeep { offset });
        }
        self.count_node(offset)?;
        let name = start.name();
        if name.into_inner().len() > MAX_NAME_LEN {
            return Err(Reason::LongName { offset });
        }
        // Element names must not use the `xmlns` prefix (Namespaces in XML
        // 1.0 §3).
        if !is_qname(name.into_inner()) || name.prefix().is_some_and(|prefix| prefix.is_xmlns()) {
            return Err(Reason::Name { offset });
        }
        let ns = match self.reader.resolver().resolve_element(name).0 {
            ResolveResult::Bound(uri) => Ns::from_uri(uri.into_inner()),
            ResolveResult::Unbound => Ns::None,
            ResolveResult::Unknown(_) => return Err(Reason::UnknownPrefix { offset }),
        };
        // The name ends at the first whitespace of the tag, which is what
        // `Element::name_len` relies on.
        let tag: &str = start;
        self.check_attributes(offset, tag, name.into_inner().len())?;
        Ok(Element {
            tag: tag.into(),
            ns,
            children: Vec::new(),
            close: Close::Auto,
        })
    }

    /// Validate the attributes of the start tag `tag`, whose name is
    /// `name_len` bytes long.
    ///
    /// quick-xml checks neither the syntax of the attribute list nor the
    /// values, and the accessors of [`Element`] rely on both.
    fn check_attributes(
        &mut self,
        offset: usize,
        tag: &str,
        name_len: usize,
    ) -> Result<(), Reason> {
        self.names.clear();
        let attrs = RawAttrs { tag, pos: name_len };
        for attr in attrs {
            let attr = attr.map_err(|()| Reason::Attribute { offset })?;
            if attr.name.len() > MAX_NAME_LEN {
                return Err(Reason::LongName { offset });
            }
            if !is_qname(attr.name) {
                return Err(Reason::Name { offset });
            }
            check_attr_value(offset, attr.value)?;
            let (resolved, _) = self.reader.resolver().resolve_attribute(QName(attr.name));
            if matches!(resolved, ResolveResult::Unknown(_)) {
                return Err(Reason::UnknownPrefix { offset });
            }
            self.names.push(attr.name_range);
        }
        // Attribute names must be unique (XML 1.0 §3.1, WFC: Unique Att
        // Spec). Readers disagree on which of two equal names wins, and an
        // edit can make the duplicate a well-formedness error of the
        // output; names are compared as written.
        if has_duplicates(tag, &mut self.names) {
            return Err(Reason::Attribute { offset });
        }
        Ok(())
    }

    /// Close the innermost open element with the end tag `end`.
    fn end(&mut self, offset: usize, end: &BytesEnd<'_>) -> Result<(), Reason> {
        // With trimming off, the end tag text is the name followed by any
        // whitespace before the `>`.
        let raw: &str = end;
        let name = raw.trim_end_matches([' ', '\t', '\r', '\n']);
        let Some(mut element) = self.open.pop() else {
            return Err(Reason::MismatchedEndTag { offset });
        };
        if element.name() != name {
            return Err(Reason::MismatchedEndTag { offset });
        }
        element.close = Close::EndTag(raw.get(name.len()..).unwrap_or_default().into());
        // `Vec` allocates room for at least four nodes, which would make
        // the tree several times larger than the input for typical markup
        // (`<w:t>text</w:t>`, `<w:rPr><w:b/></w:rPr>`).
        element.children.shrink_to_fit();
        self.attach(element);
        Ok(())
    }

    /// Add a complete element to its parent, or make it the root element.
    fn attach(&mut self, element: Element) {
        match self.open.last_mut() {
            Some(parent) => parent.children.push(Node::Element(element)),
            // `start` rejects a second root element.
            None => self.root = Some(element),
        }
    }

    /// Add a node that may only appear inside the root element.
    fn inside(&mut self, offset: usize, node: Node) -> Result<(), Reason> {
        if self.open.is_empty() {
            return Err(Reason::OutsideRoot { offset });
        }
        self.count_node(offset)?;
        if let Some(parent) = self.open.last_mut() {
            parent.children.push(node);
        }
        Ok(())
    }

    /// Add a comment or a processing instruction, allowed anywhere but
    /// around the root element of a fragment.
    fn misc(&mut self, offset: usize, markup: String) -> Result<(), Reason> {
        if self.open.is_empty() && self.fragment {
            return Err(Reason::OutsideRoot { offset });
        }
        self.count_node(offset)?;
        match self.open.last_mut() {
            Some(parent) => parent.children.push(Node::Other(markup)),
            None => self.outside(Node::Other(markup)),
        }
        Ok(())
    }

    /// Add a node before or after the root element.
    fn outside(&mut self, node: Node) {
        if self.root.is_none() {
            self.prolog.push(node);
        } else {
            self.epilog.push(node);
        }
    }

    /// Validate the XML declaration and add it to the prolog.
    fn declaration(&mut self, offset: usize, decl: &BytesDecl<'_>) -> Result<(), Reason> {
        // The declaration can only open a document (XML 1.0 §2.8,
        // production `[22]`).
        if offset != self.base || self.fragment {
            return Err(Reason::Declaration { offset });
        }
        // The text between `<?` and `?>`: `xml` and pseudo-attributes.
        let content: &str = decl;
        let attrs = RawAttrs {
            tag: content,
            pos: "xml".len(),
        };
        for attr in attrs {
            let attr = attr.map_err(|()| Reason::Declaration { offset })?;
            if attr.name == "encoding" && !attr.value.eq_ignore_ascii_case("UTF-8") {
                return Err(Reason::Encoding);
            }
        }
        self.count_node(offset)?;
        self.prolog
            .push(Node::Other(["<?", content, "?>"].concat()));
        Ok(())
    }

    /// Check that the input ended outside of any element and return the
    /// tree.
    fn finish(self) -> Result<Tree, Reason> {
        if !self.open.is_empty() {
            let offset = self.offset(self.reader.buffer_position());
            return Err(Reason::Unclosed { offset });
        }
        let root = self.root.ok_or(Reason::NoRoot)?;
        Ok(Tree {
            prolog: self.prolog,
            root,
            epilog: self.epilog,
        })
    }
}

/// Check an attribute value at `offset`: it may not contain `<` (XML 1.0
/// §3.1, WFC: No < in Attribute Values), and every reference must be one
/// [`resolve_reference`] accepts.
fn check_attr_value(offset: usize, value: &str) -> Result<(), Reason> {
    if value.contains('<') {
        return Err(Reason::Attribute { offset });
    }
    let mut rest = value;
    while let Some((_, after)) = rest.split_once('&') {
        let Some((name, tail)) = after.split_once(';') else {
            return Err(Reason::Reference { offset });
        };
        if resolve_reference(name).is_none() {
            return Err(Reason::Reference { offset });
        }
        rest = tail;
    }
    Ok(())
}

/// Whether two of the attribute names at `names` (ranges in `tag`) are
/// equal.
///
/// A pairwise scan beats sorting for the few attributes most elements
/// have; for many, `names` is sorted in place, which keeps hostile tags
/// O(n log n) without allocating.
fn has_duplicates(tag: &str, names: &mut [Range<usize>]) -> bool {
    let name = |range: &Range<usize>| tag.get(range.clone());
    if names.len() <= 16 {
        return names
            .iter()
            .enumerate()
            .any(|(i, a)| names.iter().take(i).any(|b| name(a) == name(b)));
    }
    names.sort_unstable_by(|a, b| name(a).cmp(&name(b)));
    names
        .windows(2)
        .any(|pair| matches!(pair, [a, b] if name(a) == name(b)))
}

/// Whether `name` is a qualified name (Namespaces in XML 1.0 §4): an XML
/// name with at most one colon, which neither starts nor ends it.
///
/// ASCII characters are checked against the name productions of XML 1.0
/// §2.3; other characters are accepted without checking their ranges.
fn is_qname(name: &str) -> bool {
    let mut parts = name.split(':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(local), None, _) => is_ncname(local),
        (Some(prefix), Some(local), None) => is_ncname(prefix) && is_ncname(local),
        _ => false,
    }
}

/// Whether `name` is an XML name without colons (see [`is_qname`]).
fn is_ncname(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_' || !first.is_ascii())
        && bytes
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.') || !b.is_ascii())
}

/// Whether `b` is XML whitespace (XML 1.0 §2.3, production `[3]`).
const fn is_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

// ── Attributes ────────────────────────────────────────────────────────────────

/// One attribute of a start tag, borrowed from the tag text.
struct RawAttr<'a> {
    /// The qualified name, e.g. `w:val`.
    name: &'a str,
    /// Byte range of `name` in the tag.
    name_range: Range<usize>,
    /// The value between the quotes, escapes included.
    value: &'a str,
    /// Byte range of `value` in the tag.
    value_range: Range<usize>,
}

/// Iterator over the attributes of a start tag: whitespace, a name, `=`
/// with optional whitespace around it, and a quoted value, repeated (XML
/// 1.0 §3.1, productions `[40]` and `[41]`).
///
/// Yields `Err(())` once, then stops, at the first malformed attribute.
/// Trailing whitespace is allowed.
struct RawAttrs<'a> {
    /// The start tag text.
    tag: &'a str,
    /// Where the next attribute (or its leading whitespace) starts: just
    /// after the element name at first, `usize::MAX` once the iterator is
    /// done.
    pos: usize,
}

impl<'a> Iterator for RawAttrs<'a> {
    type Item = Result<RawAttr<'a>, ()>;

    fn next(&mut self) -> Option<Self::Item> {
        let bytes = self.tag.as_bytes();
        if self.pos >= bytes.len() {
            return None;
        }
        let name_start = skip_whitespace(bytes, self.pos);
        if name_start >= bytes.len() {
            self.pos = usize::MAX;
            return None;
        }
        // Attributes are separated from the name and from each other by
        // whitespace.
        let attr = if name_start > self.pos {
            scan_attr(self.tag, name_start)
        } else {
            None
        };
        self.pos = attr
            .as_ref()
            .map_or(usize::MAX, |attr| attr.value_range.end + 1);
        Some(attr.ok_or(()))
    }
}

/// Scan one `name = "value"` attribute starting at `start`.
fn scan_attr(tag: &str, start: usize) -> Option<RawAttr<'_>> {
    let bytes = tag.as_bytes();
    let name_len = bytes
        .get(start..)?
        .iter()
        .position(|&b| b == b'=' || is_whitespace(b))?;
    let name_end = start + name_len;
    let eq = skip_whitespace(bytes, name_end);
    if name_len == 0 || bytes.get(eq) != Some(&b'=') {
        return None;
    }
    let quote_pos = skip_whitespace(bytes, eq + 1);
    let quote = *bytes.get(quote_pos).filter(|&&b| b == b'"' || b == b'\'')?;
    let value_start = quote_pos + 1;
    let value_len = bytes.get(value_start..)?.iter().position(|&b| b == quote)?;
    let value_end = value_start + value_len;
    Some(RawAttr {
        name: tag.get(start..name_end)?,
        name_range: start..name_end,
        value: tag.get(value_start..value_end)?,
        value_range: value_start..value_end,
    })
}

/// Return the position of the first non-whitespace byte at or after `pos`.
fn skip_whitespace(bytes: &[u8], pos: usize) -> usize {
    bytes.get(pos..).map_or(pos, |rest| {
        pos + rest.iter().take_while(|&&b| is_whitespace(b)).count()
    })
}

// ── References ────────────────────────────────────────────────────────────────

/// Return the character the reference `&name;` stands for.
///
/// Only the five predefined entities (XML 1.0 §4.6) and character
/// references to characters XML allows (§4.1, WFC: Legal Character) are
/// known: anything else, including custom entities, gives `None`.
fn resolve_reference(name: &str) -> Option<char> {
    match name {
        "lt" => Some('<'),
        "gt" => Some('>'),
        "amp" => Some('&'),
        "apos" => Some('\''),
        "quot" => Some('"'),
        _ => {
            let number = name.strip_prefix('#')?;
            let (digits, radix) = match number.strip_prefix('x') {
                Some(hex) => (hex, 16),
                None => (number, 10),
            };
            if digits.is_empty() {
                return None;
            }
            let code = digits.chars().try_fold(0_u32, |code, c| {
                code.checked_mul(radix)?.checked_add(c.to_digit(radix)?)
            })?;
            char::from_u32(code).filter(|&c| is_xml_char(c))
        }
    }
}

/// What [`unescape`] does besides expanding references.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unescape {
    /// Character data: line breaks are normalized to `\n` (XML 1.0 §2.11)
    /// and references expanded.
    Text,
    /// CDATA content: line breaks are normalized, references are plain
    /// text.
    CData,
    /// Attribute values: literal tabs and line breaks (`\r\n` counting as
    /// one) become spaces (XML 1.0 §3.3.3), and references are expanded.
    Attr,
}

/// Unescape `raw` as an XML processor reports it, see [`Unescape`].
///
/// Parsed trees only contain references [`resolve_reference`] accepts; any
/// other `&` is kept as is.
fn unescape(raw: &str, mode: Unescape) -> Cow<'_, str> {
    let special = |b: u8| match mode {
        Unescape::Text => b == b'&' || b == b'\r',
        Unescape::CData => b == b'\r',
        Unescape::Attr => matches!(b, b'&' | b'\r' | b'\n' | b'\t'),
    };
    if !raw.bytes().any(special) {
        return Cow::Borrowed(raw);
    }
    let line_break = if mode == Unescape::Attr { ' ' } else { '\n' };
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    // Every special byte is ASCII, so each split lands on a char boundary.
    while let Some(at) = rest.bytes().position(special) {
        let (plain, tail) = rest.split_at_checked(at).unwrap_or((rest, ""));
        out.push_str(plain);
        rest = if let Some(after) = tail.strip_prefix('&') {
            let resolved = after
                .split_once(';')
                .and_then(|(name, tail)| Some((resolve_reference(name)?, tail)));
            if let Some((c, tail)) = resolved {
                out.push(c);
                tail
            } else {
                out.push('&');
                after
            }
        } else if let Some(after) = tail
            .strip_prefix("\r\n")
            .or_else(|| tail.strip_prefix('\r'))
        {
            out.push(line_break);
            after
        } else if let Some(after) = tail.strip_prefix(['\n', '\t']) {
            out.push(' ');
            after
        } else {
            // Unreachable: `special` only matches the bytes handled above.
            out.push_str(tail);
            ""
        };
    }
    out.push_str(rest);
    Cow::Owned(out)
}

// ── Serialization ─────────────────────────────────────────────────────────────

/// Append `node` to `out`.
fn write_node(out: &mut String, node: &Node) {
    match node {
        Node::Element(element) => write_element(out, element),
        Node::Text(raw) | Node::Other(raw) => out.push_str(raw),
        Node::CData(content) => {
            out.push_str("<![CDATA[");
            out.push_str(content);
            out.push_str("]]>");
        }
    }
}

/// Append `element` and everything below it to `out`.
fn write_element(out: &mut String, element: &Element) {
    // An explicit stack rather than recursion, as in `Clone` and the
    // parser, so that the thread's stack doesn't need room for every level
    // of the tree (see the module docs).
    let mut stack: Vec<(&Element, slice::Iter<'_, Node>)> = Vec::new();
    if write_start_tag(out, element) {
        stack.push((element, element.children.iter()));
    }
    while let Some((parent, children)) = stack.last_mut() {
        let parent = *parent;
        match children.next() {
            Some(Node::Element(child)) => {
                if write_start_tag(out, child) {
                    stack.push((child, child.children.iter()));
                }
            }
            Some(node) => write_node(out, node),
            None => {
                write_end_tag(out, parent);
                stack.pop();
            }
        }
    }
}

/// Append the start tag of `element` to `out`, or its empty-element tag if
/// that is how it closes; return whether children and an end tag must
/// follow.
fn write_start_tag(out: &mut String, element: &Element) -> bool {
    out.push('<');
    out.push_str(&element.tag);
    if element.children.is_empty() && element.close == Close::Auto {
        out.push_str("/>");
        false
    } else {
        out.push('>');
        true
    }
}

/// Append the end tag of `element` to `out`.
fn write_end_tag(out: &mut String, element: &Element) {
    out.push_str("</");
    out.push_str(element.name());
    if let Close::EndTag(space) = &element.close {
        out.push_str(space);
    }
    out.push('>');
}

// ── Escaping ──────────────────────────────────────────────────────────────────

/// Return the qualified name `{prefix}:{local}`, or `local` when there is no
/// prefix: the name a new element or attribute needs to land in the same
/// namespace as one spelled with `prefix`.
pub(crate) fn qualified_name(prefix: Option<&str>, local: &str) -> String {
    match prefix {
        Some(prefix) => format!("{prefix}:{local}"),
        None => local.to_owned(),
    }
}

/// Escape `s` for use as XML character data.
///
/// Characters that XML 1.0 does not allow anywhere (most C0 controls,
/// U+FFFE and U+FFFF) are dropped, since no escape can represent them.
pub(crate) fn escape_text(s: &str) -> Cow<'_, str> {
    escape(s, false)
}

/// Escape `s` for use inside an attribute value, quoted with either `"` or
/// `'`.
///
/// Like [`escape_text`], but also escapes both quotes and the whitespace
/// characters that attribute-value normalization would otherwise turn
/// into spaces.
pub(crate) fn escape_attr(s: &str) -> Cow<'_, str> {
    escape(s, true)
}

/// Escape `s` for character data, or for an attribute value with `attr`.
fn escape(s: &str, attr: bool) -> Cow<'_, str> {
    // What a character is written as when it can't be written as is: its
    // escape, or nothing for a character XML doesn't allow anywhere.
    let replacement = |c: char| -> Option<&'static str> {
        match c {
            '&' => Some("&amp;"),
            '<' => Some("&lt;"),
            '>' => Some("&gt;"),
            '\r' => Some("&#13;"),
            '"' if attr => Some("&quot;"),
            '\'' if attr => Some("&apos;"),
            '\t' if attr => Some("&#9;"),
            '\n' if attr => Some("&#10;"),
            c if is_xml_char(c) => None,
            _ => Some(""),
        }
    };
    let Some(start) = s.find(|c| replacement(c).is_some()) else {
        return Cow::Borrowed(s);
    };

    // `find` returns the position of a character, so this can't panic.
    let (plain, rest) = s.split_at(start);
    let mut out = String::with_capacity(s.len() + 16);
    out.push_str(plain);
    for c in rest.chars() {
        match replacement(c) {
            Some(escaped) => out.push_str(escaped),
            None => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Whether `c` matches the XML 1.0 `Char` production.
fn is_xml_char(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\r'
            | '\u{20}'..='\u{D7FF}'
            | '\u{E000}'..='\u{FFFD}'
            | '\u{10000}'..='\u{10FFFF}'
    )
}

// ── Errors ────────────────────────────────────────────────────────────────────

/// Error returned when an XML part is malformed, uses something this crate
/// does not support (an encoding other than UTF-8 or a document type
/// declaration), or exceeds a limit: elements nested deeper than 512
/// levels, more than 128 namespace declarations in scope, a name longer
/// than 256 bytes, or more nodes than the parts of a document may hold
/// together.
///
/// Its message says what is wrong and where.
#[derive(Debug, Error)]
#[error(transparent)]
#[non_exhaustive]
pub struct InvalidXml(Reason);

/// Why an XML part was rejected. Offsets count bytes from the start of the
/// input, byte order mark included.
#[derive(Debug, Error)]
enum Reason {
    /// The input is not valid UTF-8, which includes UTF-16.
    #[error("invalid UTF-8 at byte {offset}")]
    Utf8 {
        /// Where the invalid sequence starts.
        offset: usize,
    },
    /// The XML declaration names an encoding other than UTF-8.
    #[error("XML declaration names an encoding other than UTF-8")]
    Encoding,
    /// quick-xml rejected the markup.
    #[error("malformed XML at byte {offset}")]
    Syntax {
        /// Where the malformed markup starts.
        offset: usize,
        /// What quick-xml found.
        #[source]
        source: quick_xml::Error,
    },
    /// The XML declaration is malformed, or not at the very start of a
    /// document.
    #[error("invalid XML declaration at byte {offset}")]
    Declaration {
        /// Where the declaration starts.
        offset: usize,
    },
    /// A document type declaration, which OPC forbids (ECMA-376 Part 2,
    /// §8.1.4).
    #[error("document type declaration at byte {offset}")]
    Doctype {
        /// Where the declaration starts.
        offset: usize,
    },
    /// An element or attribute name is not a qualified name, or an element
    /// name uses the reserved `xmlns` prefix.
    #[error("invalid name in the tag at byte {offset}")]
    Name {
        /// Where the tag starts.
        offset: usize,
    },
    /// A start tag has a malformed or duplicate attribute, or an attribute
    /// value contains `<`.
    #[error("invalid attribute in the tag at byte {offset}")]
    Attribute {
        /// Where the tag starts.
        offset: usize,
    },
    /// A reference is neither a predefined entity nor a character reference
    /// to a character XML allows.
    #[error("unknown entity or invalid character reference at byte {offset}")]
    Reference {
        /// Where the reference, or the tag holding it, starts.
        offset: usize,
    },
    /// A prefix that is not bound to a namespace.
    #[error("unknown namespace prefix in the tag at byte {offset}")]
    UnknownPrefix {
        /// Where the tag starts.
        offset: usize,
    },
    /// Elements are nested deeper than [`MAX_DEPTH`] levels.
    #[error("elements nested deeper than {max} levels at byte {offset}", max = MAX_DEPTH)]
    TooDeep {
        /// Where the first element too deep starts.
        offset: usize,
    },
    /// An element or attribute name is longer than [`MAX_NAME_LEN`] bytes.
    #[error("name longer than {max} bytes in the tag at byte {offset}", max = MAX_NAME_LEN)]
    LongName {
        /// Where the tag starts.
        offset: usize,
    },
    /// The part has more nodes than the budget it was parsed with allows
    /// (see [`NODE_BUDGET`]).
    #[error("too many XML nodes at byte {offset}")]
    TooManyNodes {
        /// Where the first node over the budget starts.
        offset: usize,
    },
    /// An end tag does not match the innermost open element.
    #[error("mismatched end tag at byte {offset}")]
    MismatchedEndTag {
        /// Where the end tag starts.
        offset: usize,
    },
    /// The input ends inside an element.
    #[error("unclosed element at the end of the input (byte {offset})")]
    Unclosed {
        /// The length of the input.
        offset: usize,
    },
    /// A second root element.
    #[error("second root element at byte {offset}")]
    MultipleRoots {
        /// Where the second root element starts.
        offset: usize,
    },
    /// The input has no root element.
    #[error("no root element")]
    NoRoot,
    /// Content outside the root element other than whitespace, comments
    /// and processing instructions, or other than whitespace in a
    /// fragment.
    #[error("content outside the root element at byte {offset}")]
    OutsideRoot {
        /// Where the content starts.
        offset: usize,
    },
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// A Word-2019-style `document.xml`: the declaration line with CRLF,
    /// the usual root namespace declarations and `mc:Ignorable`, rsid
    /// attributes, `xml:space`, entity and character references in text and
    /// attributes, a comment, CDATA, empty-element tags and a single-quoted
    /// value containing `"`.
    const WORD_DOCUMENT: &str = concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        "\r\n",
        "<w:document",
        r#" xmlns:wpc="http://schemas.microsoft.com/office/word/2010/wordprocessingCanvas""#,
        r#" xmlns:cx="http://schemas.microsoft.com/office/drawing/2014/chartex""#,
        r#" xmlns:cx1="http://schemas.microsoft.com/office/drawing/2015/9/8/chartex""#,
        r#" xmlns:cx2="http://schemas.microsoft.com/office/drawing/2015/10/21/chartex""#,
        r#" xmlns:cx3="http://schemas.microsoft.com/office/drawing/2016/5/9/chartex""#,
        r#" xmlns:cx4="http://schemas.microsoft.com/office/drawing/2016/5/10/chartex""#,
        r#" xmlns:cx5="http://schemas.microsoft.com/office/drawing/2016/5/11/chartex""#,
        r#" xmlns:cx6="http://schemas.microsoft.com/office/drawing/2016/5/12/chartex""#,
        r#" xmlns:cx7="http://schemas.microsoft.com/office/drawing/2016/5/13/chartex""#,
        r#" xmlns:cx8="http://schemas.microsoft.com/office/drawing/2016/5/14/chartex""#,
        r#" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006""#,
        r#" xmlns:aink="http://schemas.microsoft.com/office/drawing/2016/ink""#,
        r#" xmlns:am3d="http://schemas.microsoft.com/office/drawing/2017/model3d""#,
        r#" xmlns:o="urn:schemas-microsoft-com:office:office""#,
        r#" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#,
        r#" xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math""#,
        r#" xmlns:v="urn:schemas-microsoft-com:vml""#,
        r#" xmlns:wp14="http://schemas.microsoft.com/office/word/2010/wordprocessingDrawing""#,
        r#" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing""#,
        r#" xmlns:w10="urn:schemas-microsoft-com:office:word""#,
        r#" xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#,
        r#" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml""#,
        r#" xmlns:w15="http://schemas.microsoft.com/office/word/2012/wordml""#,
        r#" xmlns:w16cid="http://schemas.microsoft.com/office/word/2016/wordml/cid""#,
        r#" xmlns:w16se="http://schemas.microsoft.com/office/word/2015/wordml/symex""#,
        r#" xmlns:wpg="http://schemas.microsoft.com/office/word/2010/wordprocessingGroup""#,
        r#" xmlns:wpi="http://schemas.microsoft.com/office/word/2010/wordprocessingInk""#,
        r#" xmlns:wne="http://schemas.microsoft.com/office/word/2006/wordml""#,
        r#" xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape""#,
        r#" mc:Ignorable="w14 w15 w16se wp14">"#,
        "<w:body><!-- Template for {{customer}} -->",
        r#"<w:p w14:paraId="0A1B2C3D" w14:textId="77777777" w:rsidR="00B57C6E""#,
        r#" w:rsidRDefault="00B57C6E" w:rsidP="00B57C6E">"#,
        r#"<w:pPr><w:pStyle w:val="Title"/><w:jc w:val="center"/></w:pPr>"#,
        r#"<w:r><w:rPr><w:b/><w:bCs/></w:rPr><w:t>Invoice {{</w:t></w:r>"#,
        r#"<w:proofErr w:type="spellStart"/>"#,
        r#"<w:r w:rsidRPr="00A13F2E"><w:rPr><w:b/></w:rPr><w:t>number</w:t></w:r>"#,
        r#"<w:proofErr w:type="spellEnd"/>"#,
        r#"<w:r><w:rPr><w:b/></w:rPr><w:t>}}</w:t></w:r></w:p>"#,
        r#"<w:p w:rsidR="00B57C6E" w:rsidRDefault="00B57C6E">"#,
        r#"<w:r><w:t xml:space="preserve">Fish &amp; chips &lt;3 </w:t></w:r>"#,
        r#"<w:r><w:t xml:space="preserve"> </w:t></w:r>"#,
        r#"<w:bookmarkStart w:id="0" w:name="_GoBack"/><w:bookmarkEnd w:id="0"/>"#,
        r#"<w:hyperlink r:id="rId5" w:history="1"><w:r><w:rPr>"#,
        r#"<w:rStyle w:val="Hyperlink"/></w:rPr><w:t>our site</w:t></w:r></w:hyperlink>"#,
        r#"<w:r><w:t>two&#10;lines</w:t></w:r>"#,
        "<w:r><w:t><![CDATA[a < b && c]]></w:t></w:r>",
        r#"<w:r><w:tab/><w:t>tabbed</w:t></w:r></w:p>"#,
        r#"<w:sdt><w:sdtPr><w:alias w:val="Say &quot;hi&quot;&#10;twice"/>"#,
        r#"<w:tag w:val='a "quoted" tag'/><w:id w:val="-1234567"/></w:sdtPr>"#,
        r#"<w:sdtContent><w:p><w:r><w:t>{{field}}</w:t></w:r></w:p></w:sdtContent></w:sdt>"#,
        r#"<w:p><w:r><mc:AlternateContent><mc:Choice Requires="wps"><w:t>new</w:t>"#,
        r#"</mc:Choice><mc:Fallback><w:t>old</w:t></mc:Fallback></mc:AlternateContent>"#,
        "</w:r></w:p>",
        r#"<w:sectPr w:rsidR="00B57C6E"><w:pgSz w:w="11906" w:h="16838"/>"#,
        r#"<w:pgMar w:top="1417" w:right="1134" w:bottom="1134" w:left="1134""#,
        r#" w:header="708" w:footer="708" w:gutter="0"/><w:cols w:space="708"/>"#,
        r#"<w:docGrid w:linePitch="360"/></w:sectPr></w:body></w:document>"#,
    );

    /// A hand-edited, pretty-printed document: whitespace between elements,
    /// a start tag spread over several lines, whitespace before `/>` and
    /// `>`, and an end tag with whitespace before its `>`. Small, with
    /// references and CDATA, for the byte-flip test.
    const PRETTY_DOCUMENT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- edited by hand -->
<w:document
    xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
    xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <w:body>
    <w:p>
      <w:pPr><w:pStyle w:val="Say &quot;hi&quot;"/></w:pPr>
      <w:r>
        <w:t>Hello {{name}} &amp; &#x263A;<![CDATA[ <ok> ]]></w:t>
      </w:r>
      <w:r >
        <w:br />
        <w:t xml:space = 'preserve' > tail </w:t >
      </w:r>
    </w:p>
    <?app-hint keep me?>
  </w:body>
</w:document>
"#;

    /// Real LibreOffice 25.2 output, converted from an HTML page with a
    /// heading, lists, a table and a link: `word/document.xml`.
    const LO_DOCUMENT: &[u8] = include_bytes!("testdata/libreoffice/document.xml");
    /// LibreOffice `word/document.xml` with a picture, whose DrawingML
    /// elements redeclare namespaces locally.
    const LO_DOCUMENT_IMAGE: &[u8] = include_bytes!("testdata/libreoffice/document_image.xml");
    /// LibreOffice `word/styles.xml`.
    const LO_STYLES: &[u8] = include_bytes!("testdata/libreoffice/styles.xml");
    /// LibreOffice `[Content_Types].xml`.
    const LO_CONTENT_TYPES: &[u8] = include_bytes!("testdata/libreoffice/content_types.xml");

    /// The WordprocessingML namespace URI, for building test inputs.
    const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

    /// Parse `xml`, check that serializing the tree gives back the same
    /// bytes, and return the tree.
    fn round_trip(xml: &[u8]) -> XmlDocument {
        let doc = XmlDocument::parse(xml).unwrap_or_else(|err| {
            panic!("{err} ({:?}): {}", err.0, String::from_utf8_lossy(xml));
        });
        let out = doc.to_bytes();
        assert!(
            out == xml,
            "round trip changed the document:\n{}\n---\n{}",
            String::from_utf8_lossy(xml),
            String::from_utf8_lossy(&out)
        );
        doc
    }

    /// Parse `xml` as a document and return why it was rejected.
    fn reason(xml: impl AsRef<[u8]>) -> Reason {
        let xml = xml.as_ref();
        match XmlDocument::parse(xml) {
            Ok(_) => panic!("accepted {:?}", String::from_utf8_lossy(xml)),
            Err(InvalidXml(reason)) => reason,
        }
    }

    /// Parse `xml` as a fragment and return why it was rejected.
    fn fragment_reason(xml: &str) -> Reason {
        match Element::parse_fragment(xml) {
            Ok(_) => panic!("accepted {xml:?}"),
            Err(InvalidXml(reason)) => reason,
        }
    }

    /// Parse a fragment that must be valid.
    fn fragment(xml: &str) -> Element {
        Element::parse_fragment(xml).unwrap_or_else(|err| panic!("{err} ({:?}): {xml}", err.0))
    }

    /// Return the first element below `root` that is `local` in `ns`.
    fn descendant<'a>(root: &'a Element, ns: Ns, local: &str) -> &'a Element {
        root.descendants()
            .find(|e| e.is(ns, local))
            .unwrap_or_else(|| panic!("no {local} in {root:?}"))
    }

    /// Return the first element at or below `element` matching `pred`.
    fn find_mut<'a>(
        element: &'a mut Element,
        pred: &impl Fn(&Element) -> bool,
    ) -> Option<&'a mut Element> {
        if pred(element) {
            return Some(element);
        }
        element
            .child_elements_mut()
            .find_map(|child| find_mut(child, pred))
    }

    /// Elements `<a>` nested `depth` levels deep.
    fn nested(depth: usize) -> String {
        "<a>".repeat(depth) + &"</a>".repeat(depth)
    }

    /// Check that parsing never panics and that every accepted input round
    /// trips.
    fn parse_without_panicking(input: &[u8]) {
        if let Ok(doc) = XmlDocument::parse(input) {
            assert!(
                doc.to_bytes() == input,
                "{:?}",
                String::from_utf8_lossy(input)
            );
        }
    }

    // ── Round trips ───────────────────────────────────────────────────────────

    #[test]
    fn word_document_round_trips_byte_for_byte() {
        let doc = round_trip(WORD_DOCUMENT.as_bytes());
        assert!(!doc.bom);
        assert!(doc.root.is(Ns::W, "document"));
        assert_eq!(doc.prolog.len(), 2, "declaration and CRLF");
        assert_eq!(doc.epilog, [] as [Node; 0]);
        let body = doc.root.find_child(Ns::W, "body").unwrap();
        assert!(matches!(body.children.first(), Some(Node::Other(c)) if c.starts_with("<!--")));
        let texts: Vec<_> = doc
            .root
            .descendants()
            .filter(|e| e.is(Ns::W, "t"))
            .map(Element::text)
            .collect();
        assert_eq!(
            texts,
            [
                "Invoice {{",
                "number",
                "}}",
                "Fish & chips <3 ",
                " ",
                "our site",
                "two\nlines",
                "a < b && c",
                "tabbed",
                "{{field}}",
                "new",
                "old",
            ]
        );
        let alias = descendant(&doc.root, Ns::W, "alias");
        assert_eq!(alias.attr("w:val").unwrap(), "Say \"hi\"\ntwice");
        let tag = descendant(&doc.root, Ns::W, "tag");
        assert_eq!(tag.attr("w:val").unwrap(), "a \"quoted\" tag");
    }

    #[test]
    fn byte_order_mark_is_preserved() {
        let mut xml = UTF8_BOM.to_vec();
        xml.extend_from_slice(WORD_DOCUMENT.as_bytes());
        let doc = round_trip(&xml);
        assert!(doc.bom);
        assert!(doc.root.is(Ns::W, "document"));
    }

    #[test]
    fn pretty_printed_document_round_trips() {
        let doc = round_trip(PRETTY_DOCUMENT.as_bytes());
        assert_eq!(doc.epilog, [Node::Text("\n".to_owned())]);
        let style = descendant(&doc.root, Ns::W, "pStyle");
        assert_eq!(style.attr("w:val").unwrap(), "Say \"hi\"");
        let texts: Vec<_> = doc
            .root
            .descendants()
            .filter(|e| e.is(Ns::W, "t"))
            .collect();
        assert_eq!(texts[0].text(), "Hello {{name}} & \u{263A} <ok> ");
        assert_eq!(texts[1].text(), " tail ");
        assert_eq!(texts[1].attr("xml:space").unwrap(), "preserve");
        assert_eq!(texts[1].name(), "w:t");

        // The same document with CRLF line breaks.
        round_trip(PRETTY_DOCUMENT.replace('\n', "\r\n").as_bytes());
    }

    #[test]
    fn libreoffice_parts_round_trip() {
        let doc = round_trip(LO_DOCUMENT);
        assert!(doc.root.is(Ns::W, "document"));
        let table = descendant(&doc.root, Ns::W, "tbl");
        assert_eq!(table.descendants().filter(|e| e.is(Ns::W, "tc")).count(), 4);
        let link = descendant(&doc.root, Ns::W, "hyperlink");
        assert_eq!(link.attr("r:id").unwrap(), "rId2");

        let doc = round_trip(LO_DOCUMENT_IMAGE);
        let graphic = descendant(&doc.root, Ns::Other, "graphic");
        assert!(graphic.find_child(Ns::Other, "graphicData").is_some());
        let pic = descendant(&doc.root, Ns::Other, "pic");
        assert_eq!(pic.prefix(), Some("pic"));
        let doc_pr = descendant(&doc.root, Ns::Wp, "docPr");
        assert_eq!(doc_pr.attr("descr").unwrap(), "Red \"logo\"");

        let doc = round_trip(LO_STYLES);
        assert!(doc.root.is(Ns::W, "styles"));

        let doc = round_trip(LO_CONTENT_TYPES);
        assert!(doc.root.is(Ns::Other, "Types"));
        assert!(doc.root.child_elements().all(|e| e.ns() == Ns::Other));
        let png = doc
            .root
            .child_elements()
            .find(|e| e.attr("Extension").as_deref() == Some("png"))
            .unwrap();
        assert_eq!(png.attr("ContentType").unwrap(), "image/png");
    }

    #[test]
    fn unusual_but_well_formed_markup_round_trips() {
        for xml in [
            "<a/>",
            "<a />",
            "<a></a>",
            "<a>\n</a>",
            "<a\n\tb = '1'\r\n  c=\"2\"  ></a  >",
            "<a b='&apos;\"' c=\"'&#x22;\"/>",
            "<a>]]&gt; &#x1F600; \u{1F600} \u{FEFF}</a>",
            "<a><![CDATA[]]><![CDATA[<&>]]></a>",
            "<?xml version='1.0'?><a/>",
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!-- c -->\n<?pi data?>\n\
             <a/>\n<!-- x -->\n",
            "<a><!----><?pi?><b/>text<c></c></a>",
            "<a xmlns=\"urn:x\" xmlns:b=\"urn:y\" b:c=\"1\"><b:d/></a>",
            "<a xml:lang=\"en\" xml:space=\"default\"/>",
            "<a\u{e9}\u{4e2d}:b xmlns:a\u{e9}\u{4e2d}=\"urn:x\"/>",
            "\r\n\t <a/> \r\n",
        ] {
            round_trip(xml.as_bytes());
        }
    }

    #[test]
    fn edits_change_only_the_edited_bytes() {
        let mut doc = XmlDocument::parse(LO_DOCUMENT).unwrap();
        find_mut(&mut doc.root, &|e| {
            e.is(Ns::W, "t") && e.text() == "{{customer}}"
        })
        .unwrap()
        .children = vec![Node::Text(escape_text("ACME & Co").into_owned())];
        let expected = String::from_utf8(LO_DOCUMENT.to_vec())
            .unwrap()
            .replace("{{customer}}", "ACME &amp; Co");
        assert_eq!(String::from_utf8(doc.to_bytes()).unwrap(), expected);
    }

    // ── Namespaces ────────────────────────────────────────────────────────────

    #[test]
    fn namespaces_resolve_through_scopes() {
        let xml = r#"<Relationships
    xmlns="http://schemas.openxmlformats.org/package/2006/relationships"
    xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
    xmlns:R="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <Relationship Id="rId1" R:id="x"/>
  <w:p><child/></w:p>
  <inner xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default/></inner>
  <w:p xmlns:w="urn:not-word"><w:r/></w:p>
  <reset xmlns=""><plain/></reset>
  <after/><w:p/>
</Relationships>"#;
        let doc = round_trip(xml.as_bytes());
        let ns: Vec<_> = std::iter::once(&doc.root)
            .chain(doc.root.descendants())
            .map(|e| (e.name(), e.ns()))
            .collect();
        assert_eq!(
            ns,
            [
                ("Relationships", Ns::Rels),
                ("Relationship", Ns::Rels),
                ("w:p", Ns::W),
                ("child", Ns::Rels),
                ("inner", Ns::Other),
                ("Default", Ns::Other),
                ("w:p", Ns::Other),
                ("w:r", Ns::Other),
                ("reset", Ns::None),
                ("plain", Ns::None),
                ("after", Ns::Rels),
                ("w:p", Ns::W),
            ]
        );
    }

    #[test]
    fn wordprocessingml_under_another_prefix_resolves_to_w() {
        let xml = format!(
            "<ns0:document xmlns:ns0=\"{W}\"><ns0:body><ns0:p ns0:rsidR=\"1\"/>\
             </ns0:body></ns0:document>"
        );
        let doc = round_trip(xml.as_bytes());
        let p = doc
            .root
            .find_child(Ns::W, "body")
            .unwrap()
            .find_child(Ns::W, "p")
            .unwrap();
        assert!(p.is(Ns::W, "p"));
        assert_eq!((p.prefix(), p.local_name()), (Some("ns0"), "p"));
        assert_eq!(p.attr("ns0:rsidR").unwrap(), "1");

        // The default namespace too.
        let p = fragment(&format!("<p xmlns=\"{W}\"/>"));
        assert!(p.is(Ns::W, "p"));
        assert_eq!((p.prefix(), p.local_name()), (None, "p"));

        // URIs are compared as written: one spelled with a character
        // reference is another namespace, which fails safe.
        let escaped = W.replace("main", "m&#97;in");
        assert!(fragment(&format!("<w:p xmlns:w=\"{escaped}\"/>")).is(Ns::Other, "p"));
    }

    #[test]
    fn unqualified_names_have_no_namespace() {
        let doc = round_trip(b"<root><child a=\"1\"/></root>");
        assert_eq!(doc.root.ns(), Ns::None);
        assert!(doc.root.child_elements().all(|e| e.is(Ns::None, "child")));
    }

    #[test]
    fn known_namespaces_map_to_their_uri() {
        for ns in Ns::KNOWN {
            let uri = ns.uri().unwrap();
            assert_eq!(Ns::from_uri(uri), ns);
            assert_eq!(fragment(&format!("<x:a xmlns:x=\"{uri}\"/>")).ns(), ns);
        }
        assert_eq!(Ns::from_uri("urn:other"), Ns::Other);
        assert_eq!(Ns::None.uri(), None);
        assert_eq!(Ns::Other.uri(), None);
    }

    #[test]
    fn unknown_prefixes_are_rejected() {
        for xml in [
            "<w:p/>",
            "<a w:b=\"1\"/>",
            "<a><b xmlns:x=\"urn:x\"/><x:c/></a>",
            "<a xmlns:x=\"\"><x:b/></a>",
        ] {
            assert!(matches!(reason(xml), Reason::UnknownPrefix { .. }), "{xml}");
        }
        assert!(matches!(
            fragment_reason("<w:drawing><wp:inline/></w:drawing>"),
            Reason::UnknownPrefix { offset: 0 }
        ));
    }

    // ── Element API ───────────────────────────────────────────────────────────

    #[test]
    fn attr_returns_the_normalized_value() {
        let el = fragment(concat!(
            r#"<a b="x &amp; &lt;y&gt; &quot;z&quot; &apos;&#65;&#x42;" c='say "hi"'"#,
            " d=\"tab&#9;lf&#10;cr&#13;\" e=\"raw\ttab\nlf\r\ncrlf\"",
            " f=\"\" xml:space=\"preserve\"/>"
        ));
        assert_eq!(el.attr("b").unwrap(), "x & <y> \"z\" 'AB");
        assert_eq!(el.attr("c").unwrap(), "say \"hi\"");
        assert_eq!(el.attr("d").unwrap(), "tab\tlf\ncr\r");
        assert_eq!(el.attr("e").unwrap(), "raw tab lf crlf");
        assert_eq!(el.attr("f").unwrap(), "");
        assert!(matches!(
            el.attr("xml:space"),
            Some(Cow::Borrowed("preserve"))
        ));
        assert_eq!(el.attr("space"), None);
        assert_eq!(el.attr("missing"), None);
    }

    #[test]
    fn set_attr_edits_the_tag_in_place() {
        let mut el = fragment(&format!("<w:t xmlns:w=\"{W}\" w:a='1'  w:b=\"2\" />"));
        el.set_attr("w:a", "x\"y'z<&\n");
        el.set_attr("xml:space", "preserve");
        el.set_attr("w:b", "3");
        let xml = el.to_xml();
        assert_eq!(
            xml,
            format!(
                "<w:t xmlns:w=\"{W}\" w:a='x&quot;y&apos;z&lt;&amp;&#10;'  \
                 w:b=\"3\" xml:space=\"preserve\" />"
            )
        );
        let el = fragment(&xml);
        assert_eq!(el.attr("w:a").unwrap(), "x\"y'z<&\n");
        assert_eq!(el.attr("xml:space").unwrap(), "preserve");

        let mut el = Element::new("Override", Ns::Other);
        el.set_attr("PartName", "/word/document.xml");
        el.set_attr("ContentType", "application/xml");
        assert_eq!(
            el.to_xml(),
            r#"<Override PartName="/word/document.xml" ContentType="application/xml"/>"#
        );
    }

    #[test]
    fn text_concatenates_unescaped_character_data() {
        let el = fragment(
            "<w:t xmlns:w=\"urn:w\">a &amp; b<![CDATA[ <c>\r\n ]]>d<!-- x -->e<x/>\
             f&#13;\r\ng\rh</w:t>",
        );
        assert_eq!(el.text(), "a & b <c>\n def\r\ng\nh");
        assert!(matches!(
            fragment("<t>plain</t>").text(),
            Cow::Borrowed("plain")
        ));
        assert!(matches!(fragment("<t/>").text(), Cow::Borrowed("")));
        assert!(matches!(
            fragment("<t><x>nested</x></t>").text(),
            Cow::Borrowed("")
        ));
    }

    #[test]
    fn emptied_elements_keep_how_they_were_closed() {
        // Written with an end tag in the source, an element keeps it when
        // emptied, whitespace included.
        let mut el = fragment("<w:t xmlns:w=\"urn:w\">old<w:x/><!-- c --></w:t>");
        el.children.clear();
        assert_eq!(el.to_xml(), "<w:t xmlns:w=\"urn:w\"></w:t>");
        let mut el = fragment("<t>x</t >");
        el.children.clear();
        assert_eq!(el.to_xml(), "<t></t >");

        // Written as an empty-element tag, it becomes one again.
        let mut el = fragment("<t/>");
        el.children
            .push(Node::Text(escape_text("a < b").into_owned()));
        assert_eq!(el.to_xml(), "<t>a &lt; b</t>");
        el.children.clear();
        assert_eq!(el.to_xml(), "<t/>");
    }

    #[test]
    fn new_elements_build_valid_markup() {
        let mut run = fragment(&format!("<x:r xmlns:x=\"{W}\"><x:rPr><x:b/></x:rPr></x:r>"));
        let prefix = run.prefix().unwrap().to_owned();
        let mut t = Element::new(format!("{prefix}:t"), Ns::W);
        t.set_attr("xml:space", "preserve");
        t.children
            .push(Node::Text(escape_text(" {{name}} & co ").into_owned()));
        run.children.push(Node::Element(t));
        run.children
            .push(Node::Element(Element::new(format!("{prefix}:br"), Ns::W)));
        let xml = run.to_xml();
        assert_eq!(
            xml,
            format!(
                "<x:r xmlns:x=\"{W}\"><x:rPr><x:b/></x:rPr>\
                 <x:t xml:space=\"preserve\"> {{{{name}}}} &amp; co </x:t><x:br/></x:r>"
            )
        );
        // The result parses back to the same tree.
        let parsed = fragment(&xml);
        let t = parsed.find_child(Ns::W, "t").unwrap();
        assert_eq!(t.text(), " {{name}} & co ");
        assert_eq!(parsed.to_xml(), xml);
    }

    #[test]
    fn child_iterators_skip_other_nodes() {
        let mut p = fragment(&format!(
            "<w:p xmlns:w=\"{W}\"> <w:pPr/><!-- c --><w:r><w:t>a</w:t></w:r>text<w:r/></w:p>"
        ));
        let names: Vec<_> = p.child_elements().map(Element::local_name).collect();
        assert_eq!(names, ["pPr", "r", "r"]);
        assert_eq!(p.find_child(Ns::W, "r").unwrap().children.len(), 1);
        assert!(p.find_child(Ns::W, "t").is_none(), "not a direct child");
        assert!(p.find_child(Ns::Wp, "r").is_none(), "wrong namespace");

        for run in p.child_elements_mut().filter(|e| e.is(Ns::W, "r")) {
            run.set_attr("w:rsidR", "00AB");
        }
        let properties = p.child_elements_mut().find(|e| e.is(Ns::W, "pPr"));
        properties
            .unwrap()
            .children
            .push(Node::Text("x".to_owned()));
        assert_eq!(
            p.to_xml(),
            format!(
                "<w:p xmlns:w=\"{W}\"> <w:pPr>x</w:pPr><!-- c -->\
                 <w:r w:rsidR=\"00AB\"><w:t>a</w:t></w:r>text<w:r w:rsidR=\"00AB\"/></w:p>"
            )
        );
        let all: Vec<_> = p.descendants().map(Element::local_name).collect();
        assert_eq!(all, ["pPr", "r", "t", "r"]);
    }

    #[test]
    fn new_document_has_the_word_declaration() {
        let root = fragment(
            "<Relationships \
             xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"/>",
        );
        assert!(root.is(Ns::Rels, "Relationships"));
        let bytes = XmlDocument::new(root).to_bytes();
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n\
             <Relationships \
             xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"/>"
        );
        round_trip(&bytes);
    }

    #[test]
    fn fragments_hold_exactly_one_element() {
        let el = fragment("\n  <a><b/></a>\n");
        assert_eq!(el.to_xml(), "<a><b/></a>");
        assert!(matches!(fragment_reason(""), Reason::NoRoot));
        assert!(matches!(
            fragment_reason("<a/><b/>"),
            Reason::MultipleRoots { offset: 4 }
        ));
        assert!(matches!(
            fragment_reason("<!-- c --><a/>"),
            Reason::OutsideRoot { offset: 0 }
        ));
        assert!(matches!(
            fragment_reason("<a/><?pi?>"),
            Reason::OutsideRoot { offset: 4 }
        ));
        assert!(matches!(
            fragment_reason("<a/>x"),
            Reason::OutsideRoot { offset: 4 }
        ));
        assert!(matches!(
            fragment_reason("\u{FEFF}<a/>"),
            Reason::OutsideRoot { offset: 0 }
        ));
        assert!(matches!(
            fragment_reason("<?xml version=\"1.0\"?><a/>"),
            Reason::Declaration { offset: 0 }
        ));
    }

    // ── Rejected input ────────────────────────────────────────────────────────

    #[test]
    fn misplaced_content_is_rejected() {
        assert!(matches!(
            reason("<a/><b/>"),
            Reason::MultipleRoots { offset: 4 }
        ));
        assert!(matches!(
            reason("<a></a>\n<b>"),
            Reason::MultipleRoots { offset: 8 }
        ));
        assert!(matches!(reason("x<a/>"), Reason::OutsideRoot { offset: 0 }));
        assert!(matches!(
            reason("<a/> x"),
            Reason::OutsideRoot { offset: 4 }
        ));
        assert!(matches!(
            reason("<a/>&amp;"),
            Reason::OutsideRoot { offset: 4 }
        ));
        assert!(matches!(
            reason("<![CDATA[x]]><a/>"),
            Reason::OutsideRoot { offset: 0 }
        ));
        assert!(matches!(
            reason("\u{FEFF}\u{FEFF}<a/>"),
            Reason::OutsideRoot { offset: 3 }
        ));
        for xml in ["", " \n", "<!-- only -->", "<?xml version=\"1.0\"?>"] {
            assert!(matches!(reason(xml), Reason::NoRoot), "{xml:?}");
        }
    }

    #[test]
    fn mismatched_and_unclosed_tags_are_rejected() {
        assert!(matches!(
            reason("<a></b>"),
            Reason::MismatchedEndTag { offset: 3 }
        ));
        assert!(matches!(
            reason("<a><b></a></b>"),
            Reason::MismatchedEndTag { offset: 6 }
        ));
        assert!(matches!(
            reason("<a></ a>"),
            Reason::MismatchedEndTag { .. }
        ));
        assert!(matches!(
            reason("<a></a b=\"1\">"),
            Reason::MismatchedEndTag { .. }
        ));
        assert!(matches!(reason("<a>"), Reason::Unclosed { offset: 3 }));
        assert!(matches!(
            reason("<a><b></b>text"),
            Reason::Unclosed { offset: 14 }
        ));
        for xml in [
            "</a>",
            "<a",
            "<a b=\"1>",
            "<a></a",
            "<a><!-- x</a>",
            "<a><![CDATA[x</a>",
            "<a>&amp</a>",
            "<a>&</a>",
            "<a><?pi</a>",
            "<a><!x></a>",
        ] {
            assert!(matches!(reason(xml), Reason::Syntax { .. }), "{xml}");
        }
    }

    #[test]
    fn unknown_entities_and_invalid_character_references_are_rejected() {
        for reference in [
            "foo",
            "nbsp",
            "AMP",
            "#0",
            "#1",
            "#x1F",
            "#xD800",
            "#xDFFF",
            "#xFFFE",
            "#xFFFF",
            "#x110000",
            "#99999999999999999999",
            "#X41",
            "#",
            "#x",
            "#-65",
            "#+65",
            "#6 5",
            "#x4G",
            "#١٢",
        ] {
            let text = format!("<a>x&{reference};</a>");
            assert!(
                matches!(reason(&text), Reason::Reference { offset: 4 }),
                "{text}"
            );
            let attr = format!("<a b=\"&{reference};\"/>");
            assert!(
                matches!(reason(&attr), Reason::Reference { offset: 0 }),
                "{attr}"
            );
        }
        assert!(matches!(
            reason("<a b=\"&amp\"/>"),
            Reason::Reference { .. }
        ));
        assert!(matches!(
            reason("<a b=\"x & y\"/>"),
            Reason::Reference { .. }
        ));

        let ok = "<a b=\"&#x41;&#65;&#x10FFFF;&#0065;\">&#9;&#10;&#13;&#x20;&#xD7FF;&#xE000;\
                  &#xFFFD;&#x10000;&lt;&gt;&amp;&apos;&quot;</a>";
        let doc = round_trip(ok.as_bytes());
        assert_eq!(doc.root.attr("b").unwrap(), "AA\u{10FFFF}A");
        assert_eq!(
            doc.root.text(),
            "\t\n\r \u{D7FF}\u{E000}\u{FFFD}\u{10000}<>&'\""
        );
    }

    #[test]
    fn unsupported_encodings_are_rejected() {
        // A UTF-16 byte order mark is not UTF-8.
        assert!(matches!(
            reason(b"\xFF\xFE<\x00a\x00/\x00>\x00"),
            Reason::Utf8 { offset: 0 }
        ));
        assert!(matches!(
            reason(b"\xFE\xFF\x00<\x00a\x00/\x00>"),
            Reason::Utf8 { offset: 0 }
        ));
        assert!(matches!(reason(b"<a>\xFF</a>"), Reason::Utf8 { offset: 3 }));
        assert!(matches!(
            reason(b"\xEF\xBB\xBF<a>\xC3</a>"),
            Reason::Utf8 { offset: 6 }
        ));
        for encoding in ["ISO-8859-1", "UTF-16", "US-ASCII", "utf8"] {
            let xml = format!("<?xml version=\"1.0\" encoding=\"{encoding}\"?><a/>");
            assert!(matches!(reason(&xml), Reason::Encoding), "{xml}");
        }
        for encoding in ["UTF-8", "utf-8", "Utf-8"] {
            round_trip(format!("<?xml version='1.0' encoding='{encoding}'?><a/>").as_bytes());
        }
    }

    #[test]
    fn misplaced_or_malformed_declarations_are_rejected() {
        for xml in [
            " <?xml version=\"1.0\"?><a/>",
            "<!-- c --><?xml version=\"1.0\"?><a/>",
            "<a/><?xml version=\"1.0\"?>",
            "<a><?xml version=\"1.0\"?></a>",
            "<?xml version=1.0?><a/>",
            "<?xml version=\"1.0\"encoding=\"UTF-8\"?><a/>",
        ] {
            assert!(matches!(reason(xml), Reason::Declaration { .. }), "{xml}");
        }
        let mut xml = UTF8_BOM.to_vec();
        xml.extend_from_slice(b"<?xml version=\"1.0\"?><a/>");
        round_trip(&xml);
    }

    #[test]
    fn document_type_declarations_are_rejected() {
        assert!(matches!(
            reason("<!DOCTYPE a><a/>"),
            Reason::Doctype { offset: 0 }
        ));
        assert!(matches!(
            reason("<?xml version=\"1.0\"?>\n<!DOCTYPE a [<!ENTITY x \"y\">]><a>&x;</a>"),
            Reason::Doctype { offset: 22 }
        ));
    }

    #[test]
    fn invalid_names_and_attributes_are_rejected() {
        for xml in [
            "<1a/>",
            "<-a/>",
            "<a:b:c xmlns:a=\"urn:x\"/>",
            "<:a/>",
            "<a:/>",
            "<xmlns:a/>",
            "<a\u{0}/>",
            "<a/ >",
            "< a/>",
            "<a 1b=\"x\"/>",
            "<a b:c:d=\"x\"/>",
            "<a =\"x\"/>",
        ] {
            assert!(
                matches!(reason(xml), Reason::Name { .. } | Reason::Attribute { .. }),
                "{xml:?}"
            );
        }
        for xml in [
            "<a b/>",
            "<a b=c/>",
            "<a b=\"1\"c=\"2\"/>",
            "<a b=\"<\"/>",
            "<a b=\"1\" =\"2\"/>",
        ] {
            assert!(
                matches!(reason(xml), Reason::Attribute { offset: 0 }),
                "{xml:?}"
            );
        }
    }

    #[test]
    fn duplicate_attributes_are_rejected() {
        let rels = "http://schemas.openxmlformats.org/package/2006/relationships";
        for xml in [
            "<a b=\"1\" b=\"2\"/>".to_owned(),
            "<a b=\"1\" c=\"\" b=\"1\"></a>".to_owned(),
            "<a xmlns:x=\"urn:x\" xmlns:x=\"urn:y\"/>".to_owned(),
            // An edit that keeps the second would write it twice.
            format!("<w:t xmlns:w=\"{W}\" xml:space=\"default\" xml:space=\"preserve\">x</w:t>"),
            // Readers disagree on which part a relationship points to.
            format!(
                "<Relationships xmlns=\"{rels}\"><Relationship Id=\"rId1\" Type=\"t\" Target=\"a.xml\" Target=\"b.xml\"/></Relationships>"
            ),
            format!(
                "<Relationships xmlns=\"{rels}\"><Relationship Id=\"rId1\" Type=\"t\" Type=\"u\" Target=\"a.xml\"/></Relationships>"
            ),
        ] {
            assert!(matches!(reason(&xml), Reason::Attribute { .. }), "{xml:?}");
            assert!(Element::parse_fragment(&xml).is_err(), "{xml:?}");
        }
        // Names are compared as written: different prefixes bound to the
        // same namespace are different names.
        round_trip(format!("<a xmlns:w=\"{W}\" xmlns:x=\"{W}\" w:b=\"1\" x:b=\"2\"/>").as_bytes());

        // Duplicates among many attributes take the sorting path.
        let many = (0..40)
            .map(|i| format!(" a{i}=\"{i}\""))
            .collect::<Vec<_>>()
            .concat();
        round_trip(format!("<a{many}/>").as_bytes());
        assert!(matches!(
            reason(format!("<a{many} a17=\"x\"/>")),
            Reason::Attribute { offset: 0 }
        ));
    }

    #[test]
    fn node_budget_counts_every_node() {
        // The declaration, whitespace, two elements, text, a comment, CDATA,
        // a processing instruction and whitespace again: nine nodes.
        let xml = b"<?xml version=\"1.0\"?>\n<a><b/>x<!--c--><![CDATA[d]]><?pi?></a>\n";
        let mut budget = 9;
        XmlDocument::parse_with_budget(xml, &mut budget).unwrap();
        assert_eq!(budget, 0);

        let mut budget = 8;
        let Err(InvalidXml(reason)) = XmlDocument::parse_with_budget(xml, &mut budget) else {
            panic!("over budget");
        };
        assert!(
            matches!(reason, Reason::TooManyNodes { offset } if offset == xml.len() - 1),
            "{reason:?}"
        );
        assert_eq!(budget, 8, "unchanged on error");

        // What a part leaves is what the next one gets.
        let mut budget = 5;
        XmlDocument::parse_with_budget(b"<a><b/></a>", &mut budget).unwrap();
        assert_eq!(budget, 3);
        assert!(XmlDocument::parse_with_budget(b"<a>x<b/>y</a>", &mut budget).is_err());

        // Generated snippets are not charged to any budget.
        fragment("<a><b/>x</a>");
    }

    #[test]
    fn long_namespace_uris_take_linear_time() {
        // Every element resolves the URI its prefix is bound to. Reading
        // the whole URI for each of them, as unescaping it did, took time
        // proportional to the URI's length times the number of elements:
        // a 5.6 KB .docx kept the parser busy for a minute. 1 MiB of URI
        // and 50,000 elements take milliseconds now, and minutes that way.
        let uri = "u".repeat(1 << 20);
        let xml = format!("<x:a xmlns:x=\"{uri}\">{}</x:a>", "<x:b/>".repeat(50_000));
        let start = std::time::Instant::now();
        let doc = round_trip(xml.as_bytes());
        let elapsed = start.elapsed();
        assert!(doc.root.descendants().all(|e| e.is(Ns::Other, "b")));
        assert!(elapsed < std::time::Duration::from_secs(10), "{elapsed:?}");
    }

    #[test]
    fn nesting_is_limited_to_512_levels() {
        let doc = round_trip(nested(MAX_DEPTH).as_bytes());
        assert_eq!(doc.root.descendants().count(), MAX_DEPTH - 1);
        let innermost_empty = "<a>".repeat(MAX_DEPTH - 1) + "<a/>" + &"</a>".repeat(MAX_DEPTH - 1);
        round_trip(innermost_empty.as_bytes());

        let offset = 3 * MAX_DEPTH;
        assert!(
            matches!(reason(nested(MAX_DEPTH + 1)), Reason::TooDeep { offset: o } if o == offset)
        );
        let too_deep_empty = "<a>".repeat(MAX_DEPTH) + "<a/>" + &"</a>".repeat(MAX_DEPTH);
        assert!(matches!(reason(too_deep_empty), Reason::TooDeep { offset: o } if o == offset));
        assert!(matches!(
            fragment_reason(&nested(MAX_DEPTH + 1)),
            Reason::TooDeep { .. }
        ));
    }

    #[test]
    fn errors_describe_the_problem() {
        let err = XmlDocument::parse(b"<a></b>").unwrap_err();
        assert_eq!(err.to_string(), "mismatched end tag at byte 3");
        let err = XmlDocument::parse(b"<a").unwrap_err();
        assert_eq!(err.to_string(), "malformed XML at byte 0");
        assert!(std::error::Error::source(&err).is_some());
        let err = XmlDocument::parse(nested(600).as_bytes()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "elements nested deeper than 512 levels at byte 1536"
        );
        let err =
            XmlDocument::parse(format!("<r><{}/></r>", "a".repeat(300)).as_bytes()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "name longer than 256 bytes in the tag at byte 3"
        );
    }

    #[test]
    fn names_are_limited_to_256_bytes() {
        let name = "a".repeat(MAX_NAME_LEN);
        round_trip(format!("<{name} {name}=\"1\"/>").as_bytes());
        let long = "a".repeat(MAX_NAME_LEN + 1);
        assert!(matches!(
            reason(format!("<r><{long}/></r>")),
            Reason::LongName { offset: 3 }
        ));
        assert!(matches!(
            reason(format!("<r><a {long}=\"1\"/></r>")),
            Reason::LongName { offset: 3 }
        ));
        // A prefix is declared by an attribute named `xmlns:` and the
        // prefix, so it is at most 250 bytes long.
        let prefix = "p".repeat(MAX_NAME_LEN - "xmlns:".len());
        round_trip(format!("<{prefix}:a xmlns:{prefix}=\"urn:p\"/>").as_bytes());
        let prefix = "p".repeat(MAX_NAME_LEN - "xmlns:".len() + 1);
        assert!(matches!(
            reason(format!("<{prefix}:a xmlns:{prefix}=\"urn:p\"/>")),
            Reason::LongName { offset: 0 }
        ));
    }

    #[test]
    fn namespace_declarations_in_scope_are_limited() {
        use std::fmt::Write as _;

        let declarations = |count: usize| -> String {
            let mut declarations = String::new();
            for index in 0..count {
                write!(declarations, " xmlns:n{index}=\"urn:n\"").unwrap();
            }
            declarations
        };
        round_trip(format!("<r{}><a/></r>", declarations(MAX_NAMESPACE_BINDINGS)).as_bytes());
        // Those of an element go out of scope with it.
        round_trip(
            format!(
                "<r{}><a xmlns:x=\"urn:x\"/><a xmlns:x=\"urn:x\"/></r>",
                declarations(MAX_NAMESPACE_BINDINGS - 1)
            )
            .as_bytes(),
        );
        // One more is an error at the tag that declares it, even if it
        // declares a prefix again.
        for more in ["xmlns:x=\"urn:x\"", "xmlns:n0=\"urn:n\"", "xmlns=\"urn:d\""] {
            let xml = format!(
                "<r{}><a/><a {more}/></r>",
                declarations(MAX_NAMESPACE_BINDINGS)
            );
            let Reason::Syntax { offset, source } = reason(&xml) else {
                panic!("{more}");
            };
            assert_eq!(offset, xml.find("<a ").unwrap(), "{more}");
            assert!(matches!(source, quick_xml::Error::Namespace(_)), "{more}");
        }
    }

    #[test]
    fn namespace_declarations_are_counted() {
        let element = fragment(
            "<a xmlns=\"urn:a\" xmlns:b=\"urn:b\" b:c=\"1\" xmlnsx=\"2\" xml:space=\"preserve\"/>",
        );
        assert_eq!(element.namespace_declarations(), 2);
        assert_eq!(fragment("<a x=\"xmlns\"/>").namespace_declarations(), 0);
    }

    // ── Robustness ────────────────────────────────────────────────────────────

    #[test]
    fn every_truncation_parses_without_panicking() {
        let mut with_bom = UTF8_BOM.to_vec();
        with_bom.extend_from_slice(WORD_DOCUMENT.as_bytes());
        for input in [
            WORD_DOCUMENT.as_bytes(),
            &with_bom,
            PRETTY_DOCUMENT.as_bytes(),
        ] {
            for len in 0..=input.len() {
                parse_without_panicking(&input[..len]);
            }
        }
        let snippet = fragment(&format!(
            "<w:p xmlns:w=\"{W}\"><w:r><w:t a='&amp;'>x</w:t></w:r></w:p>"
        ))
        .to_xml();
        for (end, _) in snippet.char_indices() {
            let _ = Element::parse_fragment(&snippet[..end]);
        }
    }

    #[test]
    fn byte_flips_parse_without_panicking() {
        // Markup characters, whitespace, and bytes that break UTF-8.
        let replacements = [
            0x00, b'\t', b'\n', b' ', b'!', b'"', b'#', b'&', b'\'', b'-', b'/', b':', b';', b'<',
            b'=', b'>', b'?', b'[', b']', b'x', 0x80, 0xBF, 0xC3, 0xEF, 0xFF,
        ];
        let mut input = UTF8_BOM.to_vec();
        input.extend_from_slice(PRETTY_DOCUMENT.as_bytes());
        let mut mutated = input.clone();
        for i in 0..input.len() {
            for &b in &replacements {
                mutated[i] = b;
                parse_without_panicking(&mutated);
            }
            mutated[i] = input[i];
        }
    }

    #[test]
    fn deepest_tree_is_handled_on_a_small_stack() {
        let xml = nested(MAX_DEPTH);
        // A quarter of the 2 MiB that spawned threads get by default. In a
        // debug build the recursive `PartialEq` needs about 256 KiB here and
        // `Drop` 160 KiB (48 KiB each in release); everything else uses an
        // explicit stack.
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || {
                let doc = XmlDocument::parse(xml.as_bytes()).unwrap();
                let copy = doc.clone();
                assert_eq!(copy, doc);
                assert_eq!(copy.to_bytes(), xml.as_bytes());
                assert!(format!("{copy:?}").contains(&xml));
                drop(copy);
                drop(doc);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    // ── Escaping ──────────────────────────────────────────────────────────────

    #[test]
    fn text_passthrough_borrows() {
        assert!(matches!(escape_text("plain text"), Cow::Borrowed(_)));
        assert!(matches!(escape_attr("plain value"), Cow::Borrowed(_)));
    }

    #[test]
    fn text_escapes_markup() {
        assert_eq!(escape_text("a < b & c > d"), "a &lt; b &amp; c &gt; d");
        assert_eq!(escape_text("quote \" ' stays"), "quote \" ' stays");
        assert_eq!(escape_text("cr\r"), "cr&#13;");
    }

    #[test]
    fn attr_escapes_quotes_and_whitespace() {
        assert_eq!(
            escape_attr("\"x\"\t'y'\n\r"),
            "&quot;x&quot;&#9;&apos;y&apos;&#10;&#13;"
        );
    }

    #[test]
    fn illegal_characters_are_dropped() {
        assert_eq!(escape_text("a\u{0}b\u{b}c\u{FFFE}d"), "abcd");
        assert_eq!(escape_attr("\u{1F}x"), "x");
        assert_eq!(escape_text("tab\tand\nnewline"), "tab\tand\nnewline");
    }

    #[test]
    fn unescaping_inverts_escaping() {
        let s = "a < b & c > d \"e\" 'f'\tg\nh\r\ni\rj \u{1F600}";
        assert_eq!(unescape(&escape_text(s), Unescape::Text), s);
        assert_eq!(unescape(&escape_attr(s), Unescape::Attr), s);
        assert_eq!(
            unescape("a&#13;&#10;b\r\nc\rd", Unescape::Text),
            "a\r\nb\nc\nd"
        );
        assert_eq!(
            unescape("a&#13;&#10;b\r\nc\rd\te", Unescape::Attr),
            "a\r\nb c d e"
        );
        assert_eq!(unescape("a&amp;\r\n", Unescape::CData), "a&amp;\n");
        // Unknown references can only come from edits; they stay as written.
        assert_eq!(unescape("&foo; & &amp", Unescape::Text), "&foo; & &amp");
    }
}
