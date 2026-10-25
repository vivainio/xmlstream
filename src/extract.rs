//! Pull fields out of records without writing callbacks.
//!
//! A rule says which elements are records and which fields to take from each; the result is a
//! plain [`Record`] per element. Fields are filled while the element streams past, so only the
//! values you asked for are kept, never the element's subtree (paths that end in `@attr` or
//! `text()` buffer nothing at all).
//!
//! Build rules in Rust:
//!
//! ```
//! use xmlstream::{Extract, Reader};
//!
//! let ex = Extract::new()
//!     .rule("//order[@status='open']")
//!         .field("id", "@id")
//!         .field("total", "total/text()").number()
//!         .field("items", "./items/item").nested(|i| i
//!             .field("sku", "@sku")
//!             .field("qty", "@qty").int()).many()
//!     .build()?;
//! let xml = "<r><order id='1' status='open'><total>9.5</total>\
//!     <items><item sku='a' qty='2'/><item sku='b' qty='1'/></items></order></r>";
//! for rec in ex.run(Reader::from_str(xml)) {
//!     let rec = rec?;
//!     assert_eq!(rec["id"].as_str(), Some("1"));
//!     for item in rec["items"].as_list().unwrap() {
//!         println!("{} x{}", item["sku"], item["qty"]);       // parent in the outer loop
//!     }
//! }
//! # Ok::<(), xmlstream::Error>(())
//! ```
//!
//! or write them as text with [`Extract::parse`]:
//!
//! ```text
//! open = //order[@status='open'] {     # optional "name =" before the path
//!     id    = @id                       # attribute of the record element itself
//!     total = total/text() as number    # text, int, number or bool
//!     skus  = sku/text() *              # * zero or more, + one or more, ? optional
//!     kind  = "open"                    # a quoted value is a constant in every record
//!     items = ./items/item {            # a block makes a nested record per child element
//!         sku = @sku
//!         qty = @qty as int
//!     } *                               # same markers: none = exactly one, ? * +
//! }
//! ```
//!
//! Field paths are relative to the record element: `sku` and `./sku` are children, `//sku` and
//! `.//sku` any descendant, `@id` its own attribute (fields are separated by a newline or `;`),
//! `text()` its own text and `.` all its text. When a one-valued field matches several times,
//! the first match wins.

use std::{collections::VecDeque, io::Read, sync::Arc};

use crate::{
    error::{Error, Result},
    path::{Engine, Limits, Matcher, Selector, SelectorSet},
    reader::{EventRef, Reader},
    value::{Record, Value},
};

/// Records are filled while they stream past, so a rule cannot depend on content not read yet.
fn check_stream(sel: &Selector) -> Result<()> {
    if !sel.yields_elements() {
        return Err(Error::Path(format!(
            "{:?}: a rule's path must select elements (not @attr or text())",
            sel.as_str()
        )));
    }
    if sel.needs_buffering() {
        return Err(Error::Path(format!(
            "{:?}: a rule's path cannot use child-content predicates; they would need the whole element buffered",
            sel.as_str()
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Text,
    Int,
    Number,
    Bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Card {
    /// Exactly one; a missing value is an error.
    One,
    /// Zero or one: `Value::Null` when missing.
    Optional,
    /// Zero or more, as a list.
    Many,
    /// One or more, as a list.
    OneOrMore,
}

struct FieldSpec {
    name: String,
    path: String,
    ty: Type,
    card: Card,
    /// A fixed value instead of something read from the document.
    constant: Option<Value>,
    /// Fields of a child record; the field then holds a record, or a list of them.
    nested: Option<Vec<FieldSpec>>,
}

impl FieldSpec {
    fn new(name: &str, path: &str) -> Self {
        FieldSpec {
            name: name.to_string(),
            path: path.to_string(),
            ty: Type::Text,
            card: Card::One,
            constant: None,
            nested: None,
        }
    }
}

struct RuleSpec {
    name: Option<String>,
    path: String,
    fields: Vec<FieldSpec>,
}

/// Collects rules. Errors are held until [`build`](Self::build), so calls chain without `?`.
#[derive(Default)]
pub struct ExtractBuilder {
    rules: Vec<RuleSpec>,
    errors: Vec<String>,
}

impl ExtractBuilder {
    /// A builder for the fields of a nested record: one implicit rule.
    fn inner() -> Self {
        ExtractBuilder {
            rules: vec![RuleSpec {
                name: None,
                path: String::new(),
                fields: Vec::new(),
            }],
            errors: Vec::new(),
        }
    }

    /// Starts a rule: every element matching `path` becomes a record.
    pub fn rule(mut self, path: &str) -> Self {
        self.rules.push(RuleSpec {
            name: None,
            path: path.to_string(),
            fields: Vec::new(),
        });
        self
    }

    /// Names the current rule; shows up as [`Record::rule_name`].
    pub fn name(mut self, name: &str) -> Self {
        match self.rules.last_mut() {
            Some(r) => r.name = Some(name.to_string()),
            None => self.errors.push("name() before any rule()".into()),
        }
        self
    }

    /// Adds a text field to the current rule. Refine it with `.int()`, `.optional()`, ...
    pub fn field(mut self, name: &str, path: &str) -> Self {
        match self.rules.last_mut() {
            Some(r) => r.fields.push(FieldSpec::new(name, path)),
            None => self
                .errors
                .push(format!("field {name:?} before any rule()")),
        }
        self
    }

    /// Adds a field with a fixed value to every record of the current rule, for example to tell
    /// rules apart: `.constant("kind", "refund")`.
    pub fn constant(mut self, name: &str, value: impl Into<Value>) -> Self {
        match self.rules.last_mut() {
            Some(r) => {
                let mut f = FieldSpec::new(name, "");
                f.constant = Some(value.into());
                r.fields.push(f);
            }
            None => self
                .errors
                .push(format!("constant {name:?} before any rule()")),
        }
        self
    }

    /// Makes the last field a nested record: its path selects the child elements, and `fields`
    /// says what to take from each of them (paths relative to that child). Combine with
    /// `.many()` for a list of children. A parent's fields stay in scope in the outer loop, so
    /// `for item in rec["items"].as_list()` reads naturally.
    pub fn nested(mut self, fields: impl FnOnce(ExtractBuilder) -> ExtractBuilder) -> Self {
        let mut inner = fields(ExtractBuilder::inner());
        let spec = if inner.rules.len() == 1 {
            Some(inner.rules.pop().unwrap().fields)
        } else {
            inner
                .errors
                .push("a nested record has no rule(); use field()".into());
            None
        };
        self.errors.append(&mut inner.errors);
        if let (Some(f), Some(fields)) = (self.last_field("nested"), spec) {
            f.nested = Some(fields);
        }
        self
    }

    fn last_field(&mut self, what: &str) -> Option<&mut FieldSpec> {
        let f = self.rules.last_mut().and_then(|r| r.fields.last_mut());
        if f.is_none() {
            self.errors.push(format!("{what} before any field()"));
        }
        f
    }

    fn typed(mut self, ty: Type) -> Self {
        if let Some(f) = self.last_field("type") {
            f.ty = ty;
        }
        self
    }

    fn card(mut self, card: Card) -> Self {
        if let Some(f) = self.last_field("cardinality") {
            f.card = card;
        }
        self
    }

    pub fn text(self) -> Self {
        self.typed(Type::Text)
    }
    pub fn int(self) -> Self {
        self.typed(Type::Int)
    }
    pub fn number(self) -> Self {
        self.typed(Type::Number)
    }
    pub fn bool(self) -> Self {
        self.typed(Type::Bool)
    }
    /// The last field may be missing (`Value::Null`).
    pub fn optional(self) -> Self {
        self.card(Card::Optional)
    }
    /// The last field collects every match into a list (possibly empty).
    pub fn many(self) -> Self {
        self.card(Card::Many)
    }
    /// Like `many`, but at least one match is required.
    pub fn one_or_more(self) -> Self {
        self.card(Card::OneOrMore)
    }

    pub fn build(self) -> Result<Extract> {
        if let Some(e) = self.errors.into_iter().next() {
            return Err(Error::Path(e));
        }
        if self.rules.is_empty() {
            return Err(Error::Path("no rules".into()));
        }
        let mut blocks = Vec::new();
        let mut rules = Vec::new();
        let mut sels = Vec::new();
        for (ri, spec) in self.rules.into_iter().enumerate() {
            let label = spec.name.clone().unwrap_or_else(|| format!("rule {ri}"));
            let sel =
                Selector::parse(&spec.path).map_err(|e| Error::Path(format!("{label}: {e}")))?;
            check_stream(&sel).map_err(|e| Error::Path(format!("{label}: {e}")))?;
            let block = compile(&label, spec.fields, &mut blocks)?;
            sels.push(sel);
            rules.push(Rule {
                name: spec.name.map(Arc::from),
                block,
            });
        }
        Ok(Extract {
            rules,
            blocks,
            sels,
        })
    }
}

/// Compiles a list of fields into a block (and, first, the blocks of its nested records).
fn compile(label: &str, specs: Vec<FieldSpec>, blocks: &mut Vec<Block>) -> Result<usize> {
    if specs.is_empty() {
        return Err(Error::Path(format!("{label}: no fields")));
    }
    let mut fieldset = SelectorSet::new();
    let mut owners: Vec<Option<usize>> = Vec::new();
    let mut fields: Vec<Field> = Vec::new();
    for f in specs {
        if fields.iter().any(|g| &*g.name == f.name.as_str()) {
            return Err(Error::Path(format!(
                "{label}: duplicate field {:?}",
                f.name
            )));
        }
        let fail = |e: Error| Error::Path(format!("{label}: field {:?}: {e}", f.name));
        let source = if let Some(v) = f.constant {
            Source::Const(v)
        } else if let Some(inner) = f.nested {
            let sel = Selector::parse_relative(&f.path).map_err(fail)?;
            check_stream(&sel).map_err(fail)?;
            let block = compile(&format!("{label}.{}", f.name), inner, blocks)?;
            owners.push(Some(fields.len()));
            fieldset.add_selector(sel);
            Source::Nested(block)
        } else {
            match own_form(&f.path) {
                "." => Source::AllText,
                "text()" => Source::OwnText,
                p if p.starts_with('@') && ident_ok(&p[1..]) => Source::Attr(p[1..].to_string()),
                p => {
                    owners.push(None);
                    Source::Path(fieldset.add_selector(Selector::parse_relative(p).map_err(fail)?))
                }
            }
        };
        fields.push(Field {
            name: f.name.into(),
            source,
            ty: f.ty,
            card: f.card,
        });
    }
    blocks.push(Block {
        label: label.to_string(),
        shared: (!fieldset.is_empty()).then(|| fieldset.shared()),
        owners,
        needs_attrs: fields.iter().any(|f| matches!(f.source, Source::Attr(_))),
        has_nested: fields.iter().any(|f| matches!(f.source, Source::Nested(_))),
        needs_own_text: fields.iter().any(|f| matches!(f.source, Source::OwnText)),
        needs_all_text: fields.iter().any(|f| matches!(f.source, Source::AllText)),
        fields,
    });
    Ok(blocks.len() - 1)
}

enum Source {
    Const(Value),
    Attr(String),
    OwnText,
    AllText,
    /// Index into the block's field selectors.
    Path(usize),
    /// A child record: the child's block.
    Nested(usize),
}

struct Field {
    name: Arc<str>,
    source: Source,
    ty: Type,
    card: Card,
}

/// The fields of one kind of record: a rule's, or a nested record's.
struct Block {
    /// For error messages: `open`, `open.items`.
    label: String,
    fields: Vec<Field>,
    /// The field selectors, shared by every record's matcher.
    shared: Option<Arc<[Selector]>>,
    /// Per selector: the nested field it starts a child record for, if any.
    owners: Vec<Option<usize>>,
    needs_attrs: bool,
    has_nested: bool,
    needs_own_text: bool,
    needs_all_text: bool,
}

struct Rule {
    name: Option<Arc<str>>,
    block: usize,
}

pub struct Extract {
    rules: Vec<Rule>,
    blocks: Vec<Block>,
    sels: Vec<Selector>,
}

impl Extract {
    pub fn new() -> ExtractBuilder {
        ExtractBuilder::default()
    }

    /// Parses the text form of the rules (see the module docs).
    pub fn parse(src: &str) -> Result<Extract> {
        dsl(src)?.build()
    }

    pub fn run<R: Read>(&self, reader: Reader<R>) -> Records<'_, R> {
        Records {
            ex: self,
            reader,
            engine: Engine::new(self.sels.clone().into(), Limits::default()),
            pools: self.blocks.iter().map(|_| Vec::new()).collect(),
            live: Vec::new(),
            vals: Vec::new(),
            queue: VecDeque::new(),
            done: false,
        }
    }

    pub fn run_str<'a>(&'a self, xml: &'a str) -> Records<'a, std::io::Cursor<&'a [u8]>> {
        self.run(Reader::from_str(xml))
    }
}

/// A record being filled while its element streams past.
struct Live {
    block: usize,
    /// Index of the top-level rule this record belongs to.
    rule: usize,
    /// For a nested record: the parent's field it fills.
    slot: usize,
    /// Nesting inside the record element; 0 until its start tag has been seen.
    cur: usize,
    attrs: Vec<(String, String)>,
    /// Live entries of `attrs`; the rest are spare strings kept for reuse.
    n_attrs: usize,
    own_text: String,
    all_text: String,
    matcher: Option<Matcher>,
    /// Per field selector: the values it matched so far.
    found: Vec<Vec<String>>,
    /// Per field: child records finished so far.
    nested: Vec<Vec<Value>>,
    /// Child records still being filled. Boxed, like every `Live` that moves between lists,
    /// because the struct is large and records come and go constantly.
    children: Vec<Box<Live>>,
}

/// Iterator of records, one per matched element. After an error it ends.
pub struct Records<'e, R: Read> {
    ex: &'e Extract,
    reader: Reader<R>,
    engine: Engine,
    /// Per block: state of finished records, kept so its buffers are reused.
    pools: Vec<Vec<Box<Live>>>,
    live: Vec<Box<Live>>,
    /// Scratch space for one field's raw values.
    vals: Vec<String>,
    queue: VecDeque<Record>,
    done: bool,
}

fn new_live(
    ex: &Extract,
    pool: &mut Vec<Box<Live>>,
    block: usize,
    rule: usize,
    attrs: &[(String, String)],
) -> Box<Live> {
    let b = &ex.blocks[block];
    let mut l = pool.pop().unwrap_or_else(|| {
        Box::new(Live {
            block,
            rule,
            slot: 0,
            cur: 0,
            attrs: Vec::new(),
            n_attrs: 0,
            own_text: String::new(),
            all_text: String::new(),
            matcher: b
                .shared
                .as_ref()
                .map(|s| Matcher::from_shared(s.clone(), Limits::default())),
            found: vec![Vec::new(); b.shared.as_ref().map_or(0, |s| s.len())],
            nested: (0..b.fields.len()).map(|_| Vec::new()).collect(),
            children: Vec::new(),
        })
    });
    l.rule = rule;
    l.cur = 0;
    l.n_attrs = 0;
    if b.needs_attrs {
        for (k, v) in attrs {
            if let Some(slot) = l.attrs.get_mut(l.n_attrs) {
                slot.0.clear();
                slot.0.push_str(k);
                slot.1.clear();
                slot.1.push_str(v);
            } else {
                l.attrs.push((k.clone(), v.clone()));
            }
            l.n_attrs += 1;
        }
    }
    l
}

/// Sends one event to a record (and, first, to its child records). True when the record's own
/// end tag has just been seen.
fn drive(
    ex: &Extract,
    pools: &mut [Vec<Box<Live>>],
    vals: &mut Vec<String>,
    l: &mut Live,
    ev: EventRef<'_>,
) -> Result<bool> {
    let block = &ex.blocks[l.block];
    let (depth, next) = match ev {
        EventRef::Start { .. } => (l.cur, l.cur + 1),
        EventRef::End { .. } => (l.cur - 1, l.cur - 1),
        _ => (l.cur, l.cur),
    };
    l.cur = next;

    // children that were already open see the event before anything new can start
    let mut i = 0;
    while i < l.children.len() {
        if drive(ex, pools, vals, &mut l.children[i], ev)? {
            let mut c = l.children.remove(i);
            let name = block.fields[c.slot].name.clone();
            let rec = finish(ex, &mut c, vals, Some(name))?;
            l.nested[c.slot].push(Value::Record(rec));
            recycle(pools, c);
        } else {
            i += 1;
        }
    }

    if let EventRef::Text(t) | EventRef::CData(t) = ev {
        if block.needs_all_text {
            l.all_text.push_str(t);
        }
        if block.needs_own_text && depth == 1 {
            l.own_text.push_str(t);
        }
    }
    if depth >= 1 {
        if let (Some(m), false) = (&mut l.matcher, block.has_nested) {
            // no child records to start: the plain, cheaper path
            let found = &mut l.found;
            m.feed_with(ev, |idx, hit| found[idx].push(hit.into_text()))?;
        } else if let Some(m) = &mut l.matcher {
            let (found, rule) = (&mut l.found, l.rule);
            let mut spawned: Vec<Box<Live>> = Vec::new();
            m.feed_gated(
                ev,
                &mut |si, attrs| match block.owners[si] {
                    Some(fi) => {
                        let Source::Nested(cb) = &block.fields[fi].source else {
                            unreachable!("owners point at nested fields")
                        };
                        let mut c = new_live(ex, &mut pools[*cb], *cb, rule, attrs);
                        c.slot = fi;
                        spawned.push(c);
                        false // the child is filled from the stream, never buffered
                    }
                    None => true,
                },
                |idx, hit| found[idx].push(hit.into_text()),
            )?;
            for c in spawned {
                l.children.push(c);
                let last = l.children.len() - 1;
                drive(ex, pools, vals, &mut l.children[last], ev)?;
            }
        }
    }
    Ok(next == 0)
}

/// Gives a finished record's buffers back for the next one.
fn recycle(pools: &mut [Vec<Box<Live>>], mut l: Box<Live>) {
    l.own_text.clear();
    l.all_text.clear();
    for f in &mut l.found {
        f.clear();
    }
    for n in &mut l.nested {
        n.clear();
    }
    l.children.clear();
    if let Some(m) = &mut l.matcher {
        m.reset();
    }
    pools[l.block].push(l);
}

impl<R: Read> Records<'_, R> {
    fn step(&mut self) -> Result<bool> {
        let Some(ev) = self.reader.next_ref()? else {
            return Ok(false);
        };
        let (ex, pools, live) = (self.ex, &mut self.pools, &mut self.live);
        self.engine
            .feed(ev, &mut VecDeque::new(), &mut |si, attrs| {
                let b = ex.rules[si].block;
                live.push(new_live(ex, &mut pools[b], b, si, attrs));
                false // never buffer: the fields are collected as events pass
            })?;

        let mut i = 0;
        while i < self.live.len() {
            if drive(
                self.ex,
                &mut self.pools,
                &mut self.vals,
                &mut self.live[i],
                ev,
            )? {
                let mut l = self.live.remove(i);
                let name = self.ex.rules[l.rule].name.clone();
                let rec = finish(self.ex, &mut l, &mut self.vals, name)?;
                recycle(&mut self.pools, l);
                self.queue.push_back(rec);
            } else {
                i += 1;
            }
        }
        Ok(true)
    }
}

impl<R: Read> Iterator for Records<'_, R> {
    type Item = Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(r) = self.queue.pop_front() {
                return Some(Ok(r));
            }
            if self.done {
                return None;
            }
            match self.step() {
                Ok(true) => {}
                Ok(false) => return None,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

fn finish(
    ex: &Extract,
    l: &mut Live,
    vals: &mut Vec<String>,
    rule_name: Option<Arc<str>>,
) -> Result<Record> {
    let block = &ex.blocks[l.block];
    let mut fields = Vec::with_capacity(block.fields.len());
    for (fi, f) in block.fields.iter().enumerate() {
        vals.clear();
        let missing = || Error::Extract(format!("{}: field {:?} not found", block.label, &*f.name));
        let value = match &f.source {
            Source::Const(v) => {
                fields.push((f.name.clone(), v.clone()));
                continue;
            }
            Source::Nested(_) => {
                let mut recs = std::mem::take(&mut l.nested[fi]);
                match f.card {
                    Card::One => {
                        if recs.is_empty() {
                            return Err(missing());
                        }
                        recs.swap_remove(0)
                    }
                    Card::Optional => recs.into_iter().next().unwrap_or(Value::Null),
                    Card::OneOrMore | Card::Many => {
                        if f.card == Card::OneOrMore && recs.is_empty() {
                            return Err(missing());
                        }
                        Value::List(recs)
                    }
                }
            }
            source => {
                match source {
                    Source::Attr(a) => {
                        if let Some((_, v)) = l.attrs[..l.n_attrs].iter().find(|(k, _)| k == a) {
                            vals.push(v.clone());
                        }
                    }
                    Source::OwnText => {
                        if !l.own_text.is_empty() {
                            vals.push(std::mem::take(&mut l.own_text));
                        }
                    }
                    Source::AllText => {
                        if !l.all_text.is_empty() {
                            vals.push(std::mem::take(&mut l.all_text));
                        }
                    }
                    Source::Path(i) => vals.extend(l.found[*i].drain(..)),
                    Source::Const(_) | Source::Nested(_) => unreachable!(),
                }
                match f.card {
                    Card::One => {
                        if vals.is_empty() {
                            return Err(missing());
                        }
                        convert(f, vals.swap_remove(0), &block.label)?
                    }
                    Card::Optional => match vals.drain(..).next() {
                        Some(v) => convert(f, v, &block.label)?,
                        None => Value::Null,
                    },
                    Card::OneOrMore | Card::Many => {
                        if f.card == Card::OneOrMore && vals.is_empty() {
                            return Err(missing());
                        }
                        Value::List(
                            vals.drain(..)
                                .map(|v| convert(f, v, &block.label))
                                .collect::<Result<_>>()?,
                        )
                    }
                }
            }
        };
        fields.push((f.name.clone(), value));
    }
    Ok(Record {
        rule: l.rule,
        rule_name,
        fields,
    })
}

fn convert(f: &Field, raw: String, label: &str) -> Result<Value> {
    let bad = |what: &str| {
        Error::Extract(format!(
            "{label}: field {:?}: {raw:?} is not {what}",
            &*f.name
        ))
    };
    let t = raw.trim();
    Ok(match f.ty {
        Type::Text => Value::Text(raw),
        Type::Int => Value::Int(t.parse().map_err(|_| bad("an integer"))?),
        Type::Number => match t.parse::<f64>() {
            Ok(n) if n.is_finite() => Value::Number(n),
            _ => return Err(bad("a number")),
        },
        Type::Bool => match t.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Value::Bool(true),
            "false" | "0" | "no" => Value::Bool(false),
            _ => return Err(bad("a boolean")),
        },
    })
}

// ---- text form ----

struct P<'s> {
    s: &'s str,
    i: usize,
}

fn derr(src: &str, at: usize, msg: &str) -> Error {
    let line = src[..at.min(src.len())].matches('\n').count() + 1;
    Error::Path(format!("rules, line {line}: {msg}"))
}

impl<'s> P<'s> {
    fn rest(&self) -> &'s str {
        &self.s[self.i..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    /// Skips whitespace and `# comments`; `;` separators too when `semis`.
    fn skip(&mut self, semis: bool) {
        while let Some(c) = self.peek() {
            if c == '#' {
                while self.peek().is_some_and(|c| c != '\n') {
                    self.i += 1;
                }
            } else if c.is_whitespace() || (semis && c == ';') {
                self.i += c.len_utf8();
            } else {
                break;
            }
        }
    }

    /// Text up to (not including) the first char in `stops` outside quotes and brackets.
    fn until(&mut self, stops: &[char]) -> &'s str {
        let start = self.i;
        let (mut quote, mut depth) = (None, 0usize);
        while let Some(c) = self.peek() {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), _) => {}
                (None, '\'' | '"') => quote = Some(c),
                (None, '[' | '(') => depth += 1,
                (None, ']' | ')') => depth = depth.saturating_sub(1),
                (None, c) if depth == 0 && stops.contains(&c) => break,
                _ => {}
            }
            self.i += c.len_utf8();
        }
        &self.s[start..self.i]
    }
}

/// `'text'` or `"text"` as a whole: the text inside.
fn quoted(s: &str) -> Option<&str> {
    let s = s.trim();
    let q = s.chars().next().filter(|c| matches!(c, '\'' | '"'))?;
    s.strip_prefix(q)?
        .strip_suffix(q)
        .filter(|inner| !inner.contains(q))
}

/// `./text()` and `./@id` mean the same as `text()` and `@id`: the element's own text and
/// attribute (XPath's context node), not those of its children.
fn own_form(path: &str) -> &str {
    let p = path.trim();
    match p.strip_prefix("./") {
        Some(rest) if rest == "text()" || (rest.starts_with('@') && ident_ok(&rest[1..])) => rest,
        _ => p,
    }
}

fn ident_ok(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Splits `name = path` into its parts; a path that merely contains `=` is left whole.
fn split_name(head: &str) -> (Option<&str>, &str) {
    if let Some(eq) = head.find('=') {
        let name = head[..eq].trim();
        if ident_ok(name) {
            return (Some(name), head[eq + 1..].trim());
        }
    }
    (None, head.trim())
}

/// Peels `as TYPE` and a `? * +` marker off the end of a field expression.
fn split_modifiers(expr: &str) -> (&str, Option<&str>, Option<&str>) {
    let mut e = expr.trim_end();
    let mut marker = None;
    if let Some((rest, last)) = e.rsplit_once(char::is_whitespace) {
        if matches!(last, "?" | "*" | "+") {
            marker = Some(last);
            e = rest.trim_end();
        }
    }
    // `?` and `+` can never end a path, so they may be glued on: `vat/text()?`
    if marker.is_none() && e.len() > 1 && (e.ends_with('?') || e.ends_with('+')) {
        marker = Some(&e[e.len() - 1..]);
        e = e[..e.len() - 1].trim_end();
    }
    let mut ty = None;
    if let Some((rest, last)) = e.rsplit_once(char::is_whitespace) {
        if let Some((rest2, kw)) = rest.trim_end().rsplit_once(char::is_whitespace) {
            if kw == "as" {
                ty = Some(last);
                e = rest2.trim_end();
            }
        }
    }
    (e, ty, marker)
}

fn dsl(src: &str) -> Result<ExtractBuilder> {
    let mut b = Extract::new();
    let mut p = P { s: src, i: 0 };
    loop {
        p.skip(true);
        if p.peek().is_none() {
            return Ok(b);
        }
        let head_at = p.i;
        let head = p.until(&['{']);
        if p.peek() != Some('{') {
            return Err(derr(src, head_at, "expected `{` after the rule's path"));
        }
        p.i += 1;
        let (name, path) = split_name(head);
        if path.is_empty() {
            return Err(derr(src, head_at, "a rule needs a path"));
        }
        b = b.rule(path);
        if let Some(n) = name {
            b = b.name(n);
        }
        let fields = parse_fields(&mut p, src)?;
        b.rules.last_mut().unwrap().fields = fields;
    }
}

/// The fields of one block, up to and including its closing `}`.
fn parse_fields(p: &mut P<'_>, src: &str) -> Result<Vec<FieldSpec>> {
    let mut out = Vec::new();
    loop {
        p.skip(true);
        match p.peek() {
            Some('}') => {
                p.i += 1;
                return Ok(out);
            }
            None => return Err(derr(src, p.i, "missing `}` at the end of the rule")),
            _ => {}
        }
        let at = p.i;
        let fname = p.until(&['=', '\n', '}']).trim();
        if p.peek() != Some('=') || !ident_ok(fname) {
            return Err(derr(src, at, "expected `field = path`"));
        }
        p.i += 1;
        let expr_at = p.i;
        let expr = p.until(&['\n', ';', '}', '#', '{']);

        if p.peek() == Some('{') {
            // a nested record: `name = path { fields } [marker]`
            let path = expr.trim();
            if path.is_empty() {
                return Err(derr(src, expr_at, "a nested record needs a path"));
            }
            p.i += 1;
            let inner = parse_fields(p, src)?;
            while matches!(p.peek(), Some(' ' | '\t')) {
                p.i += 1;
            }
            let mut f = FieldSpec::new(fname, path);
            f.nested = Some(inner);
            if let Some(m @ ('?' | '*' | '+')) = p.peek() {
                p.i += 1;
                f.card = match m {
                    '?' => Card::Optional,
                    '*' => Card::Many,
                    _ => Card::OneOrMore,
                };
            }
            out.push(f);
            continue;
        }

        let (path, ty, marker) = split_modifiers(expr);
        if path.is_empty() {
            return Err(derr(src, expr_at, "a field needs a path"));
        }
        let mut f = FieldSpec::new(fname, path);
        if let Some(lit) = quoted(path) {
            f.constant = Some(match ty {
                None | Some("text") => Value::Text(lit.to_string()),
                Some("int") => Value::Int(
                    lit.trim()
                        .parse()
                        .map_err(|_| derr(src, expr_at, &format!("{lit:?} is not an integer")))?,
                ),
                Some("number") => Value::Number(
                    lit.trim()
                        .parse()
                        .map_err(|_| derr(src, expr_at, &format!("{lit:?} is not a number")))?,
                ),
                Some("bool") => Value::Bool(match lit.trim() {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(derr(
                            src,
                            expr_at,
                            "a bool constant is \"true\" or \"false\"",
                        ))
                    }
                }),
                Some(other) => return Err(derr(src, expr_at, &format!("unknown type {other:?}"))),
            });
        } else {
            f.ty = match ty {
                None | Some("text") => Type::Text,
                Some("int") => Type::Int,
                Some("number") => Type::Number,
                Some("bool") => Type::Bool,
                Some(other) => {
                    return Err(derr(
                        src,
                        expr_at,
                        &format!("unknown type {other:?} (use text, int, number or bool)"),
                    ))
                }
            };
            f.card = match marker {
                Some("?") => Card::Optional,
                Some("*") => Card::Many,
                Some("+") => Card::OneOrMore,
                _ => Card::One,
            };
        }
        out.push(f);
    }
}
