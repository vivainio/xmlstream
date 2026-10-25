//! A pull parser over any `Read`. Memory use is bounded by a single event, not by the document.
//! Adjacent text and entity references are merged into one text run, DOCTYPE is rejected
//! (no entity expansion attacks), and total input size and nesting depth are capped.
//!
//! Two ways to read:
//!
//! - iterate the reader for owned [`Event`]s: simple, one allocation per string;
//! - call [`Reader::next_ref`] in a loop for borrowed [`EventRef`]s: no allocation in steady
//!   state, because the reader reuses its buffers. Selectors and `Extract` read this way.

use std::io::{self, BufReader, Read};

use quick_xml::{events::Event as Q, Reader as QReader};

use crate::error::{xml_err, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Start {
        name: String,
        attrs: Vec<(String, String)>,
    },
    End {
        name: String,
    },
    Text(String),
    CData(String),
    Comment(String),
}

/// An [`Event`] that borrows from the reader (or from an `Event`), so nothing is copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventRef<'a> {
    Start {
        name: &'a str,
        attrs: &'a [(String, String)],
    },
    End {
        name: &'a str,
    },
    Text(&'a str),
    CData(&'a str),
    Comment(&'a str),
}

impl Event {
    pub fn as_ref(&self) -> EventRef<'_> {
        match self {
            Event::Start { name, attrs } => EventRef::Start { name, attrs },
            Event::End { name } => EventRef::End { name },
            Event::Text(t) => EventRef::Text(t),
            Event::CData(t) => EventRef::CData(t),
            Event::Comment(t) => EventRef::Comment(t),
        }
    }
}

impl EventRef<'_> {
    /// Copies into an owned event.
    pub fn to_event(&self) -> Event {
        match *self {
            EventRef::Start { name, attrs } => Event::Start {
                name: name.to_string(),
                attrs: attrs.to_vec(),
            },
            EventRef::End { name } => Event::End {
                name: name.to_string(),
            },
            EventRef::Text(t) => Event::Text(t.to_string()),
            EventRef::CData(t) => Event::CData(t.to_string()),
            EventRef::Comment(t) => Event::Comment(t.to_string()),
        }
    }
}

/// Anything that can hand out events one at a time. `Reader` is one; so is an adapter over an
/// iterator of owned events.
pub trait EventSource {
    fn pull(&mut self) -> Result<Option<EventRef<'_>>>;
}

impl<R: Read> EventSource for Reader<R> {
    fn pull(&mut self) -> Result<Option<EventRef<'_>>> {
        self.next_ref()
    }
}

/// Adapts an iterator of owned events into an [`EventSource`].
pub struct IterSource<I> {
    it: I,
    cur: Option<Event>,
}

impl<I> IterSource<I> {
    pub fn new(it: I) -> Self {
        IterSource { it, cur: None }
    }
}

impl<I: Iterator<Item = Result<Event>>> EventSource for IterSource<I> {
    fn pull(&mut self) -> Result<Option<EventRef<'_>>> {
        match self.it.next() {
            None => Ok(None),
            Some(Err(e)) => Err(e),
            Some(Ok(ev)) => {
                self.cur = Some(ev);
                Ok(self.cur.as_ref().map(Event::as_ref))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Trim text runs and drop the ones that end up empty.
    pub trim: bool,
    /// Total input cap; guards against unbounded streams.
    pub max_bytes: u64,
    pub max_depth: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            trim: false,
            max_bytes: 4 * 1024 * 1024 * 1024,
            max_depth: 256,
        }
    }
}

struct Limited<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Limited<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Err(io::Error::other("input exceeds stream limit"));
        }
        let max = buf.len().min(self.left.min(usize::MAX as u64) as usize);
        let n = self.inner.read(&mut buf[..max])?;
        self.left -= n as u64;
        Ok(n)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Start,
    /// `<a/>`: a start whose end follows.
    StartEmpty,
    End,
    Text,
    CData,
    Comment,
    Eof,
}

/// One parsed event in reusable buffers.
struct Slot {
    kind: Kind,
    name: String,
    attrs: Vec<(String, String)>,
    /// Live entries of `attrs`; the rest are spare strings kept for reuse.
    n_attrs: usize,
    text: String,
}

impl Slot {
    fn new() -> Self {
        Slot {
            kind: Kind::Eof,
            name: String::new(),
            attrs: Vec::new(),
            n_attrs: 0,
            text: String::new(),
        }
    }
}

fn set(s: &mut String, v: &str) {
    s.clear();
    s.push_str(v);
}

#[derive(Clone, Copy)]
enum Which {
    Other,
    Run,
    Pending,
}

pub struct Reader<R: Read> {
    reader: QReader<BufReader<Limited<R>>>,
    buf: Vec<u8>,
    opts: Options,
    depth: usize,
    root_done: bool,
    /// The event just read from the parser.
    other: Slot,
    /// The merged text run that came before `other`.
    run: String,
    /// `other` has been read but not yet delivered (a text run went first).
    queued: bool,
    /// `<a/>` yields start then end; the end is owed.
    pending_end: bool,
    pending_name: String,
    done: bool,
}

impl<'a> Reader<io::Cursor<&'a [u8]>> {
    pub fn from_str(s: &'a str) -> Self {
        Reader::new(io::Cursor::new(s.as_bytes()))
    }
}

impl<R: Read> Reader<R> {
    pub fn new(r: R) -> Self {
        Self::with_options(r, Options::default())
    }

    pub fn with_options(r: R, opts: Options) -> Self {
        let mut q = QReader::from_reader(BufReader::with_capacity(
            64 * 1024,
            Limited {
                inner: r,
                left: opts.max_bytes,
            },
        ));
        q.config_mut().check_end_names = true;
        Reader {
            reader: q,
            buf: Vec::new(),
            opts,
            depth: 0,
            root_done: false,
            other: Slot::new(),
            run: String::new(),
            queued: false,
            pending_end: false,
            pending_name: String::new(),
            done: false,
        }
    }

    pub fn trim(mut self, yes: bool) -> Self {
        self.opts.trim = yes;
        self
    }

    /// Number of currently open elements.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Reads one raw parser event into `self.other`.
    fn read_raw(&mut self) -> Result<()> {
        loop {
            self.buf.clear();
            let decoder = self.reader.decoder();
            let ev = self
                .reader
                .read_event_into(&mut self.buf)
                .map_err(xml_err)?;
            let o = &mut self.other;
            match ev {
                Q::Start(ref e) | Q::Empty(ref e) => {
                    let empty = matches!(ev, Q::Empty(_));
                    set(&mut o.name, &String::from_utf8_lossy(e.name().as_ref()));
                    o.n_attrs = 0;
                    for a in e.attributes() {
                        let a = a.map_err(xml_err)?;
                        let key = String::from_utf8_lossy(a.key.as_ref());
                        let val = a.decode_and_unescape_value(decoder).map_err(xml_err)?;
                        if o.n_attrs < o.attrs.len() {
                            let slot = &mut o.attrs[o.n_attrs];
                            set(&mut slot.0, &key);
                            set(&mut slot.1, &val);
                        } else {
                            o.attrs.push((key.into_owned(), val.into_owned()));
                        }
                        o.n_attrs += 1;
                    }
                    o.kind = if empty { Kind::StartEmpty } else { Kind::Start };
                }
                Q::End(e) => {
                    set(&mut o.name, &String::from_utf8_lossy(e.name().as_ref()));
                    o.kind = Kind::End;
                }
                Q::Text(t) => {
                    set(&mut o.text, &t.decode().map_err(xml_err)?);
                    o.kind = Kind::Text;
                }
                Q::GeneralRef(r) => {
                    match r.resolve_char_ref().map_err(xml_err)? {
                        Some(c) => {
                            o.text.clear();
                            o.text.push(c);
                        }
                        None => {
                            let name = r.decode().map_err(xml_err)?;
                            let s = quick_xml::escape::resolve_predefined_entity(&name)
                                .ok_or_else(|| xml_err(format!("unknown entity &{name};")))?;
                            set(&mut o.text, s);
                        }
                    }
                    o.kind = Kind::Text;
                }
                Q::CData(c) => {
                    set(&mut o.text, &String::from_utf8_lossy(&c.into_inner()));
                    o.kind = Kind::CData;
                }
                Q::Comment(c) => {
                    set(&mut o.text, &String::from_utf8_lossy(&c.into_inner()));
                    o.kind = Kind::Comment;
                }
                Q::DocType(_) => return Err(xml_err("DOCTYPE/DTD is not allowed")),
                Q::Eof => o.kind = Kind::Eof,
                Q::Decl(_) | Q::PI(_) => continue,
            }
            return Ok(());
        }
    }

    /// Next logical event, with adjacent text/entity runs merged: either the text run, or the
    /// raw event in `self.other`.
    fn read_logical(&mut self) -> Result<Which> {
        if self.queued {
            self.queued = false;
            return Ok(Which::Other);
        }
        self.run.clear();
        loop {
            self.read_raw()?;
            if self.other.kind == Kind::Text {
                self.run.push_str(&self.other.text);
                continue;
            }
            if self.opts.trim {
                let end = self.run.trim_end().len();
                self.run.truncate(end);
                let start = self.run.len() - self.run.trim_start().len();
                self.run.drain(..start);
            }
            if self.run.is_empty() {
                return Ok(Which::Other);
            }
            self.queued = true;
            return Ok(Which::Run);
        }
    }

    fn advance(&mut self) -> Result<Option<Which>> {
        if self.pending_end {
            self.pending_end = false;
            self.depth -= 1;
            if self.depth == 0 {
                self.root_done = true;
            }
            return Ok(Some(Which::Pending));
        }
        if self.done {
            return Ok(None);
        }
        loop {
            let which = self.read_logical()?;
            let kind = match which {
                Which::Run => Kind::Text,
                _ => self.other.kind,
            };
            match kind {
                Kind::Start | Kind::StartEmpty => {
                    if self.depth == 0 && self.root_done {
                        return Err(xml_err("document has more than one root element"));
                    }
                    self.depth += 1;
                    if self.depth > self.opts.max_depth {
                        return Err(xml_err("nesting too deep"));
                    }
                    if kind == Kind::StartEmpty {
                        self.pending_end = true;
                        set(&mut self.pending_name, &self.other.name);
                    }
                    return Ok(Some(which));
                }
                Kind::End => {
                    self.depth = self.depth.saturating_sub(1);
                    if self.depth == 0 {
                        self.root_done = true;
                    }
                    return Ok(Some(which));
                }
                Kind::Text => {
                    if self.depth == 0 {
                        if self.run.trim().is_empty() {
                            continue;
                        }
                        return Err(xml_err("text outside the root element"));
                    }
                    return Ok(Some(which));
                }
                Kind::CData => {
                    if self.depth == 0 {
                        return Err(xml_err("CDATA outside the root element"));
                    }
                    return Ok(Some(which));
                }
                Kind::Comment => return Ok(Some(which)),
                Kind::Eof => {
                    self.done = true;
                    if self.depth != 0 {
                        return Err(xml_err("unexpected end of document"));
                    }
                    if !self.root_done {
                        return Err(xml_err("document has no root element"));
                    }
                    return Ok(None);
                }
            }
        }
    }

    /// The next event, borrowed from the reader's buffers: valid until the next call.
    pub fn next_ref(&mut self) -> Result<Option<EventRef<'_>>> {
        let which = match self.advance() {
            Ok(Some(w)) => w,
            Ok(None) => return Ok(None),
            Err(e) => {
                self.done = true;
                self.pending_end = false;
                return Err(e);
            }
        };
        let o = &self.other;
        Ok(Some(match which {
            Which::Pending => EventRef::End {
                name: &self.pending_name,
            },
            Which::Run => EventRef::Text(&self.run),
            Which::Other => match o.kind {
                Kind::Start | Kind::StartEmpty => EventRef::Start {
                    name: &o.name,
                    attrs: &o.attrs[..o.n_attrs],
                },
                Kind::End => EventRef::End { name: &o.name },
                Kind::CData => EventRef::CData(&o.text),
                Kind::Comment => EventRef::Comment(&o.text),
                Kind::Text | Kind::Eof => unreachable!("handled in advance"),
            },
        }))
    }

    /// The next event, owned. Same as iterating the reader.
    pub fn next_event(&mut self) -> Result<Option<Event>> {
        Ok(self.next_ref()?.map(|e| e.to_event()))
    }
}

impl<R: Read> Iterator for Reader<R> {
    type Item = Result<Event>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_event().transpose()
    }
}
