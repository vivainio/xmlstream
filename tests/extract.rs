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

use xmlstream::{Error, Limits, Overflow};

fn big_doc() -> String {
    let mut s = String::from("<r>");
    for i in 0..1000 {
        s.push_str(&format!(
            "<order id='{i}' kind='small'><sku>{i}</sku></order>"
        ));
    }
    s.push_str("<order id='huge' kind='big'><blob>");
    s.push_str(&"x".repeat(100_000));
    s.push_str("</blob></order></r>");
    s
}

#[test]
fn budget_errors_by_default() {
    let limits = Limits {
        max_element_bytes: 1000,
        ..Default::default()
    };
    let r: Result<Vec<_>, _> = Selector::parse("//order")
        .unwrap()
        .select_with(Reader::from_str(&big_doc()), limits)
        .collect();
    assert!(matches!(r, Err(Error::Limit(_))));
}

#[test]
fn budget_skip_drops_only_the_big_one() {
    let limits = Limits {
        max_element_bytes: 1000,
        on_overflow: Overflow::Skip,
        ..Default::default()
    };
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
    let limits = Limits {
        max_total_bytes: 200,
        max_element_bytes: 200,
        ..Default::default()
    };
    let doc = &big_doc().replace(&"x".repeat(100_000), "x");
    let n = Selector::parse("//order")
        .unwrap()
        .select_with(Reader::from_str(doc), limits)
        .map(|m| m.unwrap())
        .count();
    assert_eq!(n, 1001);
}

#[test]
fn matcher_and_select_events() {
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

#[test]
fn borrowed_reader_matches_owned_and_reuses_buffers_safely() {
    // attribute counts go 3 -> 0 -> 1 -> 2 so reused slots must not leak old values;
    // text is split by entities, CDATA and a comment; empty elements owe an end tag
    let xml = "<r a='1' b='2' c='3'><e/><f z='9'>x &amp; y<![CDATA[<z>]]><!--k-->tail</f><g p='1' q='2'/></r>";
    let owned: Vec<Event> = Reader::from_str(xml).map(|e| e.unwrap()).collect();
    let mut r = Reader::from_str(xml);
    let mut borrowed = vec![];
    while let Some(ev) = r.next_ref().unwrap() {
        borrowed.push(ev.to_event());
    }
    assert_eq!(owned, borrowed);
    assert_eq!(
        owned[1],
        Event::Start {
            name: "e".into(),
            attrs: vec![]
        },
        "{owned:?}"
    );
    assert!(owned.contains(&Event::Text("x & y".into())));
    assert!(owned.contains(&Event::Start {
        name: "g".into(),
        attrs: vec![("p".into(), "1".into()), ("q".into(), "2".into())]
    }));
    // trimming works in place too
    let t: Vec<Event> = Reader::from_str("<a>\n  <b> x </b>\n  y \n</a>")
        .trim(true)
        .map(|e| e.unwrap())
        .collect();
    assert!(
        t.contains(&Event::Text("x".into())) && t.contains(&Event::Text("y".into())),
        "{t:?}"
    );
}

#[test]
fn borrowed_events_feed_writer_without_owning() {
    let xml = r#"<a x="1 &amp; 2"><b>t&lt;</b><![CDATA[c]]><!--k--><e/></a>"#;
    let mut r = Reader::from_str(xml);
    let mut w = Writer::new(Vec::new());
    while let Some(ev) = r.next_ref().unwrap() {
        w.write_event_ref(ev).unwrap();
    }
    assert_eq!(String::from_utf8(w.finish().unwrap()).unwrap(), xml);
}

#[test]
fn element_select_str_is_relative_like_xpath() {
    let order = Selector::parse("//order[@id='1']")
        .unwrap()
        .select_str(DOC)
        .next()
        .unwrap()
        .unwrap()
        .into_element()
        .unwrap();
    let t = |p: &str| -> Vec<String> {
        order
            .select_str(p)
            .unwrap()
            .into_iter()
            .map(|m| m.to_text())
            .collect()
    };
    assert_eq!(t("sku/text()"), ["a", "b"]);
    assert_eq!(t("./sku/text()"), ["a", "b"]);
    assert_eq!(t("./sku[2]/text()"), ["b"]);
    assert_eq!(t(".//sku/text()"), ["a", "b"]);
    assert_eq!(t("@id"), ["1"]);
    assert_eq!(t("./@status"), ["open"]); // ./@x is the element's own attribute, as in XPath
    assert_eq!(t("./sku/@nothing"), Vec::<String>::new());
    assert_eq!(t("./text()"), Vec::<String>::new()); // order has no text of its own
    assert_eq!(t("./note/text()"), ["hi & bye"]);
    assert_eq!(t("."), ["abhi & bye"]);
    assert_eq!(t("order"), Vec::<String>::new()); // the element itself is not its own child
}

#[test]
fn outer_loop_parents_inner_loop_children() {
    let xml = "<r>\
        <order id='1'><items><item sku='a' qty='2'><note>x</note></item><item sku='b' qty='1'/></items></order>\
        <order id='2'><items><item sku='c' qty='5'/></items></order></r>";
    let mut seen = vec![];
    for order in Selector::parse("//order").unwrap().select_str(xml) {
        let order = order.unwrap().into_element().unwrap();
        let id = order.attr("id").unwrap().to_string();
        for item in order.select_str("./items/item").unwrap() {
            let item = item.into_element().unwrap();
            let note = item.select_str("./note/text()").unwrap();
            seen.push(format!(
                "{id}:{}x{}{}",
                item.attr("sku").unwrap(),
                item.attr("qty").unwrap(),
                note.first()
                    .map_or(String::new(), |n| format!("({})", n.to_text())),
            ));
        }
    }
    assert_eq!(seen, ["1:ax2(x)", "1:bx1", "2:cx5"]);
}
