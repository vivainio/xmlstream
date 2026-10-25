//! `xmlstream PATH [FILE]`: stream matches of an XPath-like PATH out of FILE (or stdin).

use std::{
    fs::File,
    io::{self, BufWriter, Write},
    process::ExitCode,
};

use xmlstream::{Extract, Match, Reader, Selector};

const USAGE: &str = "usage: xmlstream [--pretty] [--trim] PATH [FILE]\n\
       xmlstream [--trim] (-e RULES | -r RULES_FILE) [FILE]    (one JSON object per record)\n\
  e.g. xmlstream '//order[@status=\"open\"]/@id' orders.xml\n\
       xmlstream -e '//order { id = @id; skus = sku/text() * }' orders.xml";

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut pretty = false;
    let mut trim = false;
    let mut pos = Vec::new();
    let mut rules: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-e" => rules = Some(args.next().ok_or(USAGE)?),
            "-r" => rules = Some(std::fs::read_to_string(args.next().ok_or(USAGE)?)?),
            "--pretty" => pretty = true,
            "--trim" => trim = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => pos.push(a),
        }
    }
    if let Some(src) = rules {
        let ex = Extract::parse(&src)?;
        let input: Box<dyn io::Read> = match pos.as_slice() {
            [] => Box::new(io::stdin().lock()),
            [f] => Box::new(File::open(f)?),
            _ => return Err(USAGE.into()),
        };
        let mut out = BufWriter::new(io::stdout().lock());
        for rec in ex.run(Reader::new(input).trim(trim)) {
            writeln!(out, "{}", rec?.to_json_tagged())?;
        }
        out.flush()?;
        return Ok(());
    }
    let (path, file) = match pos.as_slice() {
        [p] => (p, None),
        [p, f] => (p, Some(f)),
        _ => return Err(USAGE.into()),
    };
    let sel = Selector::parse(path)?;
    let input: Box<dyn io::Read> = match file {
        Some(f) => Box::new(File::open(f)?),
        None => Box::new(io::stdin().lock()),
    };
    let mut out = BufWriter::new(io::stdout().lock());
    for m in sel.select(Reader::new(input).trim(trim || pretty)) {
        match m? {
            Match::Element(e) => {
                let s = if pretty {
                    e.to_xml_pretty(2)?
                } else {
                    e.to_xml()?
                };
                out.write_all(s.trim_end().as_bytes())?;
            }
            Match::Attr(s) | Match::Text(s) => out.write_all(s.as_bytes())?,
        }
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
