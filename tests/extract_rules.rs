use xmlstream::{Error, Extract, Reader, Record, Value};

const DOC: &str = r#"<shop>
  <order id="1" status="open"><total>9.5</total><sku>a</sku><sku>b</sku><vat>19</vat><paid>true</paid></order>
  <order id="2" status="closed"><total>20</total><sku>c</sku></order>
  <order id="3" status="open"><total> 3 </total></order>
  <refund id="r1" order="2"><amount>4.25</amount><note>late <b>again</b></note></refund>
</shop>"#;

fn all(ex: &Extract) -> Vec<Record> {
    ex.run(Reader::from_str(DOC)).map(|r| r.unwrap()).collect()
}

#[test]
fn builder_fields_types_and_cardinality() {
    let ex = Extract::new()
        .rule("//order[@status='open']")
        .field("id", "@id")
        .int()
        .field("total", "total/text()")
        .number()
        .field("skus", "sku/text()")
        .many()
        .field("vat", "vat/text()")
        .optional()
        .field("paid", "paid/text()")
        .bool()
        .optional()
        .build()
        .unwrap();
    let recs = all(&ex);
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0]["id"], Value::Int(1));
    assert_eq!(recs[0]["total"].as_f64(), Some(9.5));
    assert_eq!(recs[0]["skus"].as_list().unwrap().len(), 2);
    assert_eq!(recs[0]["vat"].as_str(), Some("19"));
    assert_eq!(recs[0]["paid"].as_bool(), Some(true));
    assert_eq!(recs[1]["total"].as_f64(), Some(3.0)); // surrounding spaces are fine for numbers
    assert!(recs[1]["vat"].is_null());
    assert_eq!(recs[1]["skus"].as_list().unwrap().len(), 0);
    assert!(recs[1]["nonexistent"].is_null());
}

#[test]
fn text_form_with_many_named_rules_and_constants() {
    let ex = Extract::parse(
        r#"
        # two kinds of records, one pass
        open = //order[@status='open'] {
            id    = @id
            total = total/text() as number
            skus  = sku/text() *          # zero or more
            kind  = "order"
        }
        refund = //refund {
            id     = @id
            order  = @order as int
            amount = amount/text() as number
            note   = note ?               # element value: all its text
            kind   = "refund"; version = "2" as int
        }
        "#,
    )
    .unwrap();
    let recs = all(&ex);
    let kinds: Vec<_> = recs
        .iter()
        .map(|r| (r.rule_name().unwrap(), r["kind"].to_string()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("open", "order".into()),
            ("open", "order".into()),
            ("refund", "refund".into())
        ]
    );
    assert_eq!(recs[2].rule_index(), 1);
    assert_eq!(recs[2]["order"], Value::Int(2));
    assert_eq!(recs[2]["note"].as_str(), Some("late again"));
    assert_eq!(recs[2]["version"], Value::Int(2));
    assert_eq!(
        recs[2].to_json_tagged(),
        r#"{"_rule":"refund","id":"r1","order":2,"amount":4.25,"note":"late again","kind":"refund","version":2}"#
    );
    assert_eq!(
        recs[0].to_json(),
        r#"{"id":"1","total":9.5,"skus":["a","b"],"kind":"order"}"#
    );
}

#[test]
fn builder_and_text_agree_and_constants_in_builder() {
    let a = Extract::new()
        .rule("//refund")
        .name("refund")
        .field("id", "@id")
        .constant("kind", "refund")
        .build()
        .unwrap();
    let b = Extract::parse("refund = //refund { id = @id; kind = \"refund\" }").unwrap();
    assert_eq!(all(&a), all(&b));
    assert_eq!(all(&a)[0].to_json(), r#"{"id":"r1","kind":"refund"}"#);
}

#[test]
fn special_paths_and_descendants() {
    let ex = Extract::parse("//refund { own = text()?; all = . ; deep = //b }").unwrap();
    let r = &all(&ex)[0];
    assert_eq!(r["own"].as_str(), None); // refund has no text of its own
    assert_eq!(r["all"].as_str(), Some("4.25late again"));
    assert_eq!(r["deep"].as_str(), Some("again"));
}

#[test]
fn nested_records_are_independent() {
    let ex = Extract::parse("//n { id = @id; kids = //n/@id * }").unwrap();
    let recs: Vec<_> = ex
        .run_str("<n id='a'><n id='b'><n id='c'/></n></n>")
        .map(|r| r.unwrap())
        .collect();
    // inner records finish first
    let ids: Vec<_> = recs.iter().map(|r| r["id"].to_string()).collect();
    assert_eq!(ids, ["c", "b", "a"]);
}

#[test]
fn errors_name_the_rule_and_field() {
    let ex = Extract::parse("o = //order { id = @id; t = total/text() as number }").unwrap();
    let errs: Vec<_> = ex.run(Reader::from_str(DOC)).collect();
    // order 1 is fine; order 2 is fine; order 3 has total " 3 " (fine) -> all good, so break one:
    assert!(errs.iter().all(|r| r.is_ok()));
    let ex = Extract::parse("o = //order { v = vat/text() }").unwrap();
    let e = ex.run(Reader::from_str(DOC)).nth(1).unwrap().unwrap_err();
    assert!(matches!(e, Error::Extract(_)), "{e}");
    assert!(
        e.to_string().contains("o") && e.to_string().contains("\"v\""),
        "{e}"
    );
    let ex =
        Extract::parse("//order { p = paid/text() as bool; t = total/text() as int }").unwrap();
    let e = ex.run(Reader::from_str(DOC)).next().unwrap().unwrap_err();
    assert!(e.to_string().contains("not an integer"), "{e}");
}

#[test]
fn bad_rules_are_rejected_when_built() {
    for (src, why) in [
        ("", "no rules"),
        ("//a {", "unterminated"),
        ("//a { }", "no fields"),
        ("//a { x = }", "empty path"),
        ("//a { x = @id; x = @id }", "duplicate"),
        ("//a { x = @id as float }", "unknown type"),
        ("//a[b='1'] { x = @id }", "needs buffering"),
        ("//a/@id { x = @id }", "attribute path as record"),
        ("//a { x = b[ }", "bad field path"),
        ("{ x = @id }", "no path"),
        ("//a { x = \"y\" as int }", "bad constant"),
    ] {
        assert!(Extract::parse(src).is_err(), "{why}: {src:?}");
    }
    assert!(Extract::new().field("x", "@id").build().is_err());
    assert!(Extract::new().rule("//a").build().is_err());
}

#[test]
fn records_stream_without_building_the_subtree() {
    // 200k items inside one record; fields come from the stream, memory stays small.
    let mut xml = String::from("<r><big id='b'>");
    for i in 0..200_000 {
        xml.push_str(&format!("<i k='{i}'/>"));
    }
    xml.push_str("</big></r>");
    let ex = Extract::parse("//big { id = @id; first = i/@k; n = i/@k * }").unwrap();
    let rec = ex.run_str(&xml).next().unwrap().unwrap();
    assert_eq!(rec["first"].as_str(), Some("0"));
    assert_eq!(rec["n"].as_list().unwrap().len(), 200_000);
}

#[test]
fn per_record_state_never_leaks_into_the_next_record() {
    // records reuse buffers: a value present in one record must not show up in the next
    let xml = "<r>\
        <o id='1' tag='x'><v>a</v><v>b</v><n>note</n>own</o>\
        <o id='2'/>\
        <o id='3' tag='y'><v>c</v></o>\
        <o/></r>";
    let ex = Extract::parse(
        "//o { id = @id ?; tag = @tag ?; vs = v/text() *; n = n ?; own = text() ?; all = . ? }",
    )
    .unwrap();
    let recs: Vec<_> = ex.run_str(xml).map(|r| r.unwrap()).collect();
    let j: Vec<_> = recs.iter().map(|r| r.to_json()).collect();
    assert_eq!(
        j[0],
        r#"{"id":"1","tag":"x","vs":["a","b"],"n":"note","own":"own","all":"abnoteown"}"#
    );
    assert_eq!(
        j[1],
        r#"{"id":"2","tag":null,"vs":[],"n":null,"own":null,"all":null}"#
    );
    assert_eq!(
        j[2],
        r#"{"id":"3","tag":"y","vs":["c"],"n":null,"own":null,"all":"c"}"#
    );
    assert_eq!(
        j[3],
        r#"{"id":null,"tag":null,"vs":[],"n":null,"own":null,"all":null}"#
    );
}

#[test]
fn dot_slash_paths_work_like_xpath() {
    let xml = "<r><order id='1'><foo>a</foo><x><foo>deep</foo></x><bar k='v'><baz>z</baz></bar></order></r>";
    let ex = Extract::parse(
        "//order { direct = ./foo/text(); any = .//foo/text() *; k = ./bar/@k; z = ./bar/baz/text(); has = ./bar[./baz] ? }",
    )
    .unwrap();
    let r = ex.run_str(xml).next().unwrap().unwrap();
    assert_eq!(r["direct"].as_str(), Some("a"));
    assert_eq!(r["any"].as_list().unwrap().len(), 2);
    assert_eq!(r["k"].as_str(), Some("v"));
    assert_eq!(r["z"].as_str(), Some("z"));
    assert!(!r["has"].is_null()); // predicate paths accept ./ too
                                  // `.` on its own is still the element's whole text
    let ex = Extract::parse("//foo { all = . }").unwrap();
    assert_eq!(
        ex.run_str(xml).next().unwrap().unwrap()["all"].as_str(),
        Some("a")
    );
}

const NESTED: &str = "<shop>\
<order id='1' status='open'><total>9.5</total>\
  <items><item sku='a' qty='2'><note>x</note><tag>t1</tag><tag>t2</tag></item><item sku='b' qty='1'/></items></order>\
<order id='2' status='open'><total>3</total></order>\
<order id='3' status='closed'><items><item sku='c' qty='5'/></items></order>\
</shop>";

const NESTED_RULES: &str = "
    open = //order[@status='open'] {
        id    = @id
        total = total/text() as number ?
        items = ./items/item {
            sku  = @sku
            qty  = @qty as int
            note = ./note/text() ?
            tags = ./tag/text() *
        } *
        all_skus = .//item/@sku *       # the parent can still see the same data directly
    }";

#[test]
fn nested_records_loop_naturally() {
    let ex = Extract::parse(NESTED_RULES).unwrap();
    let mut lines = vec![];
    for rec in ex.run_str(NESTED) {
        let rec = rec.unwrap();
        for item in rec["items"].as_list().unwrap() {
            lines.push(format!(
                "{}:{}x{}:{}:{}",
                rec["id"], item["sku"], item["qty"], item["note"], item["tags"]
            ));
        }
    }
    assert_eq!(lines, ["1:ax2:x:[t1, t2]", "1:bx1::[]"]);
}

#[test]
fn nested_json_counts_and_rule_name() {
    let ex = Extract::parse(NESTED_RULES).unwrap();
    let recs: Vec<_> = ex.run_str(NESTED).map(|r| r.unwrap()).collect();
    assert_eq!(recs.len(), 2); // order 3 is closed
    assert_eq!(
        recs[0].to_json(),
        r#"{"id":"1","total":9.5,"items":[{"sku":"a","qty":2,"note":"x","tags":["t1","t2"]},{"sku":"b","qty":1,"note":null,"tags":[]}],"all_skus":["a","b"]}"#
    );
    // an order with no items at all: an empty list, and nothing left over from order 1
    assert_eq!(
        recs[1].to_json(),
        r#"{"id":"2","total":3,"items":[],"all_skus":[]}"#
    );
    assert_eq!(
        recs[0]["items"].as_list().unwrap()[0]
            .as_record()
            .unwrap()
            .rule_name(),
        Some("items")
    );
    assert!(recs[0]["items"].as_list().unwrap()[0]["nonexistent"].is_null());
    assert!(recs[0]["id"]["whatever"].is_null()); // indexing a non-record is Null, not a panic
}

#[test]
fn builder_and_text_nesting_agree() {
    let a = Extract::new()
        .rule("//order[@status='open']")
        .name("open")
        .field("id", "@id")
        .field("total", "total/text()")
        .number()
        .optional()
        .field("items", "./items/item")
        .nested(|i| {
            i.field("sku", "@sku")
                .field("qty", "@qty")
                .int()
                .field("note", "./note/text()")
                .optional()
                .field("tags", "./tag/text()")
                .many()
        })
        .many()
        .field("all_skus", ".//item/@sku")
        .many()
        .build()
        .unwrap();
    let b = Extract::parse(&NESTED_RULES.replace("open =", "open =")).unwrap();
    let ja: Vec<_> = a.run_str(NESTED).map(|r| r.unwrap().to_json()).collect();
    let jb: Vec<_> = b.run_str(NESTED).map(|r| r.unwrap().to_json()).collect();
    assert_eq!(ja, jb);
}

#[test]
fn deep_and_recursive_nesting() {
    let xml =
        "<r><dir name='a'><dir name='b'><file n='1'/></dir><file n='2'/><dir name='c'/></dir></r>";
    // three levels: dir -> dir -> file, and the same selector nested inside itself
    let ex = Extract::parse(
        "/r/dir { name = @name; files = ./file { n = @n as int } *; subs = ./dir { name = @name; files = ./file { n = @n as int } * } * }",
    )
    .unwrap();
    let rec = ex.run_str(xml).next().unwrap().unwrap();
    assert_eq!(
        rec.to_json(),
        r#"{"name":"a","files":[{"n":2}],"subs":[{"name":"b","files":[{"n":1}]},{"name":"c","files":[]}]}"#
    );
    // a descendant path nests children of children of the same name, each a separate record
    let ex = Extract::parse("/r/dir { all = .//dir { name = @name } * }").unwrap();
    let rec = ex.run_str(xml).next().unwrap().unwrap();
    let names: Vec<_> = rec["all"]
        .as_list()
        .unwrap()
        .iter()
        .map(|d| d["name"].to_string())
        .collect();
    assert_eq!(names, ["b", "c"]);
    let ex = Extract::parse("//dir { name = @name; inner = .//dir { name = @name } * }").unwrap();
    let recs: Vec<_> = ex.run_str(xml).map(|r| r.unwrap()).collect();
    let got: Vec<_> = recs
        .iter()
        .map(|r| (r["name"].to_string(), r["inner"].as_list().unwrap().len()))
        .collect();
    assert_eq!(got, [("b".into(), 0), ("c".into(), 0), ("a".into(), 2)]); // inner records finish first
}

#[test]
fn nested_cardinality_and_errors() {
    let xml = "<r><o id='1'><i k='x'/></o><o id='2'/><o id='3'><i k='y'/><i k='z'/></o></r>";
    let one = Extract::parse("//o { id = @id; i = ./i { k = @k } }").unwrap();
    let r: Vec<_> = one.run_str(xml).collect();
    assert_eq!(r[0].as_ref().unwrap()["i"]["k"].as_str(), Some("x")); // exactly one: a single record
    let e = r[1].as_ref().unwrap_err().to_string(); // order 2 has none
    assert!(e.contains("\"i\"") && e.contains("not found"), "{e}");

    let opt = Extract::parse("//o { id = @id; i = ./i { k = @k } ? }").unwrap();
    let r: Vec<_> = opt.run_str(xml).map(|r| r.unwrap()).collect();
    assert_eq!(r[0]["i"]["k"].as_str(), Some("x"));
    assert!(r[1]["i"].is_null());
    assert_eq!(r[2]["i"]["k"].as_str(), Some("y")); // several: the first wins

    let plus = Extract::parse("//o { id = @id; i = ./i { k = @k } + }").unwrap();
    assert!(plus.run_str(xml).any(|r| r.is_err()));

    // errors inside a nested record name the whole path
    let bad = Extract::parse("//o { i = ./i { k = @k as int } * }").unwrap();
    let e = bad.run_str(xml).find_map(|r| r.err()).unwrap().to_string();
    assert!(
        e.contains("rule 0.i") && e.contains("not an integer"),
        "{e}"
    );
}

#[test]
fn nested_blocks_rejected_when_malformed() {
    for src in [
        "//o { i = ./i { k = @k }",             // missing closing brace
        "//o { i = { k = @k } * }",             // no path
        "//o { i = ./i { } * }",                // no fields
        "//o { i = ./i { k = @k; k = @k } * }", // duplicate inside
        "//o { i = ./i[x='1'] { k = @k } * }",  // child-content predicate on the child path
        "//o { i = ./i/@k { k = @k } * }",      // not an element path
    ] {
        assert!(Extract::parse(src).is_err(), "{src:?}");
    }
}

#[test]
fn nested_records_stream_without_building_the_parent() {
    // one parent with 100k children: only the extracted values are kept
    let mut xml = String::from("<r><p id='x'><items>");
    for i in 0..100_000 {
        xml.push_str(&format!(
            "<item k='{i}'><pad>{}</pad></item>",
            "z".repeat(40)
        ));
    }
    xml.push_str("</items></p></r>");
    let ex = Extract::parse("//p { id = @id; items = ./items/item { k = @k as int } * }").unwrap();
    let rec = ex.run_str(&xml).next().unwrap().unwrap();
    let items = rec["items"].as_list().unwrap();
    assert_eq!(items.len(), 100_000);
    assert_eq!(items[99_999]["k"].as_i64(), Some(99_999));
}

#[test]
fn dot_slash_own_forms() {
    let ex = Extract::parse("//item { a = ./@k; t = ./text(); both = . }").unwrap();
    let r = ex
        .run_str("<r><item k='1'>hi</item></r>")
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(r.to_json(), r#"{"a":"1","t":"hi","both":"hi"}"#);
}
