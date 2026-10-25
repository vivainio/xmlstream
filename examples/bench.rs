//! `cargo run --release --example bench -- FILE MODE`
//! Modes: events, attr, text, element, extract, extract_wide, extract_many

use std::{fs::File, time::Instant};

use xmlstream::{Extract, Reader, Selector};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("FILE");
    let mode = args.next().expect("MODE");
    let size = std::fs::metadata(&path)?.len() as f64;
    let reader = Reader::new(File::open(&path)?);
    let t = Instant::now();
    let n: usize = match mode.as_str() {
        "events" => reader.map(|e| e.map(|_| 1)).sum::<Result<usize, _>>()?,
        "attr" => Selector::parse("//order/@id")?.select(reader).count(),
        "text" => Selector::parse("//order/sku/text()")?.select(reader).count(),
        "element" => Selector::parse("//order")?.select(reader).count(),
        "extract" => Extract::parse("//order { id = @id; total = total/text() as number }")?
            .run(reader)
            .count(),
        "extract_wide" => Extract::parse(
            "//order { id = @id; status = @status; total = total/text() as number; \
             skus = sku/text() *; vat = vat/text() ?; note = note ? ; kind = \"order\" }",
        )?
        .run(reader)
        .count(),
        "extract_many" => Extract::parse(
            "a = //order[@status='open'] { id = @id; total = total/text() as number }\n\
             b = //order[@status='closed'] { id = @id; skus = sku/text() * }\n\
             c = //refund { id = @id }",
        )?
        .run(reader)
        .count(),
        m => panic!("unknown mode {m}"),
    };
    let s = t.elapsed().as_secs_f64();
    println!("{mode:13} {n:>9} results  {s:6.2}s  {:6.0} MB/s", size / 1e6 / s);
    Ok(())
}
