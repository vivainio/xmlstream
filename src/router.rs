//! Callbacks on candidates, with a memory budget, and routes that change while parsing.
//!
//! A [`Router`] runs selectors in one forward pass and calls Rust closures. For each element a
//! route matches, a **gate** sees the start tag first, before anything is kept, and decides
//! what happens to it ([`Take`]):
//!
//! - **buffer** it: the subtree is built and handed to the route's handler when it closes;
//! - **stream** it: nothing is built; the route's stream callback gets every event of the
//!   subtree as it is parsed (feed them to a [`Matcher`](crate::Matcher) or a
//!   [`Writer`](crate::Writer));
//! - **skip** it.
//!
//! Every callback also receives a [`Control`] to switch the parsing logic on the fly: add
//! routes, remove them, or add routes that expire when the current element closes.

use std::{collections::VecDeque, io::Read};

use crate::{
    error::{Error, Result},
    path::{Engine, Limits, Match, Selector, StartTag},
    reader::{Event, Reader},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// Stop reading now, for every route. Nothing further is parsed.
    Stop,
}

/// What a gate decides about a candidate element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// Build the subtree and give it to the handler. Counts against the memory budget.
    Buffer,
    /// Hand the subtree's events to the stream callback as they are parsed. Costs no memory.
    Stream,
    Skip,
}

impl From<bool> for Take {
    /// `true` is [`Take::Buffer`], `false` is [`Take::Skip`].
    fn from(keep: bool) -> Take {
        if keep {
            Take::Buffer
        } else {
            Take::Skip
        }
    }
}

/// One event of a streamed subtree.
#[derive(Debug, Clone, Copy)]
pub struct StreamItem<'e> {
    pub event: &'e Event,
    /// Nesting relative to the matched element: 0 for its own start and end tags, 1 for what
    /// is directly inside it, and so on.
    pub depth: usize,
}

impl StreamItem<'_> {
    /// The start tag of the matched element: a new stream begins.
    pub fn is_first(&self) -> bool {
        self.depth == 0 && matches!(self.event, Event::Start { .. })
    }

    /// The end tag of the matched element: the stream is over.
    pub fn is_last(&self) -> bool {
        self.depth == 0 && matches!(self.event, Event::End { .. })
    }
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

type GateFn<'a> = Box<dyn FnMut(&StartTag, &mut Control<'a>) -> Take + 'a>;
type HandlerFn<'a> = Box<dyn FnMut(Match, &mut Control<'a>) -> Result<Flow> + 'a>;
type StreamFn<'a> = Box<dyn FnMut(&StreamItem, &mut Control<'a>) -> Result<Flow> + 'a>;

struct Route<'a> {
    gate: Option<GateFn<'a>>,
    handler: Option<HandlerFn<'a>>,
    stream: Option<StreamFn<'a>>,
}

fn boxed_gate<'a, T: Into<Take>>(
    mut g: impl FnMut(&StartTag, &mut Control<'a>) -> T + 'a,
) -> GateFn<'a> {
    Box::new(move |t, c| g(t, c).into())
}

/// A route that streams cannot also depend on content it has not read yet.
pub(crate) fn check_stream(sel: &Selector) -> Result<()> {
    if !sel.yields_elements() {
        return Err(Error::Path(format!(
            "{:?}: only element paths can be streamed (not @attr or text())",
            sel.as_str()
        )));
    }
    if sel.needs_buffering() {
        return Err(Error::Path(format!(
            "{:?}: a path with child-content predicates has to be buffered, it cannot be streamed",
            sel.as_str()
        )));
    }
    Ok(())
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
            stream: None,
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
    stream: Option<StreamFn<'a>>,
}

impl<'a> RouteBuilder<'_, 'a> {
    /// Anchors the route to the innermost element open at that moment, and removes it when that
    /// element closes. Added from a gate, that element is the one whose start tag matched.
    pub fn scoped(mut self) -> Self {
        self.scoped = true;
        self
    }

    /// Looks at each candidate's start tag first. Return `bool` or [`Take`].
    pub fn gate<T: Into<Take>>(
        mut self,
        gate: impl FnMut(&StartTag, &mut Control<'a>) -> T + 'a,
    ) -> Self {
        self.gate = Some(boxed_gate(gate));
        self
    }

    /// Lets the gate answer [`Take::Stream`] by calling this with each event of the subtree.
    pub fn stream(
        mut self,
        cb: impl FnMut(&StreamItem, &mut Control<'a>) -> Result<Flow> + 'a,
    ) -> Self {
        self.stream = Some(Box::new(cb));
        self
    }

    /// Finishes the route with a handler for buffered matches.
    pub fn on(
        self,
        handler: impl FnMut(Match, &mut Control<'a>) -> Result<Flow> + 'a,
    ) -> Result<RouteId> {
        self.finish(Some(Box::new(handler)))
    }

    /// Finishes a stream-only route: every match is streamed to the callback given to
    /// [`stream`](Self::stream).
    pub fn finish_stream(self) -> Result<RouteId> {
        self.finish(None)
    }

    fn finish(self, handler: Option<HandlerFn<'a>>) -> Result<RouteId> {
        let sel = Selector::parse_relative(self.path)?;
        if self.stream.is_some() {
            check_stream(&sel)?;
        }
        if handler.is_none() && self.stream.is_none() {
            return Err(Error::Path("a route needs a handler or a stream callback".into()));
        }
        let id = RouteId(self.ctl.next_id);
        self.ctl.next_id += 1;
        self.ctl.cmds.push(Cmd::Add {
            sel,
            route: Route {
                gate: self.gate,
                handler,
                stream: self.stream,
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
        self.add(path, None, Some(Box::new(handler)), None)
    }

    /// Like [`on`](Self::on), but `gate` sees the start tag first and can refuse the candidate,
    /// so it is never buffered and the handler never runs for it.
    pub fn on_gated<T: Into<Take>>(
        &mut self,
        path: &str,
        gate: impl FnMut(&StartTag, &mut Control<'a>) -> T + 'a,
        handler: impl FnMut(Match, &mut Control<'a>) -> Result<Flow> + 'a,
    ) -> Result<RouteId> {
        self.add(path, Some(boxed_gate(gate)), Some(Box::new(handler)), None)
    }

    /// Streams every match of `path`: `stream` gets each event of the subtree, nothing is built.
    pub fn on_stream(
        &mut self,
        path: &str,
        stream: impl FnMut(&StreamItem, &mut Control<'a>) -> Result<Flow> + 'a,
    ) -> Result<RouteId> {
        self.add(path, None, None, Some(Box::new(stream)))
    }

    /// Streams the candidates the gate answers [`Take::Stream`] for.
    pub fn on_stream_gated<T: Into<Take>>(
        &mut self,
        path: &str,
        gate: impl FnMut(&StartTag, &mut Control<'a>) -> T + 'a,
        stream: impl FnMut(&StreamItem, &mut Control<'a>) -> Result<Flow> + 'a,
    ) -> Result<RouteId> {
        self.add(path, Some(boxed_gate(gate)), None, Some(Box::new(stream)))
    }

    /// The gate picks per candidate: [`Take::Buffer`] goes to `handler`, [`Take::Stream`] to
    /// `stream`. Typical use: small elements are buffered, huge or unknown ones are streamed.
    pub fn on_split<T: Into<Take>>(
        &mut self,
        path: &str,
        gate: impl FnMut(&StartTag, &mut Control<'a>) -> T + 'a,
        handler: impl FnMut(Match, &mut Control<'a>) -> Result<Flow> + 'a,
        stream: impl FnMut(&StreamItem, &mut Control<'a>) -> Result<Flow> + 'a,
    ) -> Result<RouteId> {
        self.add(
            path,
            Some(boxed_gate(gate)),
            Some(Box::new(handler)),
            Some(Box::new(stream)),
        )
    }

    fn add(
        &mut self,
        path: &str,
        gate: Option<GateFn<'a>>,
        handler: Option<HandlerFn<'a>>,
        stream: Option<StreamFn<'a>>,
    ) -> Result<RouteId> {
        let sel = Selector::parse(path)?;
        if stream.is_some() {
            check_stream(&sel)?;
        }
        self.sels.push(sel);
        self.routes.push(Some(Route {
            gate,
            handler,
            stream,
        }));
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
        // subtrees being streamed right now: (route, nesting inside the matched element)
        let mut live: Vec<(usize, usize)> = Vec::new();

        while let Some(ev) = reader.next_event()? {
            let mut starting: Vec<usize> = Vec::new();
            let mut gate_err: Option<Error> = None;
            engine.feed(&ev, &mut out, &mut |si: usize, tag: &StartTag| {
                let Some(route) = routes.get_mut(si).and_then(Option::as_mut) else {
                    return true;
                };
                ctl.current = si;
                let take = match &mut route.gate {
                    Some(g) => g(tag, &mut ctl),
                    None if route.handler.is_some() => Take::Buffer,
                    None => Take::Stream,
                };
                match take {
                    Take::Skip => false,
                    Take::Buffer if route.handler.is_some() => true,
                    Take::Stream if route.stream.is_some() => {
                        starting.push(si);
                        false
                    }
                    other => {
                        gate_err.get_or_insert_with(|| {
                            Error::Path(format!(
                                "gate answered {other:?} but route {si} has no {}",
                                if other == Take::Stream { "stream callback" } else { "handler" }
                            ))
                        });
                        false
                    }
                }
            })?;
            if let Some(e) = gate_err {
                return Err(e);
            }
            apply(&mut ctl, routes, &mut engine, &mut scopes);
            live.extend(starting.into_iter().map(|si| (si, 0)));

            // Deliver this event to every subtree being streamed.
            let mut i = 0;
            while i < live.len() {
                let (si, cur) = live[i];
                let Some(route) = routes.get_mut(si).and_then(Option::as_mut) else {
                    live.remove(i); // route was removed mid-stream
                    continue;
                };
                let (depth, next) = match ev {
                    Event::Start { .. } => (cur, cur + 1),
                    Event::End { .. } => (cur - 1, cur - 1),
                    _ => (cur, cur),
                };
                ctl.current = si;
                let flow = (route.stream.as_mut().unwrap())(
                    &StreamItem {
                        event: &ev,
                        depth,
                    },
                    &mut ctl,
                )?;
                apply(&mut ctl, routes, &mut engine, &mut scopes);
                if flow == Flow::Stop {
                    return Ok(Stats {
                        skipped: engine.skipped,
                    });
                }
                if next == 0 {
                    live.remove(i);
                } else {
                    live[i].1 = next;
                    i += 1;
                }
            }

            while let Some((si, m)) = out.pop_front() {
                let Some(route) = routes.get_mut(si).and_then(Option::as_mut) else {
                    continue; // removed since the match was found
                };
                let Some(handler) = route.handler.as_mut() else {
                    continue;
                };
                ctl.current = si;
                let flow = handler(m, &mut ctl)?;
                apply(&mut ctl, routes, &mut engine, &mut scopes);
                if flow == Flow::Stop {
                    return Ok(Stats {
                        skipped: engine.skipped,
                    });
                }
            }

            let depth = engine.depth();
            let (expired, live_scopes): (Vec<_>, Vec<_>) =
                scopes.drain(..).partition(|&(_, d)| depth < d);
            scopes = live_scopes;
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
