# xmlstream

Query gigabyte XML files with XPath-like paths in constant memory.

xmlstream sits between SAX and DOM. You say what you want
(`//order[@status='open']`) and get back one small tree per match, instead of
writing a state machine or loading the whole file.

```rust
use xmlstream::Selector;

let sel = Selector::parse("//order[@status='open']/@id")?;
for m in sel.select_str(xml) {
    println!("{}", m?.to_text());
}
```

Streaming a file:

```rust
use xmlstream::{Reader, Selector};

let file = std::fs::File::open("huge.xml")?;
for m in Selector::parse("//order")?.select(Reader::new(file)) {
    let order = m?.into_element().unwrap();      // one subtree at a time
    let skus = order.select_str("sku/text()")?;  // query inside a match
}
```

## Command line

```sh
cargo install --path .
xmlstream '//order[@status="open"]/@id' orders.xml
xmlstream --pretty '//order[2]' < orders.xml
```

On a 1M-element, 43 MB file, finding one element takes about 0.7 s and 2 MB of memory.

## What's in the box

- **Selectors**: an XPath subset evaluated in a single forward pass.
  `SelectorSet` runs several selectors in one pass over the same stream.
- **Reader**: a pull parser over any `Read`. Memory is bounded by one event.
  Input size and nesting depth are capped, and DOCTYPE is rejected, so hostile
  input cannot expand entities or exhaust memory.
- **Writer**: a streaming generator that escapes text, validates names and
  enforces well-formedness (balanced tags, one root), so it cannot emit a
  broken document. `write_event` pipes reader events straight into it for
  streaming transforms.

## Selector syntax

| Path | Meaning |
|---|---|
| `/a/b` | child steps anchored at the document root |
| `//item` | `item` at any depth |
| `a/b` | relative paths match at any depth (same as `//a/b`) |
| `*` | any element |
| `item[3]` | third `item` among its siblings (1-based) |
| `item[@id]` | has attribute |
| `item[@id='x']`, `[@id!='x']` | attribute equals / differs |
| `item[contains(@id,'x')]`, `[starts-with(@id,'x')]` | string tests |
| `//item/@id` | attribute value |
| `//item/text()` | direct text of the element |

A name without a prefix matches the local name, so `price` matches both
`<price>` and `<ns:price>`; `ns:price` matches that exact qualified name.

## Limits

### Predicates can't look at children

A streaming parser reads the file once, front to back. When it reaches
`<order id="1">` it must decide immediately whether this element is a match, so it
can start collecting it. At that moment it knows three things: the tag name, the
attributes on that tag, and how many siblings came before it. It has not seen
the children yet.

So a predicate can use what is already known:

```text
//order[@status='open']     ok: attribute on the start tag
//order[3]                  ok: position among siblings
//order[contains(@id,'7')]  ok: attribute again
//order[sku='a']            rejected: needs the <sku> child, which comes later
//order[price > 100]        rejected: same reason
```

Answering `[sku='a']` would mean buffering every `order` until it closes, or
reading the file twice. Both would break the constant-memory promise, so the
selector refuses instead of quietly eating your RAM.

**The workaround is to filter in code.** Each match is a small tree, so test it
once it arrives. Memory stays bounded by one `order`:

```rust
let orders = Selector::parse("//order")?.select(Reader::new(file));
for m in orders {
    let order = m?.into_element().unwrap();
    if order.child("sku").is_some_and(|s| s.text() == "a") {
        // use it
    }
}
```

### Other limits

- Element matches arrive when the element closes, so with nested matches
  (`//a` on `<a><a/></a>`) the inner one comes out first. Attribute matches
  arrive at the start tag.
- `//` is only allowed before `@attr` or `text()` at the very start of a path.
  Write `//*/@id`, not `a//@id`.
- `item[2]` counts siblings with the same name test, before other predicates
  are applied.
- No validation: no XSD, no DTD, no entity expansion.

For full XPath 1.0 or XSLT on files that fit in memory, use a DOM library.

## How it compares

| | Memory | You write | Good for |
|---|---|---|---|
| DOM + XPath (roxmltree, libxml2) | whole document | a query | small and medium files |
| SAX (push callbacks) | tiny | a hand-built state machine | when you need every event |
| StAX / pull (quick-xml, xml-rs) | tiny | a hand-built loop | when you need every event |
| **xmlstream** | one matched subtree | a query | huge files, selected records |

## Built on quick-xml

xmlstream does not reinvent XML syntax. Tokenizing, tag matching, attribute
parsing and entity decoding are all done by [quick-xml](https://crates.io/crates/quick-xml),
a mature and fast parser. xmlstream adds the layer above it: bounded-memory
limits, merged text runs, the selector engine, owned subtrees for matches and
a strict writer.

## License

MIT
