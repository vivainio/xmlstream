//! A small owned tree. The streaming extractor builds one per match, so memory is bounded by
//! the matched subtree rather than by the document.

use crate::{
    error::Result,
    path::{name_matches, Match, Selector},
    reader::Event,
    writer::Writer,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Element(Element),
    Text(String),
    CData(String),
    Comment(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Element {
    pub fn new(name: impl Into<String>) -> Self {
        Element {
            name: name.into(),
            attrs: Vec::new(),
            children: Vec::new(),
        }
    }

    pub fn with_attrs(name: impl Into<String>, attrs: Vec<(String, String)>) -> Self {
        Element {
            name: name.into(),
            attrs,
            children: Vec::new(),
        }
    }

    pub fn attr_set(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.attrs.push((k.into(), v.into()));
        self
    }

    pub fn child_elem(mut self, e: Element) -> Self {
        self.children.push(Node::Element(e));
        self
    }

    pub fn text_node(mut self, t: impl Into<String>) -> Self {
        self.children.push(Node::Text(t.into()));
        self
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|n| match n {
            Node::Element(e) => Some(e),
            _ => None,
        })
    }

    /// Child elements whose name matches (local name if `name` has no prefix).
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.elements().filter(move |e| name_matches(name, &e.name))
    }

    pub fn child(&self, name: &str) -> Option<&Element> {
        self.elements().find(|e| name_matches(name, &e.name))
    }

    /// Text and CDATA directly inside this element, concatenated.
    pub fn direct_text(&self) -> String {
        let mut s = String::new();
        for n in &self.children {
            if let Node::Text(t) | Node::CData(t) = n {
                s.push_str(t);
            }
        }
        s
    }

    /// All descendant text, concatenated in document order.
    pub fn text(&self) -> String {
        fn go(e: &Element, out: &mut String) {
            for n in &e.children {
                match n {
                    Node::Text(t) | Node::CData(t) => out.push_str(t),
                    Node::Element(c) => go(c, out),
                    Node::Comment(_) => {}
                }
            }
        }
        let mut s = String::new();
        go(self, &mut s);
        s
    }

    /// Runs a selector against this element as if it were the document root: the element
    /// itself is the first thing it can match. For paths relative to the element, see
    /// [`select_str`](Self::select_str).
    pub fn select(&self, sel: &Selector) -> Result<Vec<Match>> {
        sel.select_element(self)
    }

    /// Selects relative to this element, like XPath from a context node, and with the same
    /// path rules as `Extract` fields: `foo` and `./foo` are child elements, `.//foo` and
    /// `//foo` any descendant, `./foo/@id` an attribute, `@id` this element's own attribute,
    /// `text()` its own text and `.` all its text.
    pub fn select_str(&self, path: &str) -> Result<Vec<Match>> {
        let p = path.trim();
        if p == "." {
            return Ok(vec![Match::Text(self.text())]);
        }
        if p == "text()" {
            let t = self.direct_text();
            return Ok(if t.is_empty() { vec![] } else { vec![Match::Text(t)] });
        }
        if let Some(a) = p.strip_prefix('@').filter(|a| !a.is_empty() && !a.contains('/')) {
            return Ok(self.attr(a).map(|v| Match::Attr(v.to_string())).into_iter().collect());
        }
        Ok(Selector::parse_relative(p)?.select_children(self))
    }

    /// Feeds this subtree as events (start, children, end).
    pub(crate) fn for_each_event(&self, f: &mut dyn FnMut(Event)) {
        f(Event::Start {
            name: self.name.clone(),
            attrs: self.attrs.clone(),
        });
        for n in &self.children {
            match n {
                Node::Element(e) => e.for_each_event(f),
                Node::Text(t) => f(Event::Text(t.clone())),
                Node::CData(t) => f(Event::CData(t.clone())),
                Node::Comment(t) => f(Event::Comment(t.clone())),
            }
        }
        f(Event::End {
            name: self.name.clone(),
        });
    }

    pub fn to_xml(&self) -> Result<String> {
        let mut w = Writer::new(Vec::new());
        w.write_element(self)?;
        let bytes = w.finish()?;
        Ok(String::from_utf8(bytes).expect("writer emits UTF-8"))
    }

    pub fn to_xml_pretty(&self, indent: usize) -> Result<String> {
        let mut w = Writer::new(Vec::new()).indent(indent);
        w.write_element(self)?;
        let bytes = w.finish()?;
        Ok(String::from_utf8(bytes).expect("writer emits UTF-8"))
    }
}
