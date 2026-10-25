//! A pull parser over any `Read`. Memory use is bounded by a single event, not by the document.
//! Adjacent text and entity references are merged into one text run, DOCTYPE is rejected
//! (no entity expansion attacks), and total input size and nesting depth are capped.

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

enum Raw {
    Start {
        name: String,
        attrs: Vec<(String, String)>,
        empty: bool,
    },
    End(String),
    Text(String),
    CData(String),
    Comment(String),
    Eof,
}

pub struct Reader<R: Read> {
    reader: QReader<BufReader<Limited<R>>>,
    buf: Vec<u8>,
    opts: Options,
    depth: usize,
    root_done: bool,
    /// `<a/>` yields start then end; the end is queued here.
    pending_end: Option<String>,
    queued: Option<Raw>,
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
        let mut q = QReader::from_reader(BufReader::new(Limited {
            inner: r,
            left: opts.max_bytes,
        }));
        q.config_mut().check_end_names = true;
        Reader {
            reader: q,
            buf: Vec::new(),
            opts,
            depth: 0,
            root_done: false,
            pending_end: None,
            queued: None,
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

    fn read_raw(&mut self) -> Result<Raw> {
        loop {
            self.buf.clear();
            let decoder = self.reader.decoder();
            let ev = self
                .reader
                .read_event_into(&mut self.buf)
                .map_err(xml_err)?;
            return Ok(match ev {
                Q::Start(ref e) | Q::Empty(ref e) => {
                    let empty = matches!(ev, Q::Empty(_));
                    let mut attrs = Vec::new();
                    for a in e.attributes() {
                        let a = a.map_err(xml_err)?;
                        let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
                        let val = a
                            .decode_and_unescape_value(decoder)
                            .map_err(xml_err)?
                            .into_owned();
                        attrs.push((key, val));
                    }
                    Raw::Start {
                        name: String::from_utf8_lossy(e.name().as_ref()).into_owned(),
                        attrs,
                        empty,
                    }
                }
                Q::End(e) => Raw::End(String::from_utf8_lossy(e.name().as_ref()).into_owned()),
                Q::Text(t) => Raw::Text(t.decode().map_err(xml_err)?.into_owned()),
                Q::GeneralRef(r) => {
                    let s = match r.resolve_char_ref().map_err(xml_err)? {
                        Some(c) => c.to_string(),
                        None => {
                            let name = r.decode().map_err(xml_err)?;
                            quick_xml::escape::resolve_predefined_entity(&name)
                                .ok_or_else(|| xml_err(format!("unknown entity &{name};")))?
                                .to_string()
                        }
                    };
                    Raw::Text(s)
                }
                Q::CData(c) => Raw::CData(String::from_utf8_lossy(&c.into_inner()).into_owned()),
                Q::Comment(c) => {
                    Raw::Comment(String::from_utf8_lossy(&c.into_inner()).into_owned())
                }
                Q::DocType(_) => return Err(xml_err("DOCTYPE/DTD is not allowed")),
                Q::Eof => Raw::Eof,
                Q::Decl(_) | Q::PI(_) => continue,
            });
        }
    }

    /// Next raw event with adjacent text/entity runs merged.
    fn read_logical(&mut self) -> Result<Raw> {
        if let Some(ev) = self.queued.take() {
            return Ok(ev);
        }
        let mut text = String::new();
        loop {
            match self.read_raw()? {
                Raw::Text(t) => text.push_str(&t),
                other => {
                    if self.opts.trim {
                        text = text.trim().to_string();
                    }
                    if text.is_empty() {
                        return Ok(other);
                    }
                    self.queued = Some(other);
                    return Ok(Raw::Text(text));
                }
            }
        }
    }

    pub fn next_event(&mut self) -> Result<Option<Event>> {
        if let Some(name) = self.pending_end.take() {
            self.depth -= 1;
            if self.depth == 0 {
                self.root_done = true;
            }
            return Ok(Some(Event::End { name }));
        }
        if self.done {
            return Ok(None);
        }
        loop {
            return match self.read_logical()? {
                Raw::Start { name, attrs, empty } => {
                    if self.depth == 0 && self.root_done {
                        return Err(xml_err("document has more than one root element"));
                    }
                    self.depth += 1;
                    if self.depth > self.opts.max_depth {
                        return Err(xml_err("nesting too deep"));
                    }
                    if empty {
                        self.pending_end = Some(name.clone());
                    }
                    Ok(Some(Event::Start { name, attrs }))
                }
                Raw::End(name) => {
                    self.depth = self.depth.saturating_sub(1);
                    if self.depth == 0 {
                        self.root_done = true;
                    }
                    Ok(Some(Event::End { name }))
                }
                Raw::Text(t) => {
                    if self.depth == 0 {
                        if t.trim().is_empty() {
                            continue;
                        }
                        return Err(xml_err("text outside the root element"));
                    }
                    Ok(Some(Event::Text(t)))
                }
                Raw::CData(t) => {
                    if self.depth == 0 {
                        return Err(xml_err("CDATA outside the root element"));
                    }
                    Ok(Some(Event::CData(t)))
                }
                Raw::Comment(t) => Ok(Some(Event::Comment(t))),
                Raw::Eof => {
                    self.done = true;
                    if self.depth != 0 {
                        return Err(xml_err("unexpected end of document"));
                    }
                    if !self.root_done {
                        return Err(xml_err("document has no root element"));
                    }
                    Ok(None)
                }
            };
        }
    }
}

impl<R: Read> Iterator for Reader<R> {
    type Item = Result<Event>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_event() {
            Ok(ev) => ev.map(Ok),
            Err(e) => {
                self.done = true;
                self.pending_end = None;
                Some(Err(e))
            }
        }
    }
}
