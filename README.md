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

Pulling fields out of records, with no callbacks:

```rust
use xmlstream::{Extract, Reader};

let ex = Extract::parse(r#"
    open = //order[@status='open'] {
        id    = @id
        total = total/text() as number
        skus  = sku/text() *            # * zero or more, + one or more, ? optional
        kind  = "order"                 # a quoted value is a constant
    }
    refund = //refund {
        id = @id; amount = amount/text() as number; kind = "refund"
    }
"#)?;

for rec in ex.run(Reader::new(file)) {
    let rec = rec?;                              // one Record per matched element
    println!("{:?} {} {}", rec.rule_name(), rec["id"], rec["kind"]);
    // rec.to_json() -> {"id":"1","total":9.5,"skus":["a","b"],"kind":"order"}
}
```

The same rules in Rust, if you prefer a builder:

```rust
let ex = Extract::new()
    .rule("//order[@status='open']").name("open")
        .field("id", "@id")
        .field("total", "total/text()").number()
        .field("skus", "sku/text()").many()
        .constant("kind", "order")
    .rule("//refund").name("refund")
        .field("id", "@id")
        .constant("kind", "refund")
    .build()?;
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
xmlstream -e '//order { id = @id; skus = sku/text() * }' orders.xml   # NDJSON, one record per line
```

## Performance

Release build, one thread, on a 82 MB file of 400,000 pretty-printed orders (about
9.4 million events), read from disk. Peak memory was about 2 MB in every case.

| What | Results | Time | Throughput |
|---|---|---|---|
| quick-xml alone, no allocation (the floor) | 10.2 M events | 0.18 s | 450 MB/s |
| `Reader::next_ref` (borrowed events) | 9.4 M events | 0.36 s | 225 MB/s |
| `Reader` as an iterator (owned events) | 9.4 M events | 0.60 s | 136 MB/s |
| `//order/@id` | 400,000 | 0.46 s | 178 MB/s |
| `//order/sku/text()` | 1,000,563 | 0.51 s | 159 MB/s |
| `//order` (build each element) | 400,000 | 0.77 s | 106 MB/s |
| `Extract`, 2 fields | 400,000 records | 0.65 s | 126 MB/s |
| `Extract`, 7 fields (list, optional, element, constant) | 400,000 records | 0.93 s | 88 MB/s |
| `Extract`, nested: 1 field + 1 nested record per `sku` (1 M children) | 400,000 records | 0.85 s | 96 MB/s |
| `Extract`, 3 rules with predicates | 408,000 records | 0.76 s | 108 MB/s |

Reproduce with `cargo run --release --example bench -- FILE MODE`
(`examples/floor.rs` is the quick-xml baseline). Memory does not grow with the file.

What makes it fast: selectors and `Extract` read borrowed events from buffers the
reader reuses, so steady-state parsing allocates nothing; per-record state and
matchers are pooled and reused between records. If you write your own loop, prefer
`Reader::next_ref()` over iterating when speed matters; iterating gives you owned
`Event`s, which cost an allocation per string.

## What's in the box

- **Selectors**: an XPath subset evaluated in a single forward pass, with no seeking.
  `SelectorSet` runs several selectors in one pass over the same stream.
- **Reader**: a pull parser over any `Read`. Memory is bounded by one event.
  Input size and nesting depth are capped, and DOCTYPE is rejected, so hostile
  input cannot expand entities or exhaust memory.
- **Writer**: a streaming generator that escapes text, validates names and
  enforces well-formedness (balanced tags, one root), so it cannot emit a
  broken document. `write_event` pipes reader events straight into it for
  streaming transforms.

## Extracting records

`Extract` is the easy way to get data out. A **rule** says which elements are
records and which **fields** to take from each; you get one `Record` per element.
One run can hold many rules, so a single pass over the file can pull several kinds
of record, and every record knows its rule (`rule_name()`, `rule_index()`).

- **Fields** are written `name = path`, separated by a newline or `;`. Paths are
  relative to the record: `sku` and `./sku` are children, `//sku` and `.//sku` any descendant, `@id` the
  record's own attribute, `text()` its own text, `.` all its text (`./@id` and
  `./text()` mean the same as `@id` and `text()`, as in XPath).
- **Types**: `as text` (default), `int`, `number`, `bool`. A value that doesn't
  parse is an error that names the rule, field and value.
- **How many**: no marker is exactly one (missing is an error), `?` optional
  (`Value::Null`), `*` a list of zero or more, `+` a list of one or more. If a
  one-valued field matches several times, the first wins.
- **Constants**: a quoted value (`kind = "refund"`), or `.constant("kind", ..)` in
  the builder, puts a fixed value in every record of that rule. Useful to tell
  rules apart in downstream code. `"5" as int` makes it a typed constant.
- **Memory**: fields are filled while the element streams past, and only those
  values are kept, not the subtree. Paths ending in `@attr` or `text()` buffer
  nothing, so a record with 200,000 children costs only the fields you asked for.
  A field that selects an element (`note`) keeps that element's text, so it does
  buffer that element.
- `Record` has `get`, `rec["name"]` (a missing field reads as `Null`), `iter`,
  `to_json()` and `to_json_tagged()` (adds `"_rule"` for named rules).
- A record's path follows the same rule as streaming: no child-content predicates
  (`//order[sku='a']`). Put the condition in your code, or select the field.
- Rules in text have `#` comments. Mistakes are reported with the line number.

### Nested records: parents and their children

A field can itself be a block of fields. Its path selects the child elements; each
one becomes a record of its own, so `rec["items"]` is a list you can loop over, with
the parent's fields in scope in the outer loop:

```rust
let ex = Extract::parse(r#"
    order = //order {
        id    = @id
        total = total/text() as number
        items = ./items/item {            # a nested record per <item>
            sku  = @sku
            qty  = @qty as int
            note = ./note/text() ?
            tags = ./tag/text() *
        } *                               # same markers: none = one, ? * +
    }
"#)?;

for rec in ex.run(Reader::new(file)) {
    let rec = rec?;
    for item in rec["items"].as_list().unwrap() {
        println!("{} {} x{}", rec["id"], item["sku"], item["qty"]);
    }
}
// {"id":"1","total":9.5,"items":[{"sku":"a","qty":2,"note":"x","tags":["t1","t2"]}, ...]}
```

The same in the builder:

```rust
Extract::new()
    .rule("//order")
        .field("id", "@id")
        .field("items", "./items/item").nested(|i| i
            .field("sku", "@sku")
            .field("qty", "@qty").int()
            .field("note", "./note/text()").optional()).many()
    .build()?
```

- **Nesting is unlimited**: a nested block can contain more blocks (order, items,
  item, parts), and a block can match its own element name (folders in folders).
- **Paths are relative to the child**, and the parent can still read the same data
  with its own paths (`all_skus = .//item/@sku *`).
- **Streaming, not buffering**: when a child element starts, a child record is filled
  from the stream and handed to its parent when it closes. Neither the order nor
  the item is ever built; memory is the extracted values, like a list field.
- `item["sku"]` reads a field; a missing field or a non-record reads as `Null`.
  `item.as_record()` gives the `Record`, whose `rule_name()` is the field name.
- A nested record's path must select elements and cannot use child-content
  predicates, as for rules. Errors name the whole path, for example `order.items`.
- Children finish before their parent, so nested records of the same name come out
  innermost first.

When rules aren't enough, drop down to the selectors below, or to events.

### "Find each `//order`, then select `./foo` inside it"

That is exactly what a rule is: the rule path finds each `//order`, and each field
is a path evaluated from that order, with the same spelling you'd use in XPath:

| In XPath, from an order | As a field |
|---|---|
| `./@id`, `@id` | `id = @id` (the record's own attribute) |
| `./foo/text()` | `foo = ./foo/text()` (or `foo/text()`) |
| `./foo` (string value) | `foo = ./foo` |
| `.//foo` | `foo = .//foo/text() *` |
| `./items/item/sku` | `skus = ./items/item/sku/text() *` |
| `./foo[@k='x']/bar` | `bar = ./foo[@k='x']/bar/text()` |
| `./foo[./bar]` | `has = ./foo[./bar] ?` |
| `string(.)` | `all = .` |

In Rust code, the same relative paths work on a matched element:

```rust
for m in Selector::parse("//order")?.select(Reader::new(file)) {
    let order = m?.into_element().unwrap();
    let foos = order.select_str("./foo/text()")?;     // children
    let deep = order.select_str(".//foo/text()")?;    // any depth
    let id   = order.select_str("@id")?;              // its own attribute
}
```

For the parent-then-children shape, nest a block instead of a second loop (see
Nested records below): `items = ./items/item { sku = @sku; qty = @qty as int } *`.

Prefer a rule when you only need some fields: the fields are collected while the
order streams past and the order itself is never built. Use the loop when you need
the whole subtree.

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
| `item[@n>5]` | numeric compare (`<` `<=` `>` `>=` `=` `!=`); non-numbers never match |
| `item[contains(@id,'x')]`, `[starts-with(@id,'x')]` | string tests |
| `order[sku='a']`, `[price>100]`, `[b/c]` | child content, decided when the element closes (see Limits) |
| `//item/@id` | attribute value |
| `//item/text()` | direct text of the element |

A name without a prefix matches the local name, so `price` matches both
`<price>` and `<ns:price>`; `ns:price` matches that exact qualified name.

## Limits

### Predicates on children cost memory, not a second pass

The input is never rewound: xmlstream needs only `Read`, not `Seek`, so it works
on pipes, sockets and decompression streams.

A predicate on an element's own tag is free, because the tag is already read:

```text
//order[@status='open']     attribute on the start tag
//order[3]                  position among siblings
//order[@qty>5]             numeric comparison
```

A predicate on content is different. When `<order id="1">` arrives, its `<sku>`
child has not been read yet. Instead of seeking back, xmlstream keeps the
candidate `order` in memory, and decides when it closes:

```text
//order[sku='a']            child text
//order[price>100]          numeric comparison on a child
//order[contains(note,'x')] string test on a child
//order[sku='a']/item/@id   then keep searching inside the accepted order
```

Memory is bounded by the **largest candidate**, not by the file. Keep candidates
small: `//order[sku='a']` is fine, but `/orders[order]` makes the whole document the
candidate. A predicate path is relative to the element and may go deeper
(`[b/c/@k='2']`), but cannot contain predicates of its own.

If you need logic beyond this, select the elements and test them in plain Rust,
since each match is a small tree:

```rust
for m in Selector::parse("//order")?.select(Reader::new(file)) {
    let order = m?.into_element().unwrap();
    if order.child("sku").is_some_and(|s| s.text() == "a") {
        // anything you like
    }
}
```

### Memory budget

Matches that are elements (`//order`) and candidates held for a content predicate
are buffered. Cap that with `Limits`; it is off by default:

```rust
use xmlstream::{Limits, Overflow, Reader, Selector};

let limits = Limits {
    max_element_bytes: 64 * 1024,   // biggest single candidate
    max_total_bytes: 1 << 20,       // all candidates held at once
    on_overflow: Overflow::Skip,    // or Overflow::Error (the default)
};
for m in Selector::parse("//order")?.select_with(Reader::new(file), limits) {
    // an order bigger than 64 KB is dropped instead of buffered
}
```

`Error` stops with `Error::Limit`; `Skip` drops the largest offender and carries
on. Sizes count names, attributes and text; real heap use is a small multiple.
The same limits work for `SelectorSet::limits(..)` and `Matcher`. Prefer paths that
end in `@attr` or `text()`, or an `Extract` rule, when you don't need the whole
subtree: they buffer nothing.

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

## Working with events

`Reader` is an iterator of `Event`s, and selectors run on events, so you can write
your own loops and still use paths. For speed, `Reader::next_ref()` gives borrowed
`EventRef`s with no per-event allocation (valid until the next call), and
`Writer::write_event_ref` and `Matcher::feed_ref` accept them:

```rust
let mut r = Reader::new(file);
let mut w = Writer::new(std::io::stdout());
while let Some(ev) = r.next_ref()? {
    w.write_event_ref(ev)?;               // copy a stream without allocating
}
```

Owned events work too:

```rust
// transform: copy a stream, dropping comments
let mut out = Writer::new(std::io::stdout()).indent(2);
for ev in Reader::new(file) {
    let ev = ev?;
    if !matches!(ev, Event::Comment(_)) { out.write_event(&ev)?; }
}

// push: feed a selector events as they arrive from somewhere else
let mut m = Selector::parse("item/@sku")?.matcher();
for ev in events {
    for (_, hit) in m.feed(&ev)? { println!("{}", hit.to_text()); }
}

// pull: run a selector over any iterator of events
let n = Selector::parse("//order")?.select_events(events.into_iter().map(Ok)).count();
```

`Matcher::feed` treats the first element it sees as the document root, so feeding
the events of one subtree makes paths relative to it.

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
