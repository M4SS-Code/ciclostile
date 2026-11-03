//! Test helpers shared by the unit tests of several modules.

// ── Snippet parser ───────────────────────────────────────────────────────────

/// A parser for generated snippets, used by the image and table tests: it
/// checks well-formedness and namespaces with `quick_xml::NsReader` and
/// returns a small tree to assert on.
pub(crate) mod snippet {
    use quick_xml::{
        XmlVersion,
        escape::resolve_xml_entity,
        events::{BytesStart, Event},
        name::ResolveResult,
        reader::NsReader,
    };

    /// One element of a parsed snippet.
    #[derive(Debug)]
    pub(crate) struct Element {
        /// Qualified name as written, such as `w:tbl`.
        pub(crate) name: String,
        /// Namespace URI the name's prefix resolves to.
        pub(crate) namespace: String,
        /// Namespace declarations on this element: prefix and URI.
        pub(crate) declarations: Vec<(String, String)>,
        /// Other attributes: qualified name and value, unescaped and
        /// normalized like an XML parser does.
        pub(crate) attributes: Vec<(String, String)>,
        /// Child elements, in document order.
        pub(crate) children: Vec<Element>,
        /// Character data directly inside this element, unescaped.
        pub(crate) text: String,
    }

    impl Element {
        /// Return the value of the attribute named `name`.
        pub(crate) fn attr(&self, name: &str) -> Option<&str> {
            self.attributes
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        }

        /// Return the only child element named `name`.
        ///
        /// # Panics
        ///
        /// Panics if there is no such child, or more than one.
        pub(crate) fn child(&self, name: &str) -> &Element {
            let mut matches = self.children.iter().filter(|child| child.name == name);
            let child = matches
                .next()
                .unwrap_or_else(|| panic!("no `{name}` in `{}`", self.name));
            assert!(
                matches.next().is_none(),
                "several `{name}` in `{}`",
                self.name
            );
            child
        }

        /// Return the names of the child elements, in order.
        pub(crate) fn child_names(&self) -> Vec<&str> {
            self.children
                .iter()
                .map(|child| child.name.as_str())
                .collect()
        }

        /// Return this element and all its descendants, in document
        /// order.
        pub(crate) fn descendants(&self) -> Vec<&Element> {
            let mut all = vec![self];
            for child in &self.children {
                all.extend(child.descendants());
            }
            all
        }

        /// Render the element names as a nested outline, such as
        /// `a[b,c[d]]`, to assert structure and order at once.
        pub(crate) fn outline(&self) -> String {
            if self.children.is_empty() {
                return self.name.clone();
            }
            let children: Vec<String> = self.children.iter().map(Element::outline).collect();
            format!("{}[{}]", self.name, children.join(","))
        }
    }

    /// Parse `xml` as a single standalone element.
    ///
    /// # Panics
    ///
    /// Panics if `xml` is not exactly one well-formed element, if an
    /// element or a prefixed attribute uses an undeclared prefix, or if
    /// `xml` holds anything but elements and text (comments, CDATA,
    /// processing instructions, text outside the root).
    pub(crate) fn parse(xml: &str) -> Element {
        let mut reader = NsReader::from_str(xml);
        let mut open: Vec<Element> = Vec::new();
        let mut root = None;
        loop {
            let event = reader
                .read_event()
                .unwrap_or_else(|err| panic!("malformed XML ({err}): {xml}"));
            match event {
                Event::Start(start) => open.push(element(&reader, &start)),
                Event::Empty(start) => attach(&mut open, &mut root, element(&reader, &start)),
                Event::End(_) => {
                    let element = open.pop().expect("end tag without start tag");
                    attach(&mut open, &mut root, element);
                }
                Event::Text(text) => open
                    .last_mut()
                    .unwrap_or_else(|| panic!("text outside the root element: {xml}"))
                    .text
                    .push_str(&text.xml10_content()),
                Event::GeneralRef(reference) => {
                    let resolved = match reference.resolve_char_ref() {
                        Ok(Some(c)) => c.to_string(),
                        Ok(None) => resolve_xml_entity(&reference)
                            .expect("predefined entity")
                            .to_owned(),
                        Err(err) => panic!("bad character reference ({err}): {xml}"),
                    };
                    open.last_mut()
                        .unwrap_or_else(|| panic!("reference outside the root element: {xml}"))
                        .text
                        .push_str(&resolved);
                }
                Event::Eof => break,
                other => panic!("unexpected {other:?} in {xml}"),
            }
        }
        assert!(open.is_empty(), "unclosed elements: {xml}");
        root.expect("no root element")
    }

    /// Attach a complete element to its parent, or make it the root.
    fn attach(open: &mut [Element], root: &mut Option<Element>, element: Element) {
        if let Some(parent) = open.last_mut() {
            parent.children.push(element);
        } else {
            assert!(
                root.replace(element).is_none(),
                "more than one root element"
            );
        }
    }

    /// Build an element from its start tag, checking that its prefixes
    /// are declared.
    fn element(reader: &NsReader<&[u8]>, start: &BytesStart<'_>) -> Element {
        let name = start.name();
        let ResolveResult::Bound(namespace) = reader.resolver().resolve_element(name).0 else {
            panic!("element `{}` is in no declared namespace", name.as_ref());
        };
        let mut declarations = Vec::new();
        let mut attributes = Vec::new();
        for attribute in start.attributes() {
            let attribute = attribute.expect("malformed attribute");
            let key = attribute.key.as_ref().to_owned();
            let value = attribute
                .normalized_value(XmlVersion::Implicit1_0)
                .expect("malformed attribute value")
                .into_owned();
            if let Some(prefix) = key.strip_prefix("xmlns:") {
                declarations.push((prefix.to_owned(), value));
            } else if key == "xmlns" {
                declarations.push((String::new(), value));
            } else {
                if attribute.key.prefix().is_some() {
                    let resolved = reader.resolver().resolve_attribute(attribute.key).0;
                    assert!(
                        matches!(resolved, ResolveResult::Bound(_)),
                        "attribute `{key}` is in no declared namespace"
                    );
                }
                attributes.push((key, value));
            }
        }
        Element {
            name: name.as_ref().to_owned(),
            namespace: namespace.0.to_owned(),
            declarations,
            attributes,
            children: Vec::new(),
            text: String::new(),
        }
    }
}
