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
        .field("id", "@id").int()
        .field("total", "total/text()").number()
        .field("skus", "sku/text()").many()
        .field("vat", "vat/text()").optional()
        .field("paid", "paid/text()").bool().optional()
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
    let kinds: Vec<_> = recs.iter().map(|r| (r.rule_name().unwrap(), r["kind"].to_string())).collect();
    assert_eq!(kinds, [("open", "order".into()), ("open", "order".into()), ("refund", "refund".into())]);
    assert_eq!(recs[2].rule_index(), 1);
    assert_eq!(recs[2]["order"], Value::Int(2));
    assert_eq!(recs[2]["note"].as_str(), Some("late again"));
    assert_eq!(recs[2]["version"], Value::Int(2));
    assert_eq!(
        recs[2].to_json_tagged(),
        r#"{"_rule":"refund","id":"r1","order":2,"amount":4.25,"note":"late again","kind":"refund","version":2}"#
    );
    assert_eq!(recs[0].to_json(), r#"{"id":"1","total":9.5,"skus":["a","b"],"kind":"order"}"#);
}

#[test]
fn builder_and_text_agree_and_constants_in_builder() {
    let a = Extract::new()
        .rule("//refund").name("refund")
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
    assert!(e.to_string().contains("o") && e.to_string().contains("\"v\""), "{e}");
    let ex = Extract::parse("//order { p = paid/text() as bool; t = total/text() as int }").unwrap();
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
    assert_eq!(j[0], r#"{"id":"1","tag":"x","vs":["a","b"],"n":"note","own":"own","all":"abnoteown"}"#);
    assert_eq!(j[1], r#"{"id":"2","tag":null,"vs":[],"n":null,"own":null,"all":null}"#);
    assert_eq!(j[2], r#"{"id":"3","tag":"y","vs":["c"],"n":null,"own":null,"all":"c"}"#);
    assert_eq!(j[3], r#"{"id":null,"tag":null,"vs":[],"n":null,"own":null,"all":null}"#);
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
    assert_eq!(ex.run_str(xml).next().unwrap().unwrap()["all"].as_str(), Some("a"));
}
