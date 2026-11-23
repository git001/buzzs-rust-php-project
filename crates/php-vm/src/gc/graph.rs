// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The candidate subgraph and its analysis: php-src's `gc_mark_roots`
//! (trial deletion), `gc_scan_roots` (scan black/white) and
//! `gc_collect_roots` (collect white), restated over `Rc` counts.
//!
//! Every node is held by exactly one handle owned by the graph, so
//! `strong_count - 1` is the number of references the rest of the program
//! holds. Edges are recorded with multiplicity while each node's contents
//! are enumerated (each owned handle once), giving `internal`: the
//! references coming from candidate nodes. `external = strong - 1 -
//! internal > 0` makes a node live; liveness then propagates along edges.

use super::traverse::{lookup, GcVisitor};
use crate::object::{self, ObjectData};
use php_runtime::{Obj, ObjWeak, PhpRef, Value, ZArray};
use std::collections::HashMap;

/// No node.
pub(super) const NONE: u32 = u32::MAX;

/// The graph's handle on a node (the one reference the collector owns).
pub(super) enum Handle {
    /// An object.
    Obj(Obj),
    /// An array table.
    Arr(ZArray),
    /// A PHP reference.
    Ref(PhpRef),
    /// Placeholder while the node is being expanded / after it was moved
    /// out.
    Taken,
}

/// A candidate node.
pub(super) struct Node {
    pub handle: Handle,
    /// For objects: an edge to the object was seen (objects found only
    /// through the object store were not; every reference to them is
    /// external).
    reached: bool,
    /// Children: `edges[start..start + len]`.
    start: u32,
    len: u32,
    /// References from candidate nodes (edges into this node).
    internal: u32,
    /// Must be considered live whatever the counts say (contents could
    /// not be enumerated).
    pinned: bool,
    /// Scanned black.
    pub live: bool,
}

impl Node {
    /// Whether php-src counts this node in `gc_collect_cycles()`'s result
    /// (objects and arrays; "don't count references for compatibility").
    pub fn counted(&self) -> bool {
        !matches!(self.handle, Handle::Ref(_))
    }

    /// The object handle, if this is an object reached through an edge.
    pub fn obj(&self) -> Option<&Obj> {
        match &self.handle {
            Handle::Obj(o) if self.reached => Some(o),
            _ => None,
        }
    }

    /// The object data, if this is an object.
    pub fn data(&self) -> Option<&ObjectData> {
        match &self.handle {
            Handle::Obj(o) => Some(object::data(o)),
            _ => None,
        }
    }
}

/// A `WeakMap` entry: `value` is reachable only while both the map and
/// the key are (`GC_FROM_WEAKMAP` / `GC_FROM_WEAKMAP_KEY`).
pub(super) struct CondEdge {
    pub map: u32,
    /// The key object's handle, if it is alive (resolved to a node by
    /// `analyze`).
    key_handle: Option<u32>,
    pub key: u32,
    pub value: u32,
}

/// Nodes per [`Nodes`] chunk.
const CHUNK: usize = 4096;

/// The node list, in fixed-size chunks: a graph spanning a large heap
/// grows without the reallocation copies (and the doubled capacity) of a
/// single `Vec`, which would raise the collector's peak memory by up to
/// three times the graph's size (bug77345_gc_2 under `memory_limit`).
#[derive(Default)]
pub(super) struct Nodes {
    chunks: Vec<Vec<Node>>,
    len: usize,
}

impl Nodes {
    pub fn len(&self) -> usize {
        self.len
    }

    fn push(&mut self, n: Node) {
        match self.chunks.last_mut() {
            Some(c) if c.len() < CHUNK => c.push(n),
            _ => {
                // The first chunk grows like a `Vec` (small graphs stay
                // small); later ones are allocated whole.
                #[expect(clippy::disallowed_methods, reason = "bounded: fixed size")]
                let mut c = if self.chunks.is_empty() {
                    Vec::new()
                } else {
                    Vec::with_capacity(CHUNK)
                };
                c.push(n);
                self.chunks.push(c);
            }
        }
        self.len += 1;
    }

    pub fn iter(&self) -> impl Iterator<Item = &Node> {
        self.chunks.iter().flatten()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Node> {
        self.chunks.iter_mut().flatten()
    }
}

impl std::ops::Index<usize> for Nodes {
    type Output = Node;

    fn index(&self, i: usize) -> &Node {
        &self.chunks[i / CHUNK][i % CHUNK]
    }
}

impl std::ops::IndexMut<usize> for Nodes {
    fn index_mut(&mut self, i: usize) -> &mut Node {
        &mut self.chunks[i / CHUNK][i % CHUNK]
    }
}

/// The candidate subgraph.
#[derive(Default)]
pub(super) struct Graph {
    pub nodes: Nodes,
    /// Object nodes by object handle (`NONE`: not in the graph). Handles
    /// are dense and cannot be reused while the graph holds the objects;
    /// this costs 4 bytes per handle instead of a hash entry per node.
    obj_index: Vec<u32>,
    /// Array and reference nodes by address.
    index: HashMap<usize, u32>,
    edges: Vec<u32>,
    pub cond: Vec<CondEdge>,
    /// Nodes before this index have been expanded.
    expanded: usize,
}

impl Graph {
    /// Adds a candidate root.
    pub fn add_root(&mut self, v: &Value) {
        self.intern(v);
    }

    /// Adds a live object from the object store as a candidate root.
    pub fn add_object(&mut self, o: Obj) {
        if self.obj_node(object::data(&o).handle).is_some() {
            return;
        }
        let i = self.push_node(Handle::Obj(o), false);
        self.set_obj_node(i);
    }

    /// The node of the object with handle `h` (unique among live objects).
    fn obj_node(&self, h: u32) -> Option<u32> {
        obj_node_in(&self.obj_index, h)
    }

    /// Indexes object node `i` by its handle.
    fn set_obj_node(&mut self, i: u32) {
        let Some(h) = self.nodes[i as usize].data().map(|d| d.handle as usize) else {
            return;
        };
        if self.obj_index.len() <= h {
            #[expect(
                clippy::disallowed_methods,
                reason = "bounded: handle of a live object"
            )]
            self.obj_index.resize(h + 1, NONE);
        }
        self.obj_index[h] = i;
    }

    fn push_node(&mut self, handle: Handle, reached: bool) -> u32 {
        let i = u32::try_from(self.nodes.len()).unwrap_or(NONE - 1);
        self.nodes.push(Node {
            handle,
            reached,
            start: 0,
            len: 0,
            internal: 0,
            pinned: false,
            live: false,
        });
        i
    }

    fn push(&mut self, key: usize, handle: Handle) -> u32 {
        let i = self.push_node(handle, true);
        self.index.insert(key, i);
        i
    }

    /// The node for a collectable value (creating it), or `None` for
    /// scalars, strings and resources.
    fn intern(&mut self, v: &Value) -> Option<u32> {
        match v {
            Value::Object(o) => self.intern_obj(o),
            Value::Array(a) => Some(self.intern_arr(a)),
            Value::Reference(r) => Some(self.intern_ref(r)),
            _ => None,
        }
    }

    pub(super) fn intern_obj(&mut self, o: &Obj) -> Option<u32> {
        // Objects of other `PhpObject` implementations hold nothing the
        // collector knows about: leaves.
        let d = o.downcast_ref::<ObjectData>()?;
        if let Some(i) = self.obj_node(d.handle) {
            self.nodes[i as usize].reached = true;
            return Some(i);
        }
        let i = self.push_node(Handle::Obj(o.clone()), true);
        self.set_obj_node(i);
        Some(i)
    }

    pub(super) fn intern_arr(&mut self, a: &ZArray) -> u32 {
        match self.index.get(&a.addr()) {
            Some(&i) => i,
            None => self.push(a.addr(), Handle::Arr(a.clone())),
        }
    }

    pub(super) fn intern_ref(&mut self, r: &PhpRef) -> u32 {
        match self.index.get(&r.addr()) {
            Some(&i) => i,
            None => self.push(r.addr(), Handle::Ref(r.clone())),
        }
    }

    /// Records an edge from the node being expanded to `v`.
    pub(super) fn edge(&mut self, v: &Value) {
        if let Some(t) = self.intern(v) {
            self.link(t);
        }
    }

    pub(super) fn link(&mut self, t: u32) {
        self.edges.push(t);
        self.nodes[t as usize].internal += 1;
    }

    /// Records a `WeakMap` entry of the map being expanded.
    pub(super) fn cond_edge(&mut self, map: u32, key: &ObjWeak, v: &Value) {
        if let Some(t) = self.intern(v) {
            self.nodes[t as usize].internal += 1;
            self.cond.push(CondEdge {
                map,
                key_handle: key.upgrade().map(|o| object::data(&o).handle),
                key: NONE,
                value: t,
            });
        }
    }

    /// Marks the node being expanded as live.
    pub(super) fn pin(&mut self, i: u32) {
        self.nodes[i as usize].pinned = true;
    }

    /// Enumerates the contents of every node reachable from the roots.
    pub fn build(&mut self) {
        while self.expanded < self.nodes.len() {
            let i = self.expanded;
            self.expanded += 1;
            self.expand(i);
        }
    }

    fn expand(&mut self, i: usize) {
        let handle = std::mem::replace(&mut self.nodes[i].handle, Handle::Taken);
        let start = self.edges.len();
        let me = u32::try_from(i).unwrap_or(NONE - 1);
        match &handle {
            Handle::Obj(o) => self.expand_object(me, object::data(o)),
            Handle::Arr(a) => {
                for v in a.values() {
                    self.edge(v);
                }
            }
            Handle::Ref(r) => match r.try_borrow() {
                Some(v) => self.edge(&v),
                None => self.pin(me),
            },
            Handle::Taken => {}
        }
        let n = &mut self.nodes[i];
        n.handle = handle;
        n.start = u32::try_from(start).unwrap_or(0);
        n.len = u32::try_from(self.edges.len() - start).unwrap_or(0);
    }

    fn expand_object(&mut self, me: u32, d: &ObjectData) {
        match d.slots.try_borrow() {
            Ok(slots) => {
                for v in slots.iter().flatten() {
                    self.edge(v);
                }
            }
            Err(_) => self.pin(me),
        }
        match d.dynamic.try_borrow() {
            Ok(dynamic) => {
                if let Some(t) = dynamic.as_ref() {
                    for v in t.values() {
                        self.edge(v);
                    }
                }
            }
            Err(_) => self.pin(me),
        }
        match d.internal.try_borrow() {
            Ok(state) => {
                if let Some(state) = state.as_ref() {
                    let any: &dyn std::any::Any = &**state;
                    if let Some(f) = lookup(any.type_id()) {
                        let mut v = GcVisitor::new(self, me);
                        f(any, &mut v);
                    }
                }
            }
            // Native code is working on the state right now.
            Err(_) => self.pin(me),
        }
    }

    /// Scan: decides which nodes are live. Returns the number of garbage
    /// nodes.
    pub fn analyze(&mut self) -> usize {
        for c in &mut self.cond {
            c.key = c
                .key_handle
                .and_then(|h| obj_node_in(&self.obj_index, h))
                .unwrap_or(NONE);
        }
        let mut work: Vec<u32> = Vec::new();
        for (i, n) in self.nodes.iter_mut().enumerate() {
            let live = n.pinned || external_refs(n) != Some(0);
            if live {
                n.live = true;
                work.push(u32::try_from(i).unwrap_or(0));
            }
        }
        loop {
            while let Some(i) = work.pop() {
                let n = &self.nodes[i as usize];
                let (s, l) = (n.start as usize, n.len as usize);
                for k in s..s + l {
                    let t = self.edges[k];
                    let tn = &mut self.nodes[t as usize];
                    if !tn.live {
                        tn.live = true;
                        work.push(t);
                    }
                }
            }
            // A `WeakMap` value is live when both its map and its key are.
            for c in &self.cond {
                let key_live = c.key == NONE || self.nodes[c.key as usize].live;
                if key_live && self.nodes[c.map as usize].live && !self.nodes[c.value as usize].live
                {
                    self.nodes[c.value as usize].live = true;
                    work.push(c.value);
                }
            }
            if work.is_empty() {
                break;
            }
        }
        self.nodes.iter().filter(|n| !n.live).count()
    }

    /// Releases the edges and indexes once the analysis and the nested-data
    /// removal are done ([`Graph::children`] is empty afterwards).
    pub fn drop_edges(&mut self) {
        self.edges = Vec::new();
        self.cond = Vec::new();
        self.obj_index = Vec::new();
        self.index = HashMap::new();
        for n in self.nodes.iter_mut() {
            n.start = 0;
            n.len = 0;
        }
    }

    /// Children of `i` (edges plus the `WeakMap` values it holds or keys).
    pub fn children(&self, i: u32) -> impl Iterator<Item = u32> + '_ {
        let n = &self.nodes[i as usize];
        let (s, l) = (n.start as usize, n.len as usize);
        self.edges[s..s + l].iter().copied().chain(
            self.cond
                .iter()
                .filter(move |c| c.map == i || c.key == i)
                .map(|c| c.value),
        )
    }
}

/// [`Graph::obj_node`] over the index alone (while `cond` is borrowed).
fn obj_node_in(obj_index: &[u32], h: u32) -> Option<u32> {
    obj_index.get(h as usize).copied().filter(|&i| i != NONE)
}

/// References to `n` from outside the candidate subgraph, or `None` when
/// they cannot be determined (the node is then live).
fn external_refs(n: &Node) -> Option<usize> {
    let strong = match &n.handle {
        // Reached through no edge: every reference to it is external.
        Handle::Obj(_) if !n.reached => return None,
        Handle::Obj(o) => o.refcount(),
        Handle::Arr(a) => a.refcount(),
        Handle::Ref(r) => r.refcount(),
        Handle::Taken => return None,
    };
    // The graph holds one handle; a negative remainder would mean an edge
    // was enumerated twice — treat as live.
    let internal = n.internal as usize;
    strong.checked_sub(1 + internal).or(Some(1))
}
