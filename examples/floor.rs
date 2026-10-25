//! Baseline: how fast bare quick-xml reads a file with no per-event allocation.
//! `cargo run --release --example floor -- FILE`
use std::{fs::File, io::BufReader, time::Instant};
use quick_xml::{events::Event, Reader};
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let size = std::fs::metadata(&path).unwrap().len() as f64;
    let t = Instant::now();
    let mut r = Reader::from_reader(BufReader::with_capacity(64 * 1024, File::open(&path).unwrap()));
    r.config_mut().check_end_names = true;
    let mut buf = Vec::new();
    let (mut n, mut attrs) = (0usize, 0usize);
    loop {
        buf.clear();
        match r.read_event_into(&mut buf).unwrap() {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => { for a in e.attributes() { let a = a.unwrap(); attrs += a.value.len(); } n += 1 }
            _ => n += 1,
        }
    }
    let s = t.elapsed().as_secs_f64();
    println!("quick-xml floor {n} events {attrs} {s:.2}s {:.0} MB/s", size / 1e6 / s);
}
