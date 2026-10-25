//! An XPath-like selector that runs over a stream of events.
//!
//! Predicates on a start tag are decided immediately; predicates on content (`[sku='a']`) hold the
//! candidate element in memory and are decided when it closes. The input is never rewound.
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

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
enum Cond {
    Exists,
    Eq(String),
    Ne(String),
    Contains(String),
    StartsWith(String),
    Num(Op, f64),
}

#[derive(Debug, Clone, PartialEq)]
enum Pred {
    Pos(usize),
    Attr {
        name: String,
        cond: Cond,
    },
    /// Looks at the element's content, so it is decided when the element closes.
    Child {
        path: Selector,
        cond: Cond,
    },
}

impl Cond {
    /// Everything except `Exists`, which depends on whether a value is there at all.
    fn holds(&self, v: &str) -> bool {
        match self {
            Cond::Exists => true,
            Cond::Eq(x) => v == x,
            Cond::Ne(x) => v != x,
            Cond::Contains(x) => v.contains(x.as_str()),
            Cond::StartsWith(x) => v.starts_with(x.as_str()),
            Cond::Num(op, n) => v.trim().parse::<f64>().is_ok_and(|f| match op {
                Op::Eq => f == *n,
                Op::Ne => f != *n,
                Op::Lt => f < *n,
                Op::Le => f <= *n,
                Op::Gt => f > *n,
                Op::Ge => f >= *n,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
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

/// What to do with a candidate that does not fit the memory budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overflow {
    /// Stop with `Error::Limit`.
    #[default]
    Error,
    /// Drop that candidate (the largest one first) and carry on.
    Skip,
}

/// Memory budget for buffered candidates. Sizes count the bytes of names, attribute
/// keys and values, and text; the real heap use is a small multiple of that.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Largest single candidate subtree.
    pub max_element_bytes: usize,
    /// All candidates being buffered at one moment, added together.
    pub max_total_bytes: usize,
    pub on_overflow: Overflow,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_element_bytes: usize::MAX,
            max_total_bytes: usize::MAX,
            on_overflow: Overflow::Error,
        }
    }
}

/// A start tag that matched a selector, shown to a gate before anything is buffered.
#[derive(Debug, Clone, Copy)]
pub struct StartTag<'a> {
    pub name: &'a str,
    pub attrs: &'a [(String, String)],
    /// Number of open ancestors; the root element has depth 0.
    pub depth: usize,
}

impl StartTag<'_> {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

pub(crate) type Gate<'g> = &'g mut dyn FnMut(usize, &StartTag) -> bool;

fn elem_size(name: &str, attrs: &[(String, String)]) -> usize {
    name.len() + attrs.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>()
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
    fn matches(
        &self,
        name: &str,
        attrs: &[(String, String)],
        pos: impl Fn(&Test) -> usize,
    ) -> bool {
        if !self.test.matches(name) {
            return false;
        }
        self.preds.iter().all(|p| match p {
            Pred::Pos(n) => pos(&self.test) == *n,
            Pred::Attr { name, cond } => {
                let v = attrs
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.as_str());
                match (cond, v) {
                    (Cond::Exists, v) => v.is_some(),
                    (c, v) => v.is_some_and(|v| c.holds(v)),
                }
            }
            // decided later, once the element has been read
            Pred::Child { .. } => true,
        })
    }

    fn has_deferred(&self) -> bool {
        self.preds.iter().any(|p| matches!(p, Pred::Child { .. }))
    }

    /// The deferred predicates, checked against a fully read element.
    fn deferred_hold(&self, el: &Element) -> bool {
        self.preds.iter().all(|p| match p {
            Pred::Child { path, cond } => {
                let found = path.select_children(el);
                match cond {
                    Cond::Exists => !found.is_empty(),
                    c => found.iter().any(|m| c.holds(&m.to_text())),
                }
            }
            _ => true,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
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

    /// A quoted string, or a bare number.
    fn value(&mut self, src: &str, op: Op) -> Result<Cond> {
        self.ws();
        if matches!(self.peek(), Some('\'' | '"')) {
            let lit = self.literal(src)?;
            return match op {
                Op::Eq => Ok(Cond::Eq(lit)),
                Op::Ne => Ok(Cond::Ne(lit)),
                _ => Err(perr(src, self.i, "< and > compare numbers, not strings")),
            };
        }
        let start = self.i;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+'))
        {
            self.i += 1;
        }
        let n: f64 = self.s[start..self.i]
            .iter()
            .collect::<String>()
            .parse()
            .map_err(|_| perr(src, start, "expected a quoted string or a number"))?;
        Ok(Cond::Num(op, n))
    }

    fn operator(&mut self) -> Option<Op> {
        self.ws();
        for (lit, op) in [
            ("!=", Op::Ne),
            ("<=", Op::Le),
            (">=", Op::Ge),
            ("=", Op::Eq),
            ("<", Op::Lt),
            (">", Op::Gt),
        ] {
            if self.eat(lit) {
                return Some(op);
            }
        }
        None
    }

    /// A relative path inside a predicate, up to an operator or closing bracket.
    fn rel_path(&mut self, src: &str) -> Result<Selector> {
        let start = self.i;
        while self
            .peek()
            .is_some_and(|c| !matches!(c, '=' | '!' | '<' | '>' | ']' | ',' | ')' | '['))
        {
            self.i += 1;
        }
        let text: String = self.s[start..self.i].iter().collect();
        Selector::parse_with(text.trim(), Axis::Child)
            .map_err(|e| perr(src, start, &format!("bad path in predicate ({e})")))
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
            let cond = match self.operator() {
                Some(op) => self.value(src, op)?,
                None => Cond::Exists,
            };
            Pred::Attr { name, cond }
        } else {
            let start = self.i;
            let f = self.ident();
            self.ws();
            if (f == "contains" || f == "starts-with") && self.eat("(") {
                self.ws();
                let attr = if self.peek() == Some('@') {
                    Some(self.attr_name(src)?)
                } else {
                    None
                };
                let path = match attr {
                    Some(_) => None,
                    None => Some(self.rel_path(src)?),
                };
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
                match (attr, path) {
                    (Some(name), _) => Pred::Attr { name, cond },
                    (_, Some(path)) => Pred::Child { path, cond },
                    _ => unreachable!(),
                }
            } else {
                self.i = start;
                let path = self.rel_path(src)?;
                let cond = match self.operator() {
                    Some(op) => self.value(src, op)?,
                    None => Cond::Exists,
                };
                Pred::Child { path, cond }
            }
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
        Self::parse_with(src, Axis::Descendant)
    }

    /// `default_axis` applies to a first step with no leading slash.
    fn parse_with(src: &str, default_axis: Axis) -> Result<Selector> {
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
            default_axis
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
        self.select_with(reader, Limits::default())
    }

    /// Like `select`, with a memory budget for buffered candidates.
    pub fn select_with<R: Read>(&self, reader: Reader<R>, limits: Limits) -> Select<R> {
        Select(
            SelectorSet::from_selectors(vec![self.clone()])
                .limits(limits)
                .select(reader),
        )
    }

    pub fn select_str<'a>(&self, xml: &'a str) -> Select<std::io::Cursor<&'a [u8]>> {
        self.select(Reader::from_str(xml))
    }

    /// Runs against the children of `e`, so `e` itself plays the document node.
    fn select_children(&self, e: &Element) -> Vec<Match> {
        let mut engine = Engine::new(vec![self.clone()], Limits::default());
        let mut out = VecDeque::new();
        let mut feed = |ev: Event| {
            let _ = engine.feed(&ev, &mut out, &mut allow_all);
        };
        for n in &e.children {
            match n {
                Node::Element(c) => c.for_each_event(&mut feed),
                Node::Text(t) => feed(Event::Text(t.clone())),
                Node::CData(t) => feed(Event::CData(t.clone())),
                Node::Comment(t) => feed(Event::Comment(t.clone())),
            }
        }
        out.into_iter().map(|(_, m)| m).collect()
    }

    pub(crate) fn select_element(&self, e: &Element) -> Result<Vec<Match>> {
        let mut engine = Engine::new(vec![self.clone()], Limits::default());
        let mut out = VecDeque::new();
        e.for_each_event(&mut |ev| {
            let _ = engine.feed(&ev, &mut out, &mut allow_all);
        });
        Ok(out.into_iter().map(|(_, m)| m).collect())
    }
}

/// Several selectors evaluated in one pass over the same stream.
#[derive(Debug, Clone, Default)]
pub struct SelectorSet {
    sels: Vec<Selector>,
    limits: Limits,
}

impl SelectorSet {
    pub fn new() -> Self {
        Self::default()
    }

    fn from_selectors(sels: Vec<Selector>) -> Self {
        SelectorSet {
            sels,
            limits: Limits::default(),
        }
    }

    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Adds a selector and returns the index hits will carry.
    pub fn add(&mut self, path: &str) -> Result<usize> {
        self.sels.push(Selector::parse(path)?);
        Ok(self.sels.len() - 1)
    }

    pub fn select<R: Read>(&self, reader: Reader<R>) -> Hits<R> {
        Hits {
            reader,
            engine: Engine::new(self.sels.clone(), self.limits),
            queue: VecDeque::new(),
            failed: false,
        }
    }
}

fn allow_all(_: usize, _: &StartTag) -> bool {
    true
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
                Ok(Some(ev)) => {
                    if let Err(e) = self.engine.feed(&ev, &mut self.queue, &mut allow_all) {
                        self.failed = true;
                        return Some(Err(e));
                    }
                }
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
    /// Collects the whole subtree. With `deferred`, the element is a candidate for the step at
    /// that index and is only reported (or searched further) if its child predicates hold.
    Full {
        deferred: Option<usize>,
    },
    Text,
}

struct Capture {
    sel: usize,
    kind: CapKind,
    /// `frames.len()` right after the captured element opened.
    level: usize,
    stack: Vec<Element>,
    text: String,
    /// Size charged against the budget so far.
    bytes: usize,
}

pub(crate) struct Engine {
    sels: Vec<Selector>,
    frames: Vec<Frame>,
    caps: Vec<Capture>,
    needs_pos: bool,
    limits: Limits,
    total: usize,
    pub(crate) skipped: u64,
}

impl Engine {
    pub(crate) fn new(sels: Vec<Selector>, limits: Limits) -> Self {
        let needs_pos = sels.iter().any(|s| {
            s.steps
                .iter()
                .any(|st| st.preds.iter().any(|p| matches!(p, Pred::Pos(_))))
        });
        let root = Frame {
            states: vec![vec![0]; sels.len()],
            counts: Vec::new(),
        };
        Engine {
            sels,
            frames: vec![root],
            caps: Vec::new(),
            needs_pos,
            limits,
            total: 0,
            skipped: 0,
        }
    }

    pub(crate) fn feed(
        &mut self,
        ev: &Event,
        out: &mut VecDeque<(usize, Match)>,
        gate: Gate,
    ) -> Result<()> {
        match ev {
            Event::Start { name, attrs } => self.start(name, attrs, out, gate)?,
            Event::End { .. } => self.end(out),
            Event::Text(t) | Event::CData(t) => {
                let cdata = matches!(ev, Event::CData(_));
                let level = self.frames.len();
                let mut added = 0;
                for c in &mut self.caps {
                    match c.kind {
                        CapKind::Full { .. } => {
                            let n = if cdata {
                                Node::CData(t.clone())
                            } else {
                                Node::Text(t.clone())
                            };
                            c.stack.last_mut().unwrap().children.push(n);
                            c.bytes += t.len();
                            added += t.len();
                        }
                        CapKind::Text if c.level == level => {
                            c.text.push_str(t);
                            c.bytes += t.len();
                            added += t.len();
                        }
                        CapKind::Text => {}
                    }
                }
                self.total += added;
                self.enforce()?;
            }
            Event::Comment(t) => {
                let mut added = 0;
                for c in &mut self.caps {
                    if let CapKind::Full { .. } = c.kind {
                        c.stack
                            .last_mut()
                            .unwrap()
                            .children
                            .push(Node::Comment(t.clone()));
                        c.bytes += t.len();
                        added += t.len();
                    }
                }
                self.total += added;
                self.enforce()?;
            }
        }
        Ok(())
    }

    /// Applies the memory budget: fails, or drops the largest offending candidate.
    fn enforce(&mut self) -> Result<()> {
        loop {
            let over = self
                .caps
                .iter()
                .position(|c| c.bytes > self.limits.max_element_bytes)
                .or_else(|| {
                    if self.total > self.limits.max_total_bytes {
                        (0..self.caps.len()).max_by_key(|&i| self.caps[i].bytes)
                    } else {
                        None
                    }
                });
            let Some(i) = over else { return Ok(()) };
            match self.limits.on_overflow {
                Overflow::Error => {
                    return Err(Error::Limit(format!(
                        "candidate element exceeds the memory budget ({} bytes buffered, {} in the largest)",
                        self.total, self.caps[i].bytes
                    )))
                }
                Overflow::Skip => {
                    let c = self.caps.remove(i);
                    self.total = self.total.saturating_sub(c.bytes);
                    self.skipped += 1;
                }
            }
        }
    }

    fn start(
        &mut self,
        name: &str,
        attrs: &[(String, String)],
        out: &mut VecDeque<(usize, Match)>,
        gate: Gate,
    ) -> Result<()> {
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
        let mut candidates = Vec::new();
        for (si, sel) in self.sels.iter().enumerate() {
            let mut ns = Vec::new();
            for &i in &parent.states[si] {
                let step = &sel.steps[i];
                if step.axis == Axis::Descendant {
                    ns.push(i);
                }
                if step.matches(name, attrs, pos) {
                    if step.has_deferred() {
                        candidates.push((si, i));
                    } else if i + 1 == sel.steps.len() {
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

        let size = elem_size(name, attrs);
        for c in &mut self.caps {
            if let CapKind::Full { .. } = c.kind {
                c.stack.push(Element::with_attrs(name, attrs.to_vec()));
                c.bytes += size;
                self.total += size;
            }
        }
        let tag = StartTag {
            name,
            attrs,
            depth: self.frames.len() - 1,
        };
        self.frames.push(Frame {
            states: next_states,
            counts: Vec::new(),
        });
        let level = self.frames.len();
        for (si, step) in candidates {
            if !gate(si, &tag) {
                continue;
            }
            self.total += size;
            self.caps.push(Capture {
                sel: si,
                kind: CapKind::Full {
                    deferred: Some(step),
                },
                level,
                stack: vec![Element::with_attrs(name, attrs.to_vec())],
                text: String::new(),
                bytes: size,
            });
        }
        for si in hits {
            if !gate(si, &tag) {
                continue;
            }
            match &self.sels[si].output {
                Output::Attr(a) => {
                    if let Some((_, v)) = attrs.iter().find(|(k, _)| k == a) {
                        out.push_back((si, Match::Attr(v.clone())));
                    }
                }
                Output::Element => {
                    self.total += size;
                    self.caps.push(Capture {
                        sel: si,
                        kind: CapKind::Full { deferred: None },
                        level,
                        stack: vec![Element::with_attrs(name, attrs.to_vec())],
                        text: String::new(),
                        bytes: size,
                    })
                }
                Output::Text => self.caps.push(Capture {
                    sel: si,
                    kind: CapKind::Text,
                    level,
                    stack: Vec::new(),
                    text: String::new(),
                    bytes: 0,
                }),
            }
        }
        self.enforce()
    }

    fn end(&mut self, out: &mut VecDeque<(usize, Match)>) {
        let level = self.frames.len();
        let mut keep = Vec::with_capacity(self.caps.len());
        for mut c in self.caps.drain(..) {
            match c.kind {
                CapKind::Full { deferred } => {
                    let el = c.stack.pop().unwrap();
                    match c.stack.last_mut() {
                        Some(parent) => {
                            parent.children.push(Node::Element(el));
                            keep.push(c);
                        }
                        None => {
                            self.total = self.total.saturating_sub(c.bytes);
                            match deferred {
                                None => out.push_back((c.sel, Match::Element(el))),
                                Some(i) => finish_deferred(&self.sels[c.sel], c.sel, i, el, out),
                            }
                        }
                    }
                }
                CapKind::Text if c.level == level => {
                    self.total = self.total.saturating_sub(c.bytes);
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

/// A candidate element has been read in full: check its child predicates and, if they hold,
/// report it (last step) or look for the rest of the path inside it.
fn finish_deferred(
    sel: &Selector,
    si: usize,
    i: usize,
    el: Element,
    out: &mut VecDeque<(usize, Match)>,
) {
    if !sel.steps[i].deferred_hold(&el) {
        return;
    }
    if i + 1 < sel.steps.len() {
        let rest = Selector {
            steps: sel.steps[i + 1..].to_vec(),
            output: sel.output.clone(),
            source: sel.source.clone(),
        };
        out.extend(rest.select_children(&el).into_iter().map(|m| (si, m)));
        return;
    }
    match &sel.output {
        Output::Element => out.push_back((si, Match::Element(el))),
        Output::Attr(a) => {
            if let Some(v) = el.attr(a) {
                out.push_back((si, Match::Attr(v.to_string())));
            }
        }
        Output::Text => {
            let t = el.direct_text();
            if !t.is_empty() {
                out.push_back((si, Match::Text(t)));
            }
        }
    }
}
