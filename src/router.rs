//! Callbacks on candidates, with a memory budget.
//!
//! A [`Router`] runs several selectors in one pass and calls Rust closures:
//!
//! - an optional **gate** sees the start tag the moment a selector matches it, before anything
//!   is buffered, and returns whether to keep the candidate at all;
//! - the **handler** receives the finished [`Match`] and says whether to carry on.
//!
//! Together with [`Limits`] this gives tight control: skip what you don't need from the tag
//! alone, and cap what you do buffer.

use std::{collections::VecDeque, io::Read};

use crate::{
    error::Result,
    path::{Engine, Limits, Match, Selector, StartTag},
    reader::Reader,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// Stop reading now. Nothing further is parsed, so this also saves time.
    Stop,
}

/// What a run did, besides calling handlers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Candidates dropped because of `Overflow::Skip`.
    pub skipped: u64,
}

type GateFn<'a> = Box<dyn FnMut(&StartTag) -> bool + 'a>;
type HandlerFn<'a> = Box<dyn FnMut(Match) -> Result<Flow> + 'a>;

struct Route<'a> {
    gate: Option<GateFn<'a>>,
    handler: HandlerFn<'a>,
}

#[derive(Default)]
pub struct Router<'a> {
    sels: Vec<Selector>,
    routes: Vec<Route<'a>>,
    limits: Limits,
}

impl<'a> Router<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Calls `handler` for every match of `path`.
    pub fn on(
        &mut self,
        path: &str,
        handler: impl FnMut(Match) -> Result<Flow> + 'a,
    ) -> Result<&mut Self> {
        self.add(path, None, Box::new(handler))
    }

    /// Like [`on`](Self::on), but `gate` sees the start tag first and can refuse the candidate,
    /// so it is never buffered and the handler never runs for it.
    pub fn on_gated(
        &mut self,
        path: &str,
        gate: impl FnMut(&StartTag) -> bool + 'a,
        handler: impl FnMut(Match) -> Result<Flow> + 'a,
    ) -> Result<&mut Self> {
        self.add(path, Some(Box::new(gate)), Box::new(handler))
    }

    fn add(
        &mut self,
        path: &str,
        gate: Option<GateFn<'a>>,
        handler: HandlerFn<'a>,
    ) -> Result<&mut Self> {
        self.sels.push(Selector::parse(path)?);
        self.routes.push(Route { gate, handler });
        Ok(self)
    }

    /// Reads the stream once, front to back, calling handlers as matches complete.
    pub fn run<R: Read>(&mut self, mut reader: Reader<R>) -> Result<Stats> {
        let mut engine = Engine::new(self.sels.clone(), self.limits);
        let mut out = VecDeque::new();
        let routes = &mut self.routes;
        while let Some(ev) = reader.next_event()? {
            engine.feed(
                &ev,
                &mut out,
                &mut |si: usize, tag: &StartTag| match &mut routes[si].gate {
                    Some(g) => g(tag),
                    None => true,
                },
            )?;
            while let Some((si, m)) = out.pop_front() {
                if (routes[si].handler)(m)? == Flow::Stop {
                    return Ok(Stats {
                        skipped: engine.skipped,
                    });
                }
            }
        }
        Ok(Stats {
            skipped: engine.skipped,
        })
    }
}
