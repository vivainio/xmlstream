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

use std::{collections::VecDeque, io::Read, sync::Arc};

use crate::{
    element::{Element, Node},
    error::{Error, Result},
    reader::{Event, EventRef, EventSource, IterSource, Reader},
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

    /// Like [`to_text`](Self::to_text), without copying when the match already holds a string.
    pub fn into_text(self) -> String {
        match self {
            Match::Element(e) => e.text(),
            Match::Attr(s) | Match::Text(s) => s,
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

/// Called with the selector index and the start tag's attributes when a selector matches an
/// element; answering `false` means the element is not buffered.
pub(crate) type Gate<'g> = &'g mut dyn FnMut(usize, &[(String, String)]) -> bool;

fn elem_size(name: &str, attrs: &[(String, String)]) -> usize {
    name.len() + attrs.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>()
}

/// `pattern` may be a local name or a qualified name; `actual` is the qualified name in the document.
pub(crate) fn name_matches(pattern: &str, actual: &str) -> bool {
    if pattern == actual {
        return true;
    }
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
        // In a relative path `./foo` means `foo` and `.//foo` means `//foo`.
        if default_axis == Axis::Child && p.s[p.i..].starts_with(&['.', '/']) {
            p.i += 1;
        }
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

    /// For routes added while parsing: a path with no `//` is relative to the open element, so
    /// `vat` means its child `vat` (the way predicate paths read).
    pub(crate) fn parse_relative(src: &str) -> Result<Selector> {
        Self::parse_with(src, Axis::Child)
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// Streams matches out of `reader` one at a time.
    pub fn select<R: Read>(&self, reader: Reader<R>) -> Select<Reader<R>> {
        self.select_with(reader, Limits::default())
    }

    /// Like `select`, with a memory budget for buffered candidates.
    pub fn select_with<R: Read>(&self, reader: Reader<R>, limits: Limits) -> Select<Reader<R>> {
        Select(
            SelectorSet::from_selectors(vec![self.clone()])
                .limits(limits)
                .select(reader),
        )
    }

    pub fn select_str<'a>(&self, xml: &'a str) -> Select<Reader<std::io::Cursor<&'a [u8]>>> {
        self.select(Reader::from_str(xml))
    }

    /// Streams matches out of any source of events: a `Reader`, a `Vec<Event>`, another
    /// pipeline stage. The first element that opens plays the document root.
    pub fn select_events<I>(&self, events: I) -> Select<IterSource<I::IntoIter>>
    where
        I: IntoIterator<Item = Result<Event>>,
    {
        self.select_events_with(events, Limits::default())
    }

    pub fn select_events_with<I>(
        &self,
        events: I,
        limits: Limits,
    ) -> Select<IterSource<I::IntoIter>>
    where
        I: IntoIterator<Item = Result<Event>>,
    {
        Select(
            SelectorSet::from_selectors(vec![self.clone()])
                .limits(limits)
                .select_events(events),
        )
    }

    /// A push-style matcher: feed it events one at a time, as they arrive from somewhere else.
    pub fn matcher(&self) -> Matcher {
        self.matcher_with(Limits::default())
    }

    pub fn matcher_with(&self, limits: Limits) -> Matcher {
        SelectorSet::from_selectors(vec![self.clone()])
            .limits(limits)
            .matcher()
    }

    /// True when a match can only be decided after reading the element's content, so it has to
    /// be buffered.
    pub(crate) fn needs_buffering(&self) -> bool {
        self.steps.iter().any(Step::has_deferred)
    }

    pub(crate) fn yields_elements(&self) -> bool {
        self.output == Output::Element
    }

    /// Runs against the children of `e`, so `e` itself plays the document node.
    pub(crate) fn select_children(&self, e: &Element) -> Vec<Match> {
        let mut engine = Engine::new(vec![self.clone()].into(), Limits::default());
        let mut out = VecDeque::new();
        let mut feed = |ev: Event| {
            let _ = engine.feed(ev.as_ref(), &mut out, &mut allow_all);
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
        let mut engine = Engine::new(vec![self.clone()].into(), Limits::default());
        let mut out = VecDeque::new();
        e.for_each_event(&mut |ev| {
            let _ = engine.feed(ev.as_ref(), &mut out, &mut allow_all);
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

    /// Adds an already parsed selector and returns its index.
    pub(crate) fn add_selector(&mut self, sel: Selector) -> usize {
        self.sels.push(sel);
        self.sels.len() - 1
    }

    pub(crate) fn shared(&self) -> Arc<[Selector]> {
        self.sels.clone().into()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.sels.is_empty()
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

    pub fn select<R: Read>(&self, reader: Reader<R>) -> Hits<Reader<R>> {
        self.select_source(reader)
    }

    pub fn select_events<I>(&self, events: I) -> Hits<IterSource<I::IntoIter>>
    where
        I: IntoIterator<Item = Result<Event>>,
    {
        self.select_source(IterSource::new(events.into_iter()))
    }

    /// Runs over any [`EventSource`].
    pub fn select_source<S: EventSource>(&self, source: S) -> Hits<S> {
        Hits {
            events: source,
            engine: Engine::new(self.sels.clone().into(), self.limits),
            queue: VecDeque::new(),
            failed: false,
        }
    }

    pub fn matcher(&self) -> Matcher {
        Matcher {
            engine: Engine::new(self.sels.clone().into(), self.limits),
            queue: VecDeque::new(),
        }
    }
}

/// Matching driven by the caller: `feed` one event, get the matches it completes.
///
/// Use it where events arrive by push rather than by pull (a callback, a channel, another
/// parser). Hits carry the index of the selector in the
/// set; a matcher made from a single `Selector` always reports 0.
pub struct Matcher {
    engine: Engine,
    queue: VecDeque<(usize, Match)>,
}

impl Matcher {
    /// A matcher over selectors shared with others, so making one costs no copy.
    pub(crate) fn from_shared(sels: Arc<[Selector]>, limits: Limits) -> Matcher {
        Matcher {
            engine: Engine::new(sels, limits),
            queue: VecDeque::new(),
        }
    }

    /// Forgets everything fed so far, as if freshly made, and keeps the allocations.
    pub(crate) fn reset(&mut self) {
        self.engine.reset();
        self.queue.clear();
    }

    pub fn feed(&mut self, ev: &Event) -> Result<Vec<(usize, Match)>> {
        self.feed_ref(ev.as_ref())
    }

    /// Like [`feed_ref`](Self::feed_ref), handing each hit to `f` instead of collecting them.
    pub(crate) fn feed_with(
        &mut self,
        ev: EventRef<'_>,
        mut f: impl FnMut(usize, Match),
    ) -> Result<()> {
        self.engine.feed(ev, &mut self.queue, &mut allow_all)?;
        while let Some((i, m)) = self.queue.pop_front() {
            f(i, m);
        }
        Ok(())
    }

    /// Like [`feed_with`](Self::feed_with). `gate` is called as an element starts that selector `i` matches; answer `false` to not
    /// buffer it (and get no match for it), `true` to carry on as usual.
    pub(crate) fn feed_gated(
        &mut self,
        ev: EventRef<'_>,
        gate: &mut dyn FnMut(usize, &[(String, String)]) -> bool,
        mut f: impl FnMut(usize, Match),
    ) -> Result<()> {
        self.engine.feed(ev, &mut self.queue, gate)?;
        while let Some((i, m)) = self.queue.pop_front() {
            f(i, m);
        }
        Ok(())
    }

    /// Like [`feed`](Self::feed), for borrowed events (see `Reader::next_ref`).
    pub fn feed_ref(&mut self, ev: EventRef<'_>) -> Result<Vec<(usize, Match)>> {
        self.engine.feed(ev, &mut self.queue, &mut allow_all)?;
        Ok(self.queue.drain(..).collect())
    }

    /// Candidates dropped so far because of `Overflow::Skip`.
    pub fn skipped(&self) -> u64 {
        self.engine.skipped
    }
}

fn allow_all(_: usize, _: &[(String, String)]) -> bool {
    true
}

/// Iterator of `(selector index, match)`.
pub struct Hits<S> {
    events: S,
    engine: Engine,
    queue: VecDeque<(usize, Match)>,
    failed: bool,
}

impl<S: EventSource> Iterator for Hits<S> {
    type Item = Result<(usize, Match)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(h) = self.queue.pop_front() {
                return Some(Ok(h));
            }
            if self.failed {
                return None;
            }
            match self.events.pull() {
                Ok(Some(ev)) => {
                    if let Err(e) = self.engine.feed(ev, &mut self.queue, &mut allow_all) {
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
pub struct Select<S>(Hits<S>);

impl<S: EventSource> Iterator for Select<S> {
    type Item = Result<Match>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|r| r.map(|(_, m)| m))
    }
}

#[derive(Default)]
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
    sels: Arc<[Selector]>,
    frames: Vec<Frame>,
    /// Popped frames, kept so their vectors are reused instead of reallocated.
    spare: Vec<Frame>,
    caps: Vec<Capture>,
    needs_pos: bool,
    limits: Limits,
    total: usize,
    pub(crate) skipped: u64,
}

impl Engine {
    pub(crate) fn new(sels: Arc<[Selector]>, limits: Limits) -> Self {
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
            spare: Vec::new(),
            caps: Vec::new(),
            needs_pos,
            limits,
            total: 0,
            skipped: 0,
        }
    }

    pub(crate) fn feed(
        &mut self,
        ev: EventRef<'_>,
        out: &mut VecDeque<(usize, Match)>,
        gate: Gate,
    ) -> Result<()> {
        match ev {
            EventRef::Start { name, attrs } => self.start(name, attrs, out, gate)?,
            EventRef::End { .. } => self.end(out),
            EventRef::Text(t) | EventRef::CData(t) => {
                let cdata = matches!(ev, EventRef::CData(_));
                let level = self.frames.len();
                let mut added = 0;
                for c in &mut self.caps {
                    match c.kind {
                        CapKind::Full { .. } => {
                            let n = if cdata {
                                Node::CData(t.to_string())
                            } else {
                                Node::Text(t.to_string())
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
            EventRef::Comment(t) => {
                let mut added = 0;
                for c in &mut self.caps {
                    if let CapKind::Full { .. } = c.kind {
                        c.stack
                            .last_mut()
                            .unwrap()
                            .children
                            .push(Node::Comment(t.to_string()));
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

    /// Back to the state of a fresh engine, keeping allocations for reuse.
    pub(crate) fn reset(&mut self) {
        while self.frames.len() > 1 {
            let f = self.frames.pop().unwrap();
            self.spare.push(f);
        }
        let root = &mut self.frames[0];
        root.counts.clear();
        for st in &mut root.states {
            st.clear();
            st.push(0);
        }
        self.caps.clear();
        self.total = 0;
        self.skipped = 0;
    }

    /// Applies the memory budget: fails, or drops the largest offending candidate.
    fn enforce(&mut self) -> Result<()> {
        if self.caps.is_empty() {
            return Ok(());
        }
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
        let mut frame = self.spare.pop().unwrap_or_default();
        frame.counts.clear();
        frame.states.resize_with(self.sels.len(), Vec::new);
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

        let mut hits = Vec::new();
        let mut candidates = Vec::new();
        for (si, sel) in self.sels.iter().enumerate() {
            let ns = &mut frame.states[si];
            ns.clear();
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
            if ns.len() > 1 {
                ns.sort_unstable();
                ns.dedup();
            }
        }

        let size = elem_size(name, attrs);
        for c in &mut self.caps {
            if let CapKind::Full { .. } = c.kind {
                c.stack.push(Element::with_attrs(name, attrs.to_vec()));
                c.bytes += size;
                self.total += size;
            }
        }
        self.frames.push(frame);
        let level = self.frames.len();
        for (si, step) in candidates {
            if !gate(si, attrs) {
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
            if !gate(si, attrs) {
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
        let mut i = 0;
        while i < self.caps.len() {
            let c = &mut self.caps[i];
            match c.kind {
                CapKind::Full { deferred } => {
                    let el = c.stack.pop().unwrap();
                    if let Some(parent) = c.stack.last_mut() {
                        parent.children.push(Node::Element(el));
                        i += 1;
                        continue;
                    }
                    let c = self.caps.remove(i);
                    self.total = self.total.saturating_sub(c.bytes);
                    match deferred {
                        None => out.push_back((c.sel, Match::Element(el))),
                        Some(d) => finish_deferred(&self.sels[c.sel], c.sel, d, el, out),
                    }
                }
                CapKind::Text if c.level == level => {
                    let c = self.caps.remove(i);
                    self.total = self.total.saturating_sub(c.bytes);
                    if !c.text.is_empty() {
                        out.push_back((c.sel, Match::Text(c.text)));
                    }
                }
                CapKind::Text => i += 1,
            }
        }
        if let Some(f) = self.frames.pop() {
            self.spare.push(f);
        }
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
