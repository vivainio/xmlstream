//! Bounded-memory XML: a pull [`Reader`], a strict [`Writer`], and an XPath-like streaming
//! extractor ([`Selector`], [`SelectorSet`]) that hands you one matched subtree at a time.
//!
//! ```
//! use xmlstream::Selector;
//!
//! let xml = r#"<orders><order id="1"><sku>a</sku></order><order id="2"><sku>b</sku></order></orders>"#;
//! let ids: Vec<String> = Selector::parse("//order/@id").unwrap()
//!     .select_str(xml)
//!     .map(|m| m.unwrap().to_text())
//!     .collect();
//! assert_eq!(ids, ["1", "2"]);
//! ```

mod element;
mod extract;
mod error;
mod path;
mod reader;
mod value;
mod writer;

pub use element::{Element, Node};
pub use extract::{Card, Extract, ExtractBuilder, Records, Type};
pub use error::{Error, Result};
pub use path::{Hits, Limits, Match, Matcher, Overflow, Select, Selector, SelectorSet};
pub use reader::{Event, Options, Reader};
pub use writer::Writer;
pub use value::{Record, Value};
