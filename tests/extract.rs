use xmlstream::{Element, Event, Match, Reader, Selector, SelectorSet, Writer};

const DOC: &str = r#"<?xml version="1.0"?>
<shop xmlns:x="urn:x">
  <order id="1" status="open"><sku>a</sku><sku>b</sku><x:note>hi &amp; bye</x:note></order>
  <order id="2" status="closed"><sku>c</sku></order>
  <order id="3" status="open"/>
  <group><order id="4" status="open"><sku>d</sku></order></group>
</shop>"#;

fn texts(path: &str) -> Vec<String> {
    Selector::parse(path)
        .unwrap()
        .select_str(DOC)
        .map(|m| m.unwrap().to_text())
        .collect()
}

#[test]
fn child_vs_descendant() {
    assert_eq!(texts("/shop/order/@id"), ["1", "2", "3"]);
    assert_eq!(texts("//order/@id"), ["1", "2", "3", "4"]);
    assert_eq!(texts("order/@id"), ["1", "2", "3", "4"]);
    assert_eq!(texts("/order/@id"), Vec::<String>::new());
}

#[test]
fn predicates() {
    assert_eq!(texts("//order[@status='open']/@id"), ["1", "3", "4"]);
    assert_eq!(texts("//order[@status!='open']/@id"), ["2"]);
    assert_eq!(texts("//order[starts-with(@id,'2')]/@status"), ["closed"]);
    assert_eq!(texts("//order[contains(@status,'los')]/@id"), ["2"]);
    assert_eq!(texts("/shop/order[2]/@id"), ["2"]);
    assert_eq!(texts("//sku[2]"), ["b"]);
}

#[test]
fn text_and_namespaces() {
    assert_eq!(texts("//order/sku/text()"), ["a", "b", "c", "d"]);
    assert_eq!(texts("//note/text()"), ["hi & bye"]);
    assert_eq!(texts("//x:note/text()"), ["hi & bye"]);
    assert_eq!(texts("//y:note/text()"), Vec::<String>::new());
}

#[test]
fn element_subtrees() {
    let els: Vec<Element> = Selector::parse("//order[@status='open']")
        .unwrap()
        .select_str(DOC)
        .map(|m| m.unwrap().into_element().unwrap())
        .collect();
    assert_eq!(els.len(), 3);
    assert_eq!(els[0].attr("id"), Some("1"));
    assert_eq!(els[0].children_named("sku").count(), 2);
    assert_eq!(els[0].child("note").unwrap().text(), "hi & bye");
    assert_eq!(els[1].to_xml().unwrap(), r#"<order id="3" status="open"/>"#);
}

#[test]
fn nested_matches_both_reported() {
    let xml = "<a><a><a/></a></a>";
    let n = Selector::parse("//a").unwrap().select_str(xml).count();
    assert_eq!(n, 3);
}

#[test]
fn sub_select_on_element() {
    let order = Selector::parse("//order[@id='1']")
        .unwrap()
        .select_str(DOC)
        .next()
        .unwrap()
        .unwrap()
        .into_element()
        .unwrap();
    let skus: Vec<_> = order
        .select_str("sku/text()")
        .unwrap()
        .into_iter()
        .map(|m| m.to_text())
        .collect();
    assert_eq!(skus, ["a", "b"]);
}

#[test]
fn selector_set_single_pass() {
    let mut set = SelectorSet::new();
    let ids = set.add("//order/@id").unwrap();
    let skus = set.add("//sku/text()").unwrap();
    let mut got = (vec![], vec![]);
    for h in set.select(Reader::from_str(DOC)) {
        let (i, m) = h.unwrap();
        if i == ids {
            got.0.push(m.to_text())
        } else if i == skus {
            got.1.push(m.to_text())
        }
    }
    assert_eq!(got.0, ["1", "2", "3", "4"]);
    assert_eq!(got.1, ["a", "b", "c", "d"]);
}

#[test]
fn bad_paths() {
    for p in [
        "",
        "/",
        "a[",
        "a[@]",
        "a[b=]",
        "a[@x<'s']",
        "a/@id/b",
        "a//@id",
        "a[0]",
    ] {
        assert!(Selector::parse(p).is_err(), "{p:?} should not parse");
    }
}

#[test]
fn reader_rejects_bad_input() {
    for x in [
        "<a>",
        "<a></b>",
        "<!DOCTYPE a><a/>",
        "<a/><b/>",
        "<a>&nope;</a>",
        "text",
        "",
    ] {
        assert!(
            Reader::from_str(x).any(|e| e.is_err())
                || x.is_empty() && Reader::from_str(x).any(|e| e.is_err()),
            "{x:?}"
        );
    }
}

#[test]
fn reader_limits() {
    let opts = xmlstream::Options {
        max_depth: 3,
        ..Default::default()
    };
    let r = Reader::with_options("<a><b><c><d/></c></b></a>".as_bytes(), opts);
    assert!(r.into_iter().any(|e| e.is_err()));
    let opts = xmlstream::Options {
        max_bytes: 10,
        ..Default::default()
    };
    let r = Reader::with_options("<a>0123456789012345</a>".as_bytes(), opts);
    assert!(r.into_iter().any(|e| e.is_err()));
}

#[test]
fn reader_to_writer_roundtrip() {
    let xml = r#"<a x="1 &amp; 2"><b>t&lt;</b><![CDATA[c]]><!--k--><e/></a>"#;
    let mut w = Writer::new(Vec::new());
    for ev in Reader::from_str(xml) {
        w.write_event(&ev.unwrap()).unwrap();
    }
    assert_eq!(String::from_utf8(w.finish().unwrap()).unwrap(), xml);
}

#[test]
fn writer_is_strict() {
    let mut w = Writer::new(Vec::new());
    assert!(w.text("x").is_err());
    assert!(w.start("1bad").is_err());
    w.start("a").unwrap();
    assert!(w.text("\u{0}").is_err());
    assert!(w.comment("a--b").is_err());
    w.end().unwrap();
    assert!(w.start("b").is_err());
    assert!(w.end().is_err());
    let mut w = Writer::new(Vec::new());
    w.start("a").unwrap();
    assert!(w.finish().is_err());
}

#[test]
fn writer_pretty_and_declaration() {
    let mut w = Writer::new(Vec::new()).indent(2).declaration(true);
    w.start_with("a", &[("k", "v\"")]).unwrap();
    w.text_element("b", "x").unwrap();
    w.start("c").unwrap();
    w.end().unwrap();
    w.end().unwrap();
    let s = String::from_utf8(w.finish().unwrap()).unwrap();
    assert_eq!(
        s,
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<a k=\"v&quot;\">\n  <b>x</b>\n  <c/>\n</a>\n"
    );
}

#[test]
fn trim_option() {
    let evs: Vec<_> = Reader::from_str("<a>\n  <b> x </b>\n</a>")
        .trim(true)
        .map(|e| e.unwrap())
        .collect();
    assert!(evs.contains(&Event::Text("x".into())));
    assert_eq!(
        evs.iter().filter(|e| matches!(e, Event::Text(_))).count(),
        1
    );
    let _ = Match::Attr(String::new());
}

#[test]
fn child_predicates() {
    assert_eq!(texts("//order[sku='a']/@id"), ["1"]);
    assert_eq!(texts("//order[sku='d']/@id"), ["4"]);
    assert_eq!(texts("//order[sku]/@id"), ["1", "2", "4"]);
    assert_eq!(texts("//order[sku!='a']/@id"), ["1", "2", "4"]); // some sku differs
    assert_eq!(texts("//order[contains(sku,'c')]/@id"), ["2"]);
    assert_eq!(texts("//order[note='hi & bye']/@id"), ["1"]);
    assert_eq!(texts("//order[sku='a'][@status='open']/@id"), ["1"]);
    assert_eq!(
        texts("//order[sku='a'][@status='closed']/@id"),
        Vec::<String>::new()
    );
    // the rest of the path runs inside the accepted candidate
    assert_eq!(texts("//order[sku='b']/sku/text()"), ["a", "b"]);
    assert_eq!(texts("//order[@status='closed']/sku[1]/text()"), ["c"]);
}

#[test]
fn numeric_predicates() {
    let xml = "<r><i n='5'><p>9.5</p></i><i n='12'><p>100</p></i><i n='x'><p>abc</p></i></r>";
    let q = |p: &str| -> Vec<String> {
        Selector::parse(p)
            .unwrap()
            .select_str(xml)
            .map(|m| m.unwrap().to_text())
            .collect()
    };
    assert_eq!(q("//i[@n>6]/@n"), ["12"]);
    assert_eq!(q("//i[@n<=5]/@n"), ["5"]);
    assert_eq!(q("//i[p>=100]/@n"), ["12"]);
    assert_eq!(q("//i[p=9.5]/@n"), ["5"]);
    assert_eq!(q("//i[p>0]/@n"), ["5", "12"]); // non-numbers never match
}

#[test]
fn nested_child_paths_and_set() {
    let xml = "<r><a><b><c k='1'/></b></a><a><b><c k='2'/></b></a></r>";
    let n = |p: &str| Selector::parse(p).unwrap().select_str(xml).count();
    assert_eq!(n("//a[b/c/@k='2']"), 1);
    assert_eq!(n("//a[b/c]"), 2);
    assert_eq!(n("//a[b/z]"), 0);
}

/// Only `Read`, one byte per call: no seeking, no lookahead beyond what was handed over.
struct Dribble<'a>(&'a [u8]);

impl std::io::Read for Dribble<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.0.split_first() {
            Some((b, rest)) if !buf.is_empty() => {
                buf[0] = *b;
                self.0 = rest;
                Ok(1)
            }
            _ => Ok(0),
        }
    }
}

#[test]
fn works_on_a_forward_only_stream() {
    let ids: Vec<String> = Selector::parse("//order[sku='a']/@id")
        .unwrap()
        .select(Reader::new(Dribble(DOC.as_bytes())))
        .map(|m| m.unwrap().to_text())
        .collect();
    assert_eq!(ids, ["1"]);
}

use xmlstream::{Error, Flow, Limits, Overflow, Router};

fn big_doc() -> String {
    let mut s = String::from("<r>");
    for i in 0..1000 {
        s.push_str(&format!("<order id='{i}' kind='small'><sku>{i}</sku></order>"));
    }
    s.push_str("<order id='huge' kind='big'><blob>");
    s.push_str(&"x".repeat(100_000));
    s.push_str("</blob></order></r>");
    s
}

#[test]
fn budget_errors_by_default() {
    let limits = Limits { max_element_bytes: 1000, ..Default::default() };
    let r: Result<Vec<_>, _> = Selector::parse("//order")
        .unwrap()
        .select_with(Reader::from_str(&big_doc()), limits)
        .collect();
    assert!(matches!(r, Err(Error::Limit(_))));
}

#[test]
fn budget_skip_drops_only_the_big_one() {
    let limits = Limits { max_element_bytes: 1000, on_overflow: Overflow::Skip, ..Default::default() };
    let n = Selector::parse("//order")
        .unwrap()
        .select_with(Reader::from_str(&big_doc()), limits)
        .map(|m| m.unwrap())
        .count();
    assert_eq!(n, 1000);
}

#[test]
fn total_budget_is_released_as_candidates_finish() {
    // 1000 sequential candidates fit in a budget that holds only a couple at once.
    let limits = Limits { max_total_bytes: 200, max_element_bytes: 200, ..Default::default() };
    let doc = &big_doc().replace(&"x".repeat(100_000), "x");
    let n = Selector::parse("//order")
        .unwrap()
        .select_with(Reader::from_str(doc), limits)
        .map(|m| m.unwrap())
        .count();
    assert_eq!(n, 1001);
}

#[test]
fn gate_refuses_before_buffering() {
    // The same limit that errored above is fine when the gate keeps the big one out.
    let mut seen = 0;
    let mut kinds = vec![];
    let mut router = Router::new().limits(Limits { max_element_bytes: 1000, ..Default::default() });
    let stats = router
        .on_gated(
            "//order",
            |tag, _| tag.attr("kind") == Some("small"),
            |m, _| {
                seen += 1;
                kinds.push(m.as_element().unwrap().attr("kind").unwrap().to_string());
                Ok(Flow::Continue)
            },
        )
        .and_then(|_| router.run(Reader::from_str(&big_doc())))
        .unwrap();
    drop(router);
    assert_eq!(seen, 1000);
    assert!(kinds.iter().all(|k| k == "small"));
    assert_eq!(stats.skipped, 0);
}

#[test]
fn router_dispatches_and_stops() {
    let (mut ids, mut skus) = (vec![], vec![]);
    let mut router = Router::new();
    router
        .on("//order/@id", |m, _| {
            ids.push(m.to_text());
            Ok(if ids.len() == 2 { Flow::Stop } else { Flow::Continue })
        })
        .unwrap();
    router
        .on("//sku/text()", |m, _| {
            skus.push(m.to_text());
            Ok(Flow::Continue)
        })
        .unwrap();
    router.run(Reader::from_str(DOC)).unwrap();
    drop(router);
    assert_eq!(ids, ["1", "2"]);
    // Stop ends the whole run: order 2's start tag stopped it before its <sku> was read.
    assert_eq!(skus, ["a", "b"]);
}

#[test]
fn router_reports_skips_and_handler_errors() {
    let mut router = Router::new().limits(Limits {
        max_element_bytes: 1000,
        on_overflow: Overflow::Skip,
        ..Default::default()
    });
    router.on("//order", |_, _| Ok(Flow::Continue)).unwrap();
    assert_eq!(router.run(Reader::from_str(&big_doc())).unwrap().skipped, 1);

    let mut router = Router::new();
    router.on("//order", |_, _| Err(Error::other("nope"))).unwrap();
    assert!(router.run(Reader::from_str(DOC)).is_err());
}

/// Counts bytes pulled from the source, to show every route shares one pass.
struct Counting<'a>(&'a [u8], std::rc::Rc<std::cell::Cell<usize>>);

impl std::io::Read for Counting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.0.len().min(buf.len());
        buf[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        self.1.set(self.1.get() + n);
        Ok(n)
    }
}

#[test]
fn many_routes_share_one_forward_pass() {
    let doc = big_doc();
    let read = std::rc::Rc::new(std::cell::Cell::new(0));
    let (mut a, mut b, mut c) = (0, 0, 0);
    let mut router = Router::new();
    router.on("//order", |_, _| { a += 1; Ok(Flow::Continue) }).unwrap();
    router.on("//sku/text()", |_, _| { b += 1; Ok(Flow::Continue) }).unwrap();
    router.on("//order[sku='5']/@id", |_, _| { c += 1; Ok(Flow::Continue) }).unwrap();
    router.run(Reader::new(Counting(doc.as_bytes(), read.clone()))).unwrap();
    drop(router);
    assert_eq!((a, b, c), (1001, 1000, 1));
    assert_eq!(read.get(), doc.len(), "each byte read exactly once");
}

use std::{cell::RefCell, rc::Rc};

const EU_US: &str = "<r>\
<order type='eu'><vat>19</vat><note>n1</note></order>\
<order type='us'><vat>7</vat></order>\
<order type='eu'><vat>21</vat></order></r>";

// A route added from inside a callback outlives that call, so it cannot borrow the callback's
// captures; shared state goes through Rc<RefCell<..>>.

#[test]
fn scoped_route_added_from_a_start_tag() {
    // Only inside an EU order do we start looking at <vat>; the route dies with the order.
    let vats = Rc::new(RefCell::new(vec![]));
    let mut router = Router::new();
    let v = vats.clone();
    router
        .on("//order/@type", move |m, ctl| {
            if m.to_text() == "eu" {
                let v = v.clone();
                ctl.route("vat/text()").scoped().on(move |m, _| {
                    v.borrow_mut().push(m.to_text());
                    Ok(Flow::Continue)
                })?;
            }
            Ok(Flow::Continue)
        })
        .unwrap();
    router.run(Reader::from_str(EU_US)).unwrap();
    assert_eq!(*vats.borrow(), ["19", "21"]);
}

#[test]
fn unscoped_route_lives_until_removed() {
    let seen = Rc::new(RefCell::new(vec![]));
    let mut router = Router::new();
    let s = seen.clone();
    router
        .on("//order/@type", move |m, ctl| {
            if m.to_text() == "eu" && s.borrow().is_empty() {
                let s = s.clone();
                ctl.route("//vat/text()").on(move |m, ctl| {
                    s.borrow_mut().push(m.to_text());
                    if s.borrow().len() == 2 {
                        ctl.remove_this();
                    }
                    Ok(Flow::Continue)
                })?;
            }
            Ok(Flow::Continue)
        })
        .unwrap();
    router.run(Reader::from_str(EU_US)).unwrap();
    // added inside the first order, not scoped: sees its own vat, the us vat, then removes itself
    assert_eq!(*seen.borrow(), ["19", "7"]);
}

#[test]
fn gate_switches_logic_on_for_a_section() {
    // A gate on the section tag turns on a detail route; it expires when the section closes.
    let doc = "<r><log><e k='a'/></log><detail><e k='b'/><e k='c'/></detail><log><e k='d'/></log></r>";
    let got = Rc::new(RefCell::new(vec![]));
    let mut router = Router::new();
    let g = got.clone();
    router
        .on_gated(
            "/r/detail",
            move |_, ctl| {
                let g = g.clone();
                let _ = ctl.route("e/@k").scoped().on(move |m, _| {
                    g.borrow_mut().push(m.to_text());
                    Ok(Flow::Continue)
                });
                false // the section itself is not needed, only what the gate switched on
            },
            |_, _| Ok(Flow::Continue),
        )
        .unwrap();
    router.run(Reader::from_str(doc)).unwrap();
    assert_eq!(*got.borrow(), ["b", "c"]);
}

#[test]
fn bad_dynamic_path_is_an_error_not_a_panic() {
    let mut router = Router::new();
    router
        .on("//order", |_, ctl| {
            ctl.route("a[").on(|_, _| Ok(Flow::Continue))?;
            Ok(Flow::Continue)
        })
        .unwrap();
    assert!(router.run(Reader::from_str(EU_US)).is_err());
}

use xmlstream::Take;

fn blob_doc(items: usize) -> String {
    let mut s = String::from("<r><small id='s1'><item sku='x'/></small><big id='b1'>");
    for i in 0..items {
        s.push_str(&format!("<item sku='k{i}'><pad>{}</pad></item>", "p".repeat(50)));
    }
    s.push_str("</big><small id='s2'><item sku='y'/></small></r>");
    s
}

#[test]
fn stream_a_subtree_into_a_matcher_with_no_buffering() {
    // The big element is ~7 MB; the budget is a few KB. Streaming never builds it.
    let doc = blob_doc(100_000);
    let sel = Selector::parse("item/@sku").unwrap();
    let mut m = None;
    let mut skus = 0;
    let mut events = 0;
    let mut router = Router::new().limits(Limits { max_total_bytes: 4096, ..Default::default() });
    router
        .on_stream("/r/big", |it, _| {
            events += 1;
            if it.is_first() {
                m = Some(sel.matcher());
            }
            skus += m.as_mut().unwrap().feed(it.event)?.len();
            Ok(Flow::Continue)
        })
        .unwrap();
    router.run(Reader::from_str(&doc)).unwrap();
    drop(router);
    assert_eq!(skus, 100_000);
    assert!(events > 300_000);
}

#[test]
fn gate_picks_buffer_or_stream_per_candidate() {
    let doc = blob_doc(1000);
    let buffered = Rc::new(RefCell::new(vec![]));
    let streamed = Rc::new(RefCell::new((0usize, vec![])));
    let (b, s) = (buffered.clone(), streamed.clone());
    let mut router = Router::new().limits(Limits { max_element_bytes: 2000, ..Default::default() });
    router
        .on_split(
            "/r/*",
            |tag, _| if tag.name == "big" { Take::Stream } else { Take::Buffer },
            move |m, _| {
                b.borrow_mut().push(m.as_element().unwrap().attr("id").unwrap().to_string());
                Ok(Flow::Continue)
            },
            move |it, _| {
                let mut s = s.borrow_mut();
                s.0 += 1;
                if it.depth == 0 {
                    s.1.push(matches!(it.event, Event::Start { .. }));
                }
                Ok(Flow::Continue)
            },
        )
        .unwrap();
    router.run(Reader::from_str(&doc)).unwrap();
    assert_eq!(*buffered.borrow(), ["s1", "s2"]);
    let s = streamed.borrow();
    assert_eq!(s.1, [true, false]); // one start, one end at depth 0
    assert!(s.0 > 1000);
}

#[test]
fn stream_depth_and_first_last() {
    let mut seen = vec![];
    let mut router = Router::new();
    router
        .on_stream("//b", |it, _| {
            let tag = match it.event {
                Event::Start { name, .. } => format!("<{name}"),
                Event::End { name } => format!("/{name}"),
                Event::Text(t) => format!("'{t}'"),
                _ => "?".into(),
            };
            seen.push(format!("{}{}{}{}", it.depth, tag, if it.is_first() { "^" } else { "" }, if it.is_last() { "$" } else { "" }));
            Ok(Flow::Continue)
        })
        .unwrap();
    router.run(Reader::from_str("<a><b><c>t</c></b><d/></a>")).unwrap();
    drop(router);
    assert_eq!(seen, ["0<b^", "1<c", "2't'", "1/c", "0/b$"]);
}

#[test]
fn stream_route_rules() {
    let mut router = Router::new();
    assert!(router.on_stream("//a/@x", |_, _| Ok(Flow::Continue)).is_err());
    assert!(router.on_stream("//a[b='1']", |_, _| Ok(Flow::Continue)).is_err());
    assert!(router.on_stream("//a[@x='1']", |_, _| Ok(Flow::Continue)).is_ok());
    // a gate answering Stream for a route without a stream callback is reported, not ignored
    let mut router = Router::new();
    router.on_gated("//order", |_, _| Take::Stream, |_, _| Ok(Flow::Continue)).unwrap();
    assert!(router.run(Reader::from_str(DOC)).is_err());
}

#[test]
fn matcher_and_select_events_outside_a_router() {
    let events: Vec<_> = Reader::from_str(DOC).map(|e| e.unwrap()).collect();
    let sel = Selector::parse("//order/@id").unwrap();
    // pull: any iterator of events
    let ids: Vec<_> = sel
        .select_events(events.iter().cloned().map(Ok))
        .map(|m| m.unwrap().to_text())
        .collect();
    assert_eq!(ids, ["1", "2", "3", "4"]);
    // push: one event at a time
    let mut m = sel.matcher();
    let mut ids = vec![];
    for ev in &events {
        ids.extend(m.feed(ev).unwrap().into_iter().map(|(_, h)| h.to_text()));
    }
    assert_eq!(ids, ["1", "2", "3", "4"]);
}
