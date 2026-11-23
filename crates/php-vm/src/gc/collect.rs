// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! A collection run (`zend_gc_collect_cycles`): analysis, the destructor
//! protocol, freeing, and the single re-run after destructors.

use super::graph::{Graph, Handle};
use super::traverse::is_traversed;
use super::{take_self_arrays, with_state, RootProvider};
use crate::object::{self, oflags, ObjectData};
use crate::vm::{Throw, Vm, VmResult};
use php_runtime::{Obj, PhpObject, Value};
use std::any::Any;
use std::collections::HashSet;
use std::rc::{Rc, Weak};
use std::time::Instant;

/// Values emptied out of garbage nodes are released this many at a time.
const TRASH_BATCH: usize = 4096;

/// What a run considers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// `gc_collect_cycles()`: the buffered roots plus every live object.
    Full,
    /// A threshold-triggered run over the buffered roots.
    Auto,
}

/// Disables root recording in every provider while alive.
struct Quiet {
    providers: Vec<(RootProvider, bool)>,
}

impl Quiet {
    fn new(providers: &[RootProvider]) -> Self {
        Quiet {
            providers: providers
                .iter()
                .map(|p| (*p, (p.set_recording)(false)))
                .collect(),
        }
    }
}

impl Drop for Quiet {
    fn drop(&mut self) {
        for (p, old) in self.providers.drain(..).rev() {
            (p.set_recording)(old);
        }
    }
}

/// Clears `active` even when a destructor bails out.
struct ActiveGuard;

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        with_state(|s| s.active = false);
    }
}

/// What a single pass leaves for the next one.
#[derive(Default)]
struct PassResult {
    /// Freed objects and arrays.
    count: i64,
    /// Destructors were called (a re-run is due).
    had_dtors: bool,
    /// Objects only the collector still holds after their destructor
    /// (`GC_DELREF` to zero without freeing): freed by the re-run.
    zombies: Vec<Obj>,
    /// Whether the pass had anything to look at (`num_roots != 0`).
    ran: bool,
}

fn has_pending_dtor(d: &ObjectData) -> bool {
    d.flags.get() & oflags::DESTRUCTED == 0 && d.class.magic.destruct.is_some()
}

impl Vm {
    /// Runs a collection; returns the number of freed objects and arrays.
    pub(super) fn gc_run(&mut self, mode: Mode) -> VmResult<i64> {
        let start = Instant::now();
        let Some(providers) = with_state(|s| {
            if s.active || s.protected {
                return None;
            }
            s.active = true;
            Some(s.providers.clone())
        })
        .flatten() else {
            return Ok(0);
        };
        let guard = ActiveGuard;
        let full = mode == Mode::Full;
        let mut extra: Vec<Value> = Vec::new();
        let mut total = 0;
        let mut err: Option<Throw> = None;
        let mut rerun = false;
        loop {
            let pass = self.gc_pass(&providers, full, std::mem::take(&mut extra), &mut err);
            let pass = match pass {
                Ok(p) => p,
                Err(e) => {
                    drop(guard);
                    with_state(|s| s.collector_time += start.elapsed());
                    return Err(e);
                }
            };
            total += pass.count;
            with_state(|s| {
                // The re-run after destructors always finds the destructor
                // objects buffered as roots in php-src.
                if pass.ran || rerun {
                    s.runs += 1;
                }
                s.collected += u64::try_from(pass.count).unwrap_or(0);
            });
            if !pass.had_dtors || rerun {
                // No third pass: zombies are released (their destructor
                // has run, so this simply frees them).
                drop(pass.zombies);
                break;
            }
            // Objects with destructors were removed from this pass: run
            // again right away to free them (once). They stayed in the
            // root buffer (`GC_DTOR_GARBAGE` becomes a root again); the
            // zombies are handed over strongly.
            rerun = true;
            extra.extend(pass.zombies.into_iter().map(Value::Object));
        }
        drop(guard);
        with_state(|s| s.collector_time += start.elapsed());
        self.root_live_temporaries();
        // Values released by freeing (e.g. `WeakMap` entries) may have
        // queued destructors of ordinary objects.
        let r = self.run_pending_destructors();
        match err {
            Some(e) => Err(e),
            None => r.map(|()| total),
        }
    }

    /// `zend_gc_check_root_tmpvars`: the values the running frames'
    /// temporaries and `foreach` loops hold are rooted again after a run
    /// (see [`super::root_live_value`]).
    fn root_live_temporaries(&self) {
        use crate::vm::ForeachIter;
        for f in &self.frames {
            for v in &f.tmps {
                super::root_live_value(v);
            }
            for it in f.iters.iter().flatten() {
                match it {
                    ForeachIter::ArrayRef { r, .. } => {
                        php_runtime::ref_roots::check_possible_root(r);
                    }
                    ForeachIter::Props { subject, .. } => super::root_live_value(subject),
                    ForeachIter::Iterator { obj, .. } | ForeachIter::Generator { obj, .. } => {
                        super::root_live_obj(obj);
                    }
                    ForeachIter::Array { .. } | ForeachIter::Native(..) | ForeachIter::Empty => {}
                }
            }
        }
    }

    /// One mark/scan/collect pass (`rerun_gc:` of `zend_gc_collect_cycles`)
    /// over the root buffer in slot order. Destructor exceptions are
    /// accumulated in `err` (chained like `zend_objects_destroy_object`);
    /// other unwinds abort the pass.
    #[allow(clippy::too_many_lines)] // the phases of zend_gc_collect_cycles
    fn gc_pass(
        &mut self,
        providers: &[RootProvider],
        full: bool,
        extra: Vec<Value>,
        err: &mut Option<Throw>,
    ) -> VmResult<PassResult> {
        let quiet = Quiet::new(providers);
        // `gc_mark_roots`: the buffer is compacted, then walked in slot
        // order. The roots stay buffered during the pass.
        let (roots, self_arrays) = with_state(|s| {
            s.roots.drop_dead();
            s.roots.compact();
            s.ref_pending = 0;
            (s.roots.roots(), take_self_arrays(s))
        })
        .unwrap_or_default();
        let mut refs: Vec<Value> = Vec::new();
        for p in providers {
            refs.extend((p.take)());
        }
        let had_roots = !roots.is_empty() || !refs.is_empty() || !extra.is_empty();
        let mut g = Graph::default();
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: one entry per collected root"
        )]
        let mut root_nodes: Vec<(usize, Option<u32>)> = Vec::with_capacity(roots.len());
        for (slot, o) in &roots {
            root_nodes.push((*slot, g.intern_obj(o)));
        }
        for r in refs.iter().chain(&extra) {
            g.add_root(r);
        }
        let self_nodes: Vec<u32> = self_arrays.iter().map(|a| g.intern_arr(a)).collect();
        drop(roots);
        drop(refs);
        drop(extra);
        drop(self_arrays);
        if full {
            for d in object::live_objects() {
                g.add_object(d);
            }
        }
        g.build();
        let garbage = g.analyze();
        // `gc_collect_roots`: the live (black) roots leave the buffer, which
        // is compacted (its last roots move into the holes); the garbage
        // roots, in their new slot order, come first in the buffer order.
        let white_objs = with_state(|s| {
            for &(slot, node) in &root_nodes {
                if node.is_none_or(|n| g.nodes[n as usize].live) {
                    s.roots.release(slot);
                }
            }
            s.roots.compact();
            s.roots.roots()
        })
        .unwrap_or_default();
        drop(root_nodes);
        let white: Vec<(usize, u32)> = white_objs
            .iter()
            .filter_map(|(slot, o)| g.intern_obj(o).map(|x| (*slot, x)))
            .collect();
        drop(white_objs);
        let mut res = PassResult {
            ran: had_roots || garbage > 0,
            ..PassResult::default()
        };
        if garbage == 0 {
            return Ok(res);
        }
        let n = g.nodes.len();
        let mut count: i64 = g
            .nodes
            .iter()
            .filter(|n| !n.live && n.counted())
            .count()
            .try_into()
            .unwrap_or(i64::MAX);
        // Garbage objects whose destructor has to run first.
        let in_d: Vec<bool> = g
            .nodes
            .iter()
            .map(|n| !n.live && n.data().is_some_and(has_pending_dtor) && n.obj().is_some())
            .collect();
        let mut removed = vec![false; n];
        let mut dtors: Vec<Weak<dyn PhpObject>> = Vec::new();
        // Slots taken for garbage that is no root (`gc_add_garbage`).
        let mut placeholders: Vec<usize> = Vec::new();
        if in_d.iter().any(|&b| b) {
            res.had_dtors = true;
            let white_nodes: Vec<u32> = white.iter().map(|&(_, x)| x).collect();
            // `gc_collect_white` appends the garbage it reaches from the
            // roots: objects with destructors stay buffered as roots of the
            // re-run (`GC_DTOR_GARBAGE`), the rest only holds its slot.
            let added = white_order(&g, &white_nodes);
            let mut slot_of = vec![u32::MAX; n];
            for &(slot, x) in &white {
                slot_of[x as usize] = u32::try_from(slot).unwrap_or(u32::MAX);
            }
            with_state(|s| {
                for &x in &added {
                    let xi = x as usize;
                    let slot = match g.nodes[xi].obj() {
                        Some(o) if in_d[xi] => super::buffer_obj(s, o),
                        _ => {
                            let i = s.roots.push_garbage();
                            placeholders.push(i);
                            Some(i)
                        }
                    };
                    if let Some(i) = slot {
                        slot_of[xi] = u32::try_from(i).unwrap_or(u32::MAX);
                    }
                }
            });
            // `gc_remove_nested_data_from_buffer`, for each destructor
            // object in buffer order: data reachable from it may be
            // resurrected by it; it is left for the re-run. The destructors
            // are called in the same (buffer) order.
            let mut nested: Vec<u32> = Vec::new();
            for &x in white_nodes.iter().chain(&added) {
                if !in_d[x as usize] {
                    continue;
                }
                count -= remove_nested(&g, x, &in_d, &mut removed, &mut nested);
                if let Some(o) = g.nodes[x as usize].obj() {
                    dtors.push(Rc::downgrade(o.rc()));
                }
            }
            // `GC_REMOVE_FROM_BUFFER` of the nested data, in removal order:
            // roots released by the destructors reuse these slots first.
            with_state(|s| {
                for &x in &nested {
                    let xi = x as usize;
                    if !in_d[xi] && slot_of[xi] != u32::MAX {
                        s.roots.release(slot_of[xi] as usize);
                        slot_of[xi] = u32::MAX;
                    }
                }
            });
        }
        // Garbage arrays that contain themselves are emptied in the free
        // phase (nothing else breaks a cycle of arrays alone).
        let self_garbage: HashSet<usize> = self_nodes
            .iter()
            .map(|&x| x as usize)
            .filter(|&x| !g.nodes[x].live && !removed[x])
            .filter_map(|x| match &g.nodes[x].handle {
                Handle::Arr(a) => Some(a.addr()),
                _ => None,
            })
            .collect();
        // Keep the handles of the nodes to free; release everything else
        // (no destructor can run before the destructor phase below, and
        // nothing here can reach a zero count: every node is still held by
        // its cycle). The edges and indexes are done with, and the node
        // list itself keeps the handles (no second list: peak memory on
        // large heaps).
        g.drop_edges();
        for (i, node) in g.nodes.iter_mut().enumerate() {
            if node.live || removed[i] {
                node.handle = Handle::Taken;
            }
        }
        let to_free = g.nodes;
        drop(quiet);

        // Destructor phase.
        if !dtors.is_empty() {
            let t = Instant::now();
            let r = self.gc_call_destructors(&dtors, err, &mut res.zombies);
            with_state(|s| s.dtor_time += t.elapsed());
            r?;
        }

        // Free phase: break the cycles, then let `Rc` free the nodes (a
        // freed root leaves the buffer through `forget_root`).
        let t = Instant::now();
        let quiet = Quiet::new(providers);
        let mut trash: Vec<Value> = Vec::new();
        let mut states: Vec<Box<dyn Any>> = Vec::new();
        for node in to_free.iter() {
            match &node.handle {
                Handle::Obj(o) => clear_object(object::data(o), &mut trash, &mut states),
                Handle::Ref(r) => {
                    if let Some(mut v) = r.try_borrow_mut() {
                        trash.push(std::mem::take(&mut *v));
                    }
                }
                Handle::Arr(a) if self_garbage.contains(&a.addr()) => {
                    // SAFETY: the analysis found every handle of this table
                    // owned by garbage (nothing outside the collector can
                    // reach it), and no PHP code runs in this phase: the
                    // nodes, `trash` and `states` hold handles only, no
                    // borrow into the table.
                    let t = unsafe { a.gc_take_contents() };
                    trash.push(Value::Array(php_runtime::ZArray::from_table(t)));
                }
                Handle::Arr(_) | Handle::Taken => {}
            }
            // Released in batches (bounded memory on large heaps):
            // `to_free` still holds every node being freed, so this only
            // releases values outside it; graphs below the batch size keep
            // the single release after the loop (and its free order).
            if trash.len() >= TRASH_BATCH {
                drop(std::mem::take(&mut trash));
            }
        }
        drop(to_free);
        drop(states);
        drop(trash);
        drop(quiet);
        // The garbage slots are released and the buffer compacted (the
        // destructor objects and the roots buffered meanwhile move into
        // the holes): the re-run's order.
        with_state(|s| {
            for &i in &placeholders {
                s.roots.release_dead(i);
            }
            s.roots.compact();
        });
        with_state(|s| s.free_time += t.elapsed());
        res.count = count.max(0);
        Ok(res)
    }

    /// Calls the destructors of garbage objects in buffer order
    /// (`gc_call_destructors`), skipping objects freed or destructed in the
    /// meantime. Inside a fiber they run in a dedicated fiber
    /// (`gc_call_destructors_in_fiber`).
    fn gc_call_destructors(
        &mut self,
        dtors: &[Weak<dyn PhpObject>],
        err: &mut Option<Throw>,
        zombies: &mut Vec<Obj>,
    ) -> VmResult<()> {
        if !self.fibers.is_empty() {
            return self.gc_call_destructors_in_fiber(dtors, err, zombies);
        }
        for w in dtors {
            if let Some(z) = self.gc_destruct_one(w, err)? {
                zombies.push(z);
            }
        }
        Ok(())
    }

    /// Runs one garbage object's destructor. Exceptions are chained into
    /// `err` (`zend_objects_destroy_object`); returns the object if only the
    /// collector holds it afterwards (`GC_DELREF` to zero without freeing:
    /// the re-run frees it).
    pub(super) fn gc_destruct_one(
        &mut self,
        w: &Weak<dyn PhpObject>,
        err: &mut Option<Throw>,
    ) -> VmResult<Option<Obj>> {
        let Some(rc) = w.upgrade() else {
            return Ok(None);
        };
        let o = Obj::from_rc(rc);
        let d = object::data(&o);
        if d.flags.get() & oflags::DESTRUCTED != 0 {
            return Ok(None);
        }
        d.flags.set(d.flags.get() | oflags::DESTRUCTED);
        let r = self
            .call_destructor(&o)
            .and_then(|()| self.run_pending_destructors());
        if let Err(e) = r {
            match e {
                Throw::Exception(new) => {
                    if let Some(Throw::Exception(old)) = err.take() {
                        crate::exceptions::chain_previous(&new, &old);
                    }
                    *err = Some(Throw::Exception(new));
                }
                other => return Err(other),
            }
        }
        Ok((o.refcount() == 1).then_some(o))
    }
}

/// `free_obj` for a garbage object: empties its properties and traversed
/// native state into `trash`, and drops the `WeakMap` entries keyed by it
/// (which may hold the object itself). Also what request shutdown does to
/// every object still alive ([`crate::object::free_object_storage`]).
pub(crate) fn clear_object(d: &ObjectData, trash: &mut Vec<Value>, states: &mut Vec<Box<dyn Any>>) {
    if let Ok(mut slots) = d.slots.try_borrow_mut() {
        trash.extend(slots.iter_mut().filter_map(Option::take));
    }
    if let Ok(mut dynamic) = d.dynamic.try_borrow_mut() {
        if let Some(t) = dynamic.take() {
            trash.push(Value::Array(php_runtime::ZArray::from_table(t)));
        }
    }
    if let Ok(mut state) = d.internal.try_borrow_mut() {
        if state.as_ref().is_some_and(|s| is_traversed(&**s)) {
            states.extend(state.take());
        }
    }
    crate::weakref::object_freed(d.handle);
}

/// `gc_remove_nested_data_from_buffer`: removes the garbage reachable from
/// destructor object `start` from this pass (stopping at other destructor
/// objects and at data already removed); returns how many counted nodes
/// were removed (`start` included). The removed nodes are appended to
/// `out` in php-src's visiting order (the first child is visited next,
/// the others are stacked).
fn remove_nested(
    g: &Graph,
    start: u32,
    in_d: &[bool],
    removed: &mut [bool],
    out: &mut Vec<u32>,
) -> i64 {
    let mut count = 0;
    let mut stack: Vec<u32> = Vec::new();
    let mut x = start;
    loop {
        let xi = x as usize;
        if !removed[xi] && !g.nodes[xi].live && (x == start || !in_d[xi]) {
            removed[xi] = true;
            if g.nodes[xi].counted() {
                count += 1;
            }
            out.push(x);
            let mut children = g.children(x);
            if let Some(first) = children.next() {
                stack.extend(children);
                x = first;
                continue;
            }
        }
        match stack.pop() {
            Some(c) => x = c,
            None => break,
        }
    }
    count
}

/// The order in which `gc_collect_white` adds garbage to the root buffer
/// after the garbage roots (`roots`, in slot order): walking from each root
/// in turn, a node is added when visited, its first unvisited garbage child
/// is visited next and the others are stacked (popped last first);
/// references take no slot. Garbage no root reaches (found by the
/// object-store scan of `gc_collect_cycles()`) follows, walked the same way.
fn white_order(g: &Graph, roots: &[u32]) -> Vec<u32> {
    let n = g.nodes.len();
    let mut seen = vec![false; n];
    let mut buffered = vec![false; n];
    for &r in roots {
        buffered[r as usize] = true;
    }
    let mut added = Vec::new();
    let mut stack: Vec<u32> = Vec::new();
    let rest = (0..n)
        .filter(|&i| !g.nodes[i].live)
        .filter_map(|i| u32::try_from(i).ok());
    for start in roots.iter().copied().chain(rest) {
        if seen[start as usize] {
            continue;
        }
        seen[start as usize] = true;
        let mut x = start;
        loop {
            let xi = x as usize;
            if !buffered[xi] && g.nodes[xi].counted() {
                buffered[xi] = true;
                added.push(x);
            }
            let mut next = None;
            for c in g.children(x) {
                let ci = c as usize;
                if g.nodes[ci].live || seen[ci] {
                    continue;
                }
                seen[ci] = true;
                if next.is_none() {
                    next = Some(c);
                } else {
                    stack.push(c);
                }
            }
            match next.or_else(|| stack.pop()) {
                Some(c) => x = c,
                None => break,
            }
        }
    }
    added
}
