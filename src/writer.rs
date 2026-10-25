//! A streaming XML generator. Text and attribute values are escaped, names are validated and
//! well-formedness (balanced tags, a single root) is enforced, so a broken document cannot be
//! emitted by accident.

use std::io::Write;

use crate::{
    element::{Element, Node},
    error::{Error, Result},
    reader::{Event, EventRef},
};

const FLUSH_AT: usize = 64 * 1024;
const MAX_DEPTH: usize = 256;

struct Frame {
    tag: String,
    has_child: bool,
    has_text: bool,
}

pub struct Writer<W: Write> {
    out: W,
    buf: Vec<u8>,
    flushed: u64,
    stack: Vec<Frame>,
    /// The last start tag is still open (`<a` + attributes, no `>` yet).
    pending: bool,
    root_done: bool,
    started: bool,
    finished: bool,
    declaration: bool,
    indent: Option<usize>,
}

fn is_name_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == ':'
}

fn is_name_char(c: char) -> bool {
    is_name_start(c) || c.is_numeric() || matches!(c, '-' | '.' | '\u{b7}')
}

fn check_name(kind: &str, name: &str) -> Result<()> {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if is_name_start(c) && chars.all(is_name_char) => Ok(()),
        _ => Err(werr(format!("invalid {kind} name {name:?}"))),
    }
}

fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r')
        || ('\u{20}'..='\u{d7ff}').contains(&c)
        || ('\u{e000}'..='\u{fffd}').contains(&c)
        || c >= '\u{10000}'
}

fn check_chars(s: &str) -> Result<()> {
    match s.chars().find(|c| !is_xml_char(*c)) {
        None => Ok(()),
        Some(c) => Err(werr(format!(
            "character U+{:04X} is not allowed in XML",
            c as u32
        ))),
    }
}

fn werr(m: impl Into<String>) -> Error {
    Error::Write(m.into())
}

pub fn escape(out: &mut Vec<u8>, s: &str, attr: bool) {
    for c in s.chars() {
        match c {
            '&' => out.extend_from_slice(b"&amp;"),
            '<' => out.extend_from_slice(b"&lt;"),
            '>' => out.extend_from_slice(b"&gt;"),
            '\r' => out.extend_from_slice(b"&#13;"),
            '"' if attr => out.extend_from_slice(b"&quot;"),
            '\t' if attr => out.extend_from_slice(b"&#9;"),
            '\n' if attr => out.extend_from_slice(b"&#10;"),
            c => {
                let mut b = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
        }
    }
}

impl<W: Write> Writer<W> {
    pub fn new(out: W) -> Self {
        Writer {
            out,
            buf: Vec::new(),
            flushed: 0,
            stack: Vec::new(),
            pending: false,
            root_done: false,
            started: false,
            finished: false,
            declaration: false,
            indent: None,
        }
    }

    /// Pretty-print with this many spaces per level.
    pub fn indent(mut self, n: usize) -> Self {
        self.indent = Some(n);
        self
    }

    /// Emit `<?xml version="1.0" encoding="UTF-8"?>` first.
    pub fn declaration(mut self, yes: bool) -> Self {
        self.declaration = yes;
        self
    }

    fn len(&self) -> u64 {
        self.flushed + self.buf.len() as u64
    }

    fn live(&self) -> Result<()> {
        if self.finished {
            Err(werr("writer is already finished"))
        } else {
            Ok(())
        }
    }

    fn begin(&mut self) {
        if !self.started {
            self.started = true;
            if self.declaration {
                self.buf
                    .extend_from_slice(br#"<?xml version="1.0" encoding="UTF-8"?>"#);
            }
        }
    }

    fn close_tag(&mut self) {
        if self.pending {
            self.buf.push(b'>');
            self.pending = false;
        }
    }

    fn newline(&mut self, depth: usize) {
        if let Some(n) = self.indent {
            if self.len() > 0 {
                self.buf.push(b'\n');
            }
            self.buf.resize(self.buf.len() + n * depth, b' ');
        }
    }

    fn begin_node(&mut self) {
        self.begin();
        self.close_tag();
        let depth = self.stack.len();
        match self.stack.last_mut() {
            Some(f) => {
                let inline = f.has_text;
                f.has_child = true;
                if !inline {
                    self.newline(depth);
                }
            }
            None => self.newline(0),
        }
    }

    fn spill(&mut self) -> Result<()> {
        if self.buf.len() >= FLUSH_AT {
            self.out.write_all(&self.buf)?;
            self.flushed += self.buf.len() as u64;
            self.buf.clear();
        }
        Ok(())
    }

    pub fn start(&mut self, tag: &str) -> Result<()> {
        self.start_with::<&str, &str>(tag, &[])
    }

    pub fn start_with<K: AsRef<str>, V: AsRef<str>>(
        &mut self,
        tag: &str,
        attrs: &[(K, V)],
    ) -> Result<()> {
        self.live()?;
        check_name("element", tag)?;
        if self.stack.is_empty() && self.root_done {
            return Err(werr("document already has a root element"));
        }
        if self.stack.len() >= MAX_DEPTH {
            return Err(werr("nesting too deep"));
        }
        for (k, v) in attrs {
            check_name("attribute", k.as_ref())?;
            check_chars(v.as_ref())?;
        }
        self.begin_node();
        self.buf.push(b'<');
        self.buf.extend_from_slice(tag.as_bytes());
        for (k, v) in attrs {
            self.buf.push(b' ');
            self.buf.extend_from_slice(k.as_ref().as_bytes());
            self.buf.extend_from_slice(b"=\"");
            escape(&mut self.buf, v.as_ref(), true);
            self.buf.push(b'"');
        }
        self.pending = true;
        self.stack.push(Frame {
            tag: tag.to_string(),
            has_child: false,
            has_text: false,
        });
        self.spill()
    }

    pub fn end(&mut self) -> Result<()> {
        self.live()?;
        let f = self
            .stack
            .pop()
            .ok_or_else(|| werr("end() without a matching start()"))?;
        if self.pending {
            self.buf.extend_from_slice(b"/>");
            self.pending = false;
        } else {
            if f.has_child && !f.has_text {
                let depth = self.stack.len();
                self.newline(depth);
            }
            self.buf.extend_from_slice(b"</");
            self.buf.extend_from_slice(f.tag.as_bytes());
            self.buf.push(b'>');
        }
        if self.stack.is_empty() {
            self.root_done = true;
        }
        self.spill()
    }

    pub fn text(&mut self, s: &str) -> Result<()> {
        self.live()?;
        check_chars(s)?;
        if self.stack.is_empty() {
            return Err(werr("text outside the root element"));
        }
        self.close_tag();
        self.stack.last_mut().unwrap().has_text = true;
        escape(&mut self.buf, s, false);
        self.spill()
    }

    pub fn cdata(&mut self, s: &str) -> Result<()> {
        self.live()?;
        check_chars(s)?;
        if self.stack.is_empty() {
            return Err(werr("cdata outside the root element"));
        }
        self.close_tag();
        self.stack.last_mut().unwrap().has_text = true;
        self.buf.extend_from_slice(b"<![CDATA[");
        // "]]>" cannot appear inside a section; split it across two.
        self.buf
            .extend_from_slice(s.replace("]]>", "]]]]><![CDATA[>").as_bytes());
        self.buf.extend_from_slice(b"]]>");
        self.spill()
    }

    pub fn comment(&mut self, s: &str) -> Result<()> {
        self.live()?;
        check_chars(s)?;
        if s.contains("--") || s.ends_with('-') {
            return Err(werr("a comment cannot contain \"--\" or end with \"-\""));
        }
        self.begin_node();
        self.buf.extend_from_slice(b"<!--");
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.extend_from_slice(b"-->");
        self.spill()
    }

    /// `<tag>text</tag>` in one call.
    pub fn text_element(&mut self, tag: &str, text: &str) -> Result<()> {
        self.start(tag)?;
        self.text(text)?;
        self.end()
    }

    /// Re-emit a reader event; makes `Reader` → `Writer` transforms a one-liner.
    pub fn write_event(&mut self, ev: &Event) -> Result<()> {
        self.write_event_ref(ev.as_ref())
    }

    /// Like [`write_event`](Self::write_event), for events borrowed from a `Reader::next_ref`
    /// loop, so a transform copies a stream without allocating per event.
    pub fn write_event_ref(&mut self, ev: EventRef<'_>) -> Result<()> {
        match ev {
            EventRef::Start { name, attrs } => self.start_with(name, attrs),
            EventRef::End { .. } => self.end(),
            EventRef::Text(t) => self.text(t),
            EventRef::CData(t) => self.cdata(t),
            EventRef::Comment(t) => self.comment(t),
        }
    }

    pub fn write_element(&mut self, e: &Element) -> Result<()> {
        self.start_with(&e.name, &e.attrs)?;
        for n in &e.children {
            match n {
                Node::Element(c) => self.write_element(c)?,
                Node::Text(t) => self.text(t)?,
                Node::CData(t) => self.cdata(t)?,
                Node::Comment(t) => self.comment(t)?,
            }
        }
        self.end()
    }

    /// Checks the document is complete, flushes, and returns the underlying writer.
    pub fn finish(mut self) -> Result<W> {
        self.live()?;
        if let Some(f) = self.stack.last() {
            return Err(werr(format!("element <{}> is not closed", f.tag)));
        }
        if !self.root_done {
            return Err(werr("document has no root element"));
        }
        if self.indent.is_some() {
            self.buf.push(b'\n');
        }
        self.finished = true;
        self.out.write_all(&self.buf)?;
        self.out.flush()?;
        Ok(self.out)
    }
}
