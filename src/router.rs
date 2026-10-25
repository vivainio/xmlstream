//! Callbacks on candidates, with a memory budget, and routes that change while parsing.
//!
//! A [`Router`] runs selectors in one forward pass and calls Rust closures:
//!
//! - an optional **gate** sees the start tag the moment a selector matches it, before anything
//!   is buffered, and returns whether to keep the candidate at all;
//! - the **handler** receives the finished [`Match`] and says whether to carry on.
//!
//! Both receive a [`Control`] to switch the parsing logic on the fly: add routes, remove them,
//! or add routes that expire when the current element closes. Together with [`Limits`] this
//! gives tight control over both what is looked for and what is kept in memory.

use std::{collections::VecDeque, io::Read};

use crate::{
    error::Result,
    path::{Engine, Limits, Match, Selector, StartTag},
    reader::Reader,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// Stop reading now, for every route. Nothing further is parsed.
    Stop,
}

/// Identifies a route, so it can be removed later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RouteId(usize);

/// What a run did, besides calling handlers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Candidates dropped because of `Overflow::Skip`.
    pub skipped: u64,
}

type GateFn<'a> = Box<dyn FnMut(&StartTag, &mut Control<'a>) -> bool + 'a>;
type HandlerFn<'a> = Box<dyn FnMut(Match, &mut Control<'a>) -> Result<Flow> + 'a>;

struct Route<'a> {
    gate: Option<GateFn<'a>>,
    handler: HandlerFn<'a>,
}

enum Cmd<'a> {
    Add {
        sel: Selector,
        route: Route<'a>,
        scoped: bool,
    },
    Remove(RouteId),
}

/// Handed to every callback. Changes take effect at the next event, so a route added from a
/// gate sees the children of the element whose start tag the gate just looked at.
pub struct Control<'a> {
    next_id: usize,
    current: usize,
    cmds: Vec<Cmd<'a>>,
}

impl<'a> Control<'a> {
    /// The route whose callback is running.
    pub fn this_route(&self) -> RouteId {
        RouteId(self.current)
    }

    /// Starts describing a route to add. By default it applies to everything that follows in the
    /// document, and its first step is tried on every element that opens from now on. With
    /// [`scoped`](RouteBuilder::scoped) it applies only inside the element that is open now:
    /// `vat` is its child, `//vat` any descendant.
    pub fn route<'c>(&'c mut self, path: &'c str) -> RouteBuilder<'c, 'a> {
        RouteBuilder {
            ctl: self,
            path,
            scoped: false,
            gate: None,
        }
    }

    /// Switches a route off. Anything it was still buffering is dropped.
    pub fn remove(&mut self, id: RouteId) {
        self.cmds.push(Cmd::Remove(id));
    }

    /// Switches off the route whose callback is running.
    pub fn remove_this(&mut self) {
        self.remove(RouteId(self.current));
    }
}

pub struct RouteBuilder<'c, 'a> {
    ctl: &'c mut Control<'a>,
    path: &'c str,
    scoped: bool,
    gate: Option<GateFn<'a>>,
}

impl<'a> RouteBuilder<'_, 'a> {
    /// Anchors the route to the innermost element open at that moment, and removes it when that
    /// element closes. Added from a gate, that element is the one whose start tag matched.
    pub fn scoped(mut self) -> Self {
        self.scoped = true;
        self
    }

    pub fn gate(mut self, gate: impl FnMut(&StartTag, &mut Control<'a>) -> bool + 'a) -> Self {
        self.gate = Some(Box::new(gate));
        self
    }

    pub fn on(
        self,
        handler: impl FnMut(Match, &mut Control<'a>) -> Result<Flow> + 'a,
    ) -> Result<RouteId> {
        let sel = Selector::parse_relative(self.path)?;
        let id = RouteId(self.ctl.next_id);
        self.ctl.next_id += 1;
        self.ctl.cmds.push(Cmd::Add {
            sel,
            route: Route {
                gate: self.gate,
                handler: Box::new(handler),
            },
            scoped: self.scoped,
        });
        Ok(id)
    }
}

#[derive(Default)]
pub struct Router<'a> {
    sels: Vec<Selector>,
    routes: Vec<Option<Route<'a>>>,
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

    /// Calls `handler` for every match of `path`. Paths given here are anchored at the document,
    /// like [`Selector::parse`].
    pub fn on(
        &mut self,
        path: &str,
        handler: impl FnMut(Match, &mut Control<'a>) -> Result<Flow> + 'a,
    ) -> Result<RouteId> {
        self.add(path, None, Box::new(handler))
    }

    /// Like [`on`](Self::on), but `gate` sees the start tag first and can refuse the candidate,
    /// so it is never buffered and the handler never runs for it.
    pub fn on_gated(
        &mut self,
        path: &str,
        gate: impl FnMut(&StartTag, &mut Control<'a>) -> bool + 'a,
        handler: impl FnMut(Match, &mut Control<'a>) -> Result<Flow> + 'a,
    ) -> Result<RouteId> {
        self.add(path, Some(Box::new(gate)), Box::new(handler))
    }

    fn add(
        &mut self,
        path: &str,
        gate: Option<GateFn<'a>>,
        handler: HandlerFn<'a>,
    ) -> Result<RouteId> {
        self.sels.push(Selector::parse(path)?);
        self.routes.push(Some(Route { gate, handler }));
        Ok(RouteId(self.routes.len() - 1))
    }

    /// Reads the stream once, front to back, calling handlers as matches complete.
    pub fn run<R: Read>(&mut self, mut reader: Reader<R>) -> Result<Stats> {
        let mut engine = Engine::new(self.sels.clone(), self.limits);
        let mut out = VecDeque::new();
        let routes = &mut self.routes;
        let mut ctl = Control {
            next_id: routes.len(),
            current: 0,
            cmds: Vec::new(),
        };
        // (route, depth it was added at): removed once depth drops below that
        let mut scopes: Vec<(usize, usize)> = Vec::new();

        while let Some(ev) = reader.next_event()? {
            engine.feed(&ev, &mut out, &mut |si: usize, tag: &StartTag| {
                match routes.get_mut(si).and_then(Option::as_mut) {
                    Some(Route { gate: Some(g), .. }) => {
                        ctl.current = si;
                        g(tag, &mut ctl)
                    }
                    _ => true,
                }
            })?;
            apply(&mut ctl, routes, &mut engine, &mut scopes);

            while let Some((si, m)) = out.pop_front() {
                let Some(route) = routes.get_mut(si).and_then(Option::as_mut) else {
                    continue; // removed since the match was found
                };
                ctl.current = si;
                let flow = (route.handler)(m, &mut ctl)?;
                apply(&mut ctl, routes, &mut engine, &mut scopes);
                if flow == Flow::Stop {
                    return Ok(Stats {
                        skipped: engine.skipped,
                    });
                }
            }

            let depth = engine.depth();
            let (expired, live): (Vec<_>, Vec<_>) = scopes.drain(..).partition(|&(_, d)| depth < d);
            scopes = live;
            for (id, _) in expired {
                remove(id, routes, &mut engine);
            }
        }
        Ok(Stats {
            skipped: engine.skipped,
        })
    }
}

fn remove(id: usize, routes: &mut [Option<Route<'_>>], engine: &mut Engine) {
    if let Some(slot) = routes.get_mut(id) {
        if slot.take().is_some() {
            engine.deactivate(id);
        }
    }
}

fn apply<'a>(
    ctl: &mut Control<'a>,
    routes: &mut Vec<Option<Route<'a>>>,
    engine: &mut Engine,
    scopes: &mut Vec<(usize, usize)>,
) {
    for cmd in std::mem::take(&mut ctl.cmds) {
        match cmd {
            Cmd::Add { sel, route, scoped } => {
                let idx = engine.add_selector(sel, scoped);
                debug_assert_eq!(idx, routes.len());
                routes.push(Some(route));
                if scoped {
                    scopes.push((idx, engine.depth()));
                }
            }
            Cmd::Remove(RouteId(id)) => {
                remove(id, routes, engine);
                scopes.retain(|&(s, _)| s != id);
            }
        }
    }
}
