//! An XPath-like selector that runs over a stream of events.
//!
//! Supported syntax:
//!
//! ```text
//! /a/b          child steps anchored at the document root
//! //item        item at any depth
//! a/b           relative paths match at any depth (same as //a/b)
//! *             any element
//! item[3]       3rd `item` among its siblings (1-based)
//! item[@id]     has attribute
//! item[@id='x'] attribute equals ('x' or "x"); also !=
//! item[contains(@id,'x')]  /  item[starts-with(@id,'x')]
//! //item/@id    value of an attribute        -> Match::Attr
//! //item/text() direct text of an element    -> Match::Text
//! ```
//!
//! A name without a prefix matches the local name, so `price` matches both `<price>` and
//! `<ns:price>`; `ns:price` matches that exact qualified name. Predicates only look at the
//! element's own start tag and its position, which is what makes a single forward pass possible.
//! Element matches are delivered when the element closes (so a nested match is reported before
//! the match that contains it); attribute matches are delivered at the start tag.

use std::{collections::VecDeque, io::Read};

use crate::{
    element::{Element, Node},
    error::{Error, Result},
    reader::{Event, Reader},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match {
    Element(Element),
    Attr(String),
    Text(String),
}

impl Match {
    pub fn as_element(&self) -> Option<&Element> {
        match self {
            Match::Element(e) => Some(e),
            _ => None,
        }
    }

    pub fn into_element(self) -> Option<Element> {
        match self {
            Match::Element(e) => Some(e),
            _ => None,
        }
    }

    /// Attribute value, text, or the element's full descendant text.
    pub fn to_text(&self) -> String {
        match self {
            Match::Element(e) => e.text(),
            Match::Attr(s) | Match::Text(s) => s.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Child,
    Descendant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Test {
    Any,
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Cond {
    Exists,
    Eq(String),
    Ne(String),
    Contains(String),
    StartsWith(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pred {
    Pos(usize),
    Attr { name: String, cond: Cond },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Step {
    axis: Axis,
    test: Test,
    preds: Vec<Pred>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Output {
    Element,
    Attr(String),
    Text,
}

/// `pattern` may be a local name or a qualified name; `actual` is the qualified name in the document.
pub(crate) fn name_matches(pattern: &str, actual: &str) -> bool {
    if pattern.contains(':') {
        pattern == actual
    } else {
        actual.rsplit(':').next() == Some(pattern)
    }
}

impl Test {
    fn matches(&self, name: &str) -> bool {
        match self {
            Test::Any => true,
            Test::Name(n) => name_matches(n, name),
        }
    }
}

impl Step {
    fn matches(&self, name: &str, attrs: &[(String, String)], pos: impl Fn(&Test) -> usize) -> bool {
        if !self.test.matches(name) {
            return false;
        }
        self.preds.iter().all(|p| match p {
            Pred::Pos(n) => pos(&self.test) == *n,
            Pred::Attr { name, cond } => {
                let v = attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
                match (cond, v) {
                    (Cond::Exists, v) => v.is_some(),
                    (Cond::Eq(x), v) => v == Some(x.as_str()),
                    (Cond::Ne(x), v) => v.is_some_and(|v| v != x),
                    (Cond::Contains(x), v) => v.is_some_and(|v| v.contains(x.as_str())),
                    (Cond::StartsWith(x), v) => v.is_some_and(|v| v.starts_with(x.as_str())),
                }
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    steps: Vec<Step>,
    output: Output,
    source: String,
}

struct Parser {
    s: Vec<char>,
    i: usize,
}

fn perr(src: &str, at: usize, msg: &str) -> Error {
    Error::Path(format!("{msg} at {at} in {src:?}"))
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        let n = lit.chars().count();
        if self.s[self.i..].iter().take(n).copied().eq(lit.chars()) {
            self.i += n;
            true
        } else {
            false
        }
    }

    fn ident(&mut self) -> String {
        let start = self.i;
        while self
            .peek()
            .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '-' | '.'))
        {
            self.i += 1;
        }
        self.s[start..self.i].iter().collect()
    }

    fn literal(&mut self, src: &str) -> Result<String> {
        let q = match self.peek() {
            Some(q @ ('\'' | '"')) => q,
            _ => return Err(perr(src, self.i, "expected a quoted string")),
        };
        self.i += 1;
        let start = self.i;
        while self.peek().is_some_and(|c| c != q) {
            self.i += 1;
        }
        if self.peek().is_none() {
            return Err(perr(src, start, "unterminated string"));
        }
        let v = self.s[start..self.i].iter().collect();
        self.i += 1;
        Ok(v)
    }

    fn attr_name(&mut self, src: &str) -> Result<String> {
        if !self.eat("@") {
            return Err(perr(src, self.i, "expected @attribute"));
        }
        let n = self.ident();
        if n.is_empty() {
            return Err(perr(src, self.i, "empty attribute name"));
        }
        Ok(n)
    }

    fn predicate(&mut self, src: &str) -> Result<Pred> {
        self.ws();
        let pred = if self.peek().is_some_and(|c| c.is_ascii_digit()) {
            let start = self.i;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
            let n: usize = self.s[start..self.i]
                .iter()
                .collect::<String>()
                .parse()
                .map_err(|_| perr(src, start, "bad position"))?;
            if n == 0 {
                return Err(perr(src, start, "positions are 1-based"));
            }
            Pred::Pos(n)
        } else if self.peek() == Some('@') {
            let name = self.attr_name(src)?;
            self.ws();
            let cond = if self.eat("!=") {
                self.ws();
                Cond::Ne(self.literal(src)?)
            } else if self.eat("=") {
                self.ws();
                Cond::Eq(self.literal(src)?)
            } else {
                Cond::Exists
            };
            Pred::Attr { name, cond }
        } else {
            let f = self.ident();
            if f != "contains" && f != "starts-with" {
                return Err(perr(
                    src,
                    self.i,
                    "unsupported predicate (use a position, @attr, contains(@a,'x') or starts-with(@a,'x'))",
                ));
            }
            self.ws();
            if !self.eat("(") {
                return Err(perr(src, self.i, "expected ("));
            }
            self.ws();
            let name = self.attr_name(src)?;
            self.ws();
            if !self.eat(",") {
                return Err(perr(src, self.i, "expected ,"));
            }
            self.ws();
            let lit = self.literal(src)?;
            self.ws();
            if !self.eat(")") {
                return Err(perr(src, self.i, "expected )"));
            }
            let cond = if f == "contains" {
                Cond::Contains(lit)
            } else {
                Cond::StartsWith(lit)
            };
            Pred::Attr { name, cond }
        };
        self.ws();
        if !self.eat("]") {
            return Err(perr(src, self.i, "expected ]"));
        }
        Ok(pred)
    }
}

impl Selector {
    pub fn parse(src: &str) -> Result<Selector> {
        let mut p = Parser {
            s: src.chars().collect(),
            i: 0,
        };
        p.ws();
        let mut axis = if p.eat("//") {
            Axis::Descendant
        } else if p.eat("/") {
            Axis::Child
        } else {
            Axis::Descendant
        };
        let mut steps: Vec<Step> = Vec::new();
        let mut output = Output::Element;
        loop {
            let terminal = if p.peek() == Some('@') {
                Some(Output::Attr(p.attr_name(src)?))
            } else if p.eat("text()") {
                Some(Output::Text)
            } else {
                None
            };
            if let Some(t) = terminal {
                if steps.is_empty() {
                    steps.push(Step {
                        axis,
                        test: Test::Any,
                        preds: vec![],
                    });
                } else if axis == Axis::Descendant {
                    return Err(perr(
                        src,
                        p.i,
                        "'//' before @attr or text() is only supported at the start; use //*/@attr",
                    ));
                }
                output = t;
                p.ws();
                if p.peek().is_some() {
                    return Err(perr(src, p.i, "nothing may follow @attr or text()"));
                }
                break;
            }
            let test = if p.eat("*") {
                Test::Any
            } else {
                let n = p.ident();
                if n.is_empty() {
                    return Err(perr(src, p.i, "expected an element name"));
                }
                Test::Name(n)
            };
            let mut preds = Vec::new();
            while p.eat("[") {
                preds.push(p.predicate(src)?);
            }
            steps.push(Step { axis, test, preds });
            p.ws();
            if p.peek().is_none() {
                break;
            }
            axis = if p.eat("//") {
                Axis::Descendant
            } else if p.eat("/") {
                Axis::Child
            } else {
                return Err(perr(src, p.i, "expected / or //"));
            };
        }
        Ok(Selector {
            steps,
            output,
            source: src.to_string(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// Streams matches out of `reader` one at a time.
    pub fn select<R: Read>(&self, reader: Reader<R>) -> Select<R> {
        Select(SelectorSet::from_selectors(vec![self.clone()]).select(reader))
    }

    pub fn select_str<'a>(&self, xml: &'a str) -> Select<std::io::Cursor<&'a [u8]>> {
        self.select(Reader::from_str(xml))
    }

    pub(crate) fn select_element(&self, e: &Element) -> Result<Vec<Match>> {
        let mut engine = Engine::new(vec![self.clone()]);
        let mut out = VecDeque::new();
        e.for_each_event(&mut |ev| engine.feed(&ev, &mut out));
        Ok(out.into_iter().map(|(_, m)| m).collect())
    }
}

/// Several selectors evaluated in one pass over the same stream.
#[derive(Debug, Clone, Default)]
pub struct SelectorSet {
    sels: Vec<Selector>,
}

impl SelectorSet {
    pub fn new() -> Self {
        Self::default()
    }

    fn from_selectors(sels: Vec<Selector>) -> Self {
        SelectorSet { sels }
    }

    /// Adds a selector and returns the index hits will carry.
    pub fn add(&mut self, path: &str) -> Result<usize> {
        self.sels.push(Selector::parse(path)?);
        Ok(self.sels.len() - 1)
    }

    pub fn select<R: Read>(&self, reader: Reader<R>) -> Hits<R> {
        Hits {
            reader,
            engine: Engine::new(self.sels.clone()),
            queue: VecDeque::new(),
            failed: false,
        }
    }
}

/// Iterator of `(selector index, match)`.
pub struct Hits<R: Read> {
    reader: Reader<R>,
    engine: Engine,
    queue: VecDeque<(usize, Match)>,
    failed: bool,
}

impl<R: Read> Iterator for Hits<R> {
    type Item = Result<(usize, Match)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(h) = self.queue.pop_front() {
                return Some(Ok(h));
            }
            if self.failed {
                return None;
            }
            match self.reader.next_event() {
                Ok(Some(ev)) => self.engine.feed(&ev, &mut self.queue),
                Ok(None) => return None,
                Err(e) => {
                    self.failed = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

/// Iterator of matches for a single selector.
pub struct Select<R: Read>(Hits<R>);

impl<R: Read> Iterator for Select<R> {
    type Item = Result<Match>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|r| r.map(|(_, m)| m))
    }
}

struct Frame {
    /// Per selector: step indexes to try against this element's children.
    states: Vec<Vec<usize>>,
    /// Children seen so far, by qualified name (only kept when a selector uses positions).
    counts: Vec<(String, usize)>,
}

enum CapKind {
    Full,
    Text,
}

struct Capture {
    sel: usize,
    kind: CapKind,
    /// `frames.len()` right after the captured element opened.
    level: usize,
    stack: Vec<Element>,
    text: String,
}

struct Engine {
    sels: Vec<Selector>,
    frames: Vec<Frame>,
    caps: Vec<Capture>,
    needs_pos: bool,
}

impl Engine {
    fn new(sels: Vec<Selector>) -> Self {
        let needs_pos = sels
            .iter()
            .any(|s| s.steps.iter().any(|st| st.preds.iter().any(|p| matches!(p, Pred::Pos(_)))));
        let root = Frame {
            states: vec![vec![0]; sels.len()],
            counts: Vec::new(),
        };
        Engine {
            sels,
            frames: vec![root],
            caps: Vec::new(),
            needs_pos,
        }
    }

    fn feed(&mut self, ev: &Event, out: &mut VecDeque<(usize, Match)>) {
        match ev {
            Event::Start { name, attrs } => self.start(name, attrs, out),
            Event::End { .. } => self.end(out),
            Event::Text(t) | Event::CData(t) => {
                let cdata = matches!(ev, Event::CData(_));
                let level = self.frames.len();
                for c in &mut self.caps {
                    match c.kind {
                        CapKind::Full => {
                            let n = if cdata {
                                Node::CData(t.clone())
                            } else {
                                Node::Text(t.clone())
                            };
                            c.stack.last_mut().unwrap().children.push(n);
                        }
                        CapKind::Text if c.level == level => c.text.push_str(t),
                        CapKind::Text => {}
                    }
                }
            }
            Event::Comment(t) => {
                for c in &mut self.caps {
                    if let CapKind::Full = c.kind {
                        c.stack.last_mut().unwrap().children.push(Node::Comment(t.clone()));
                    }
                }
            }
        }
    }

    fn start(&mut self, name: &str, attrs: &[(String, String)], out: &mut VecDeque<(usize, Match)>) {
        let needs_pos = self.needs_pos;
        let parent = self.frames.last_mut().unwrap();
        if needs_pos {
            match parent.counts.iter_mut().find(|(n, _)| n == name) {
                Some((_, c)) => *c += 1,
                None => parent.counts.push((name.to_string(), 1)),
            }
        }
        let counts = &parent.counts;
        let pos = |t: &Test| -> usize {
            counts
                .iter()
                .filter(|(n, _)| t.matches(n))
                .map(|(_, c)| *c)
                .sum()
        };

        let mut next_states = Vec::with_capacity(self.sels.len());
        let mut hits = Vec::new();
        for (si, sel) in self.sels.iter().enumerate() {
            let mut ns = Vec::new();
            for &i in &parent.states[si] {
                let step = &sel.steps[i];
                if step.axis == Axis::Descendant {
                    ns.push(i);
                }
                if step.matches(name, attrs, pos) {
                    if i + 1 == sel.steps.len() {
                        hits.push(si);
                    } else {
                        ns.push(i + 1);
                    }
                }
            }
            ns.sort_unstable();
            ns.dedup();
            next_states.push(ns);
        }

        for c in &mut self.caps {
            if let CapKind::Full = c.kind {
                c.stack.push(Element::with_attrs(name, attrs.to_vec()));
            }
        }
        self.frames.push(Frame {
            states: next_states,
            counts: Vec::new(),
        });
        let level = self.frames.len();
        for si in hits {
            match &self.sels[si].output {
                Output::Attr(a) => {
                    if let Some((_, v)) = attrs.iter().find(|(k, _)| k == a) {
                        out.push_back((si, Match::Attr(v.clone())));
                    }
                }
                Output::Element => self.caps.push(Capture {
                    sel: si,
                    kind: CapKind::Full,
                    level,
                    stack: vec![Element::with_attrs(name, attrs.to_vec())],
                    text: String::new(),
                }),
                Output::Text => self.caps.push(Capture {
                    sel: si,
                    kind: CapKind::Text,
                    level,
                    stack: Vec::new(),
                    text: String::new(),
                }),
            }
        }
    }

    fn end(&mut self, out: &mut VecDeque<(usize, Match)>) {
        let level = self.frames.len();
        let mut keep = Vec::with_capacity(self.caps.len());
        for mut c in self.caps.drain(..) {
            match c.kind {
                CapKind::Full => {
                    let el = c.stack.pop().unwrap();
                    match c.stack.last_mut() {
                        Some(parent) => {
                            parent.children.push(Node::Element(el));
                            keep.push(c);
                        }
                        None => out.push_back((c.sel, Match::Element(el))),
                    }
                }
                CapKind::Text if c.level == level => {
                    if !c.text.is_empty() {
                        out.push_back((c.sel, Match::Text(c.text)));
                    }
                }
                CapKind::Text => keep.push(c),
            }
        }
        self.caps = keep;
        self.frames.pop();
    }
}
