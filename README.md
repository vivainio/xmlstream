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

On a 1M-element, 43 MB file, finding one element takes about 0.7 s and 2 MB of memory.

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
  relative to the record: `sku` is a child, `//sku` any descendant, `@id` the
  record's own attribute, `text()` its own text, `.` all its text.
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

When rules aren't enough (switching logic mid-parse, event-level streaming),
drop down to the selectors and `Router` below.

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

## Memory control and callbacks

`Router` runs several selectors over **one** forward pass and calls your closures.
Each route has an optional **gate**, which sees the start tag before anything is
buffered, and a **handler**, which gets the finished match.

```rust
use xmlstream::{Flow, Limits, Overflow, Reader, Router};

let mut router = Router::new().limits(Limits {
    max_element_bytes: 64 * 1024,   // biggest single candidate
    max_total_bytes: 1 << 20,       // all candidates buffered at once
    on_overflow: Overflow::Skip,    // or Overflow::Error (the default)
});

router.on_gated(
    "//order[sku='a']",
    |tag, _| tag.attr("region") == Some("eu"),   // decided from the tag alone: no buffering if false
    |m, _| { handle(m.into_element().unwrap()); Ok(Flow::Continue) },
)?;
router.on("//audit/@user", |m, _| { log(m.to_text()); Ok(Flow::Continue) })?;

let stats = router.run(Reader::new(file))?;   // one pass; stats.skipped = dropped candidates
```

- **Many routes, one pass.** Every selector is advanced by the same event, so the
  input is read once, front to back, however many routes there are. Nothing is
  re-read. A test counts the bytes pulled from the source and checks it equals the
  document size with three routes registered.
- **Budgets are shared.** `max_total_bytes` covers candidates from all routes at
  the same moment. Bytes are released as soon as a candidate is handed to its
  handler or rejected. Sizes count names, attributes and text; real heap use is
  a small multiple of that.
- **Overflow.** `Error` stops with `Error::Limit`. `Skip` drops the largest
  offender and carries on, and `Stats::skipped` says how many were lost.
- **Gate first.** Prefer a gate when the tag already tells you the answer; the
  candidate then costs nothing. A gate only affects its own route.
- **Handlers run in event order**, as matches complete (element matches at the
  closing tag, attribute matches at the start tag). A handler error ends the run.
- **`Flow::Stop` ends the whole run**, for every route. Nothing more is parsed.

### Streaming a subtree instead of buffering it

For an element too big to build, the gate can answer `Take::Stream`. Nothing is
buffered; the route's stream callback gets every event of the subtree as it is
parsed, and can feed them to a `Matcher` (selectors) or a `Writer` (copy or
transform).

```rust
use xmlstream::{Flow, Reader, Router, Selector, Take};

let skus = Selector::parse("item/@sku")?;
let mut matcher = None;
let mut router = Router::new();
router.on_split(
    "/r/*",
    |tag, _| if tag.name == "big" { Take::Stream } else { Take::Buffer },
    |m, _| { small(m.into_element().unwrap()); Ok(Flow::Continue) },   // buffered
    |item, _| {                                                        // streamed
        if item.is_first() { matcher = Some(skus.matcher()); }
        for (_, hit) in matcher.as_mut().unwrap().feed(item.event)? {
            record(hit.to_text());
        }
        Ok(Flow::Continue)
    },
)?;
router.run(Reader::new(file))?;
```

- A gate returns a `Take` (`Buffer`, `Stream` or `Skip`); a plain `bool` still
  works (`true` is `Buffer`).
- `on_stream(path, cb)` always streams, `on_stream_gated` streams what the gate
  picks, and `on_split` lets the gate choose per candidate between a handler and
  a stream callback.
- Each `StreamItem` has the `event` and its `depth` relative to the matched
  element (0 for its own start and end tags). `is_first()` and `is_last()` mark
  the two ends, so you know when to reset per-element state.
- A `Matcher` fed the events treats the streamed element as the document root, so
  `item/@sku` means that element's `item` children. `Matcher::feed` returns the
  matches each event completes. Streaming costs no budget; only what you collect
  yourself uses memory.
- Only element paths can be streamed, and not ones with child-content predicates
  (`[sku='a']`), since those need the content before they can decide. Registering
  such a route is an error.
- Outside a router, `Selector::select_events(iter)` runs a selector over any
  iterator of `Event`s, and `Selector::matcher()` is the push equivalent.

### Switching logic while parsing

Every gate and handler also receives a `Control`, which changes the set of routes
while the parse carries on. It is a state machine driven by the document:

```rust
use std::{cell::RefCell, rc::Rc};

let vats = Rc::new(RefCell::new(Vec::new()));
let mut router = Router::new();
let v = vats.clone();
router.on("//order/@type", move |m, ctl| {
    if m.to_text() == "eu" {
        let v = v.clone();
        // watch <vat> only inside this order; the route removes itself when the order closes
        ctl.route("vat/text()").scoped().on(move |m, _| {
            v.borrow_mut().push(m.to_text());
            Ok(Flow::Continue)
        })?;
    }
    Ok(Flow::Continue)
})?;
router.run(Reader::new(file))?;
```

- `ctl.route(path)` starts a new route. Add `.gate(..)` for a start-tag gate and
  `.on(handler)` to finish. It returns a `RouteId`.
- **Unscoped** (the default): the route applies to everything that follows in the
  document. Its first step is tried on every element that opens from now on, so
  `vat` and `//vat` both find a `vat` at any depth. Positions (`[2]`) count only
  siblings seen after it was added.
- **`.scoped()`**: the route is anchored to the element that is open when it is
  added, and removed when that element closes. From a gate that is the element
  whose start tag matched, so `vat` means its child and `//vat` any descendant.
  From a handler for an element match (which fires at the closing tag) it is the
  parent.
- `ctl.remove(id)` and `ctl.remove_this()` switch a route off; anything it was
  still buffering is dropped. `ctl.this_route()` gives the current route's id.
- Changes apply from the next event on, so a route added by a gate sees the
  children of the element the gate looked at, and the stream is never rewound.
- Added routes share the router's memory budget.
- A route added from a callback outlives that call, so its closure cannot borrow
  the callback's variables. Share state with `Rc<RefCell<..>>`, or move owned
  values in.

The same budget works without a router: `Selector::select_with(reader, limits)`
and `SelectorSet::limits(limits)`.

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
