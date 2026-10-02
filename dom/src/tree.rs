//! Arena-based DOM tree.
//!
//! See the crate level documentation for the design rationale.

use markup5ever::{local_name, ns, LocalName, QualName};

/// Handle to a node inside a [`Dom`].
///
/// This is an index into the internal slot arena. Node ids are only valid for
/// the `Dom` they were created from; the [`Dom::generation`] counter lets
/// callers detect stale ids after node removal.
pub type NodeId = u32;

/// Quirks mode of a document, as determined by the HTML parser.
pub use markup5ever::interface::QuirksMode;

/// A single attribute of an element.
///
/// Attribute names are namespace-less atoms; values are plain strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    /// Attribute name.
    pub name: LocalName,
    /// Attribute value.
    pub value: String,
}

/// The data of an element node.
#[derive(Debug, Clone)]
pub struct Element {
    /// Qualified (namespaced) tag name.
    pub name: QualName,
    /// Attributes in source order.
    pub attrs: Vec<Attr>,
    /// Cached value of the `id` attribute (if any) for fast lookups.
    pub id: Option<String>,
    /// Cached whitespace-split `class` list.
    pub classes: Vec<String>,
}

impl Element {
    /// Local (unprefixed) tag name, e.g. `"div"`.
    pub fn local_name(&self) -> &LocalName {
        &self.name.local
    }

    /// Returns true when the element is in the HTML namespace.
    pub fn is_html(&self) -> bool {
        self.name.ns == ns!(html)
    }

    /// Namespace of the element, e.g. `ns!(html)` or `ns!(svg)`.
    pub fn namespace(&self) -> &markup5ever::Namespace {
        &self.name.ns
    }
}

/// The kind and data of a DOM node.
#[derive(Debug, Clone)]
pub enum NodeKind {
    /// The document node (root of the tree).
    Document,
    /// A `<!DOCTYPE ...>` node.
    Doctype {
        /// DOCTYPE name.
        name: String,
        /// Public identifier.
        public_id: String,
        /// System identifier.
        system_id: String,
    },
    /// An element node.
    Element(Element),
    /// A text node.
    Text(String),
    /// A comment node.
    Comment(String),
}

impl NodeKind {
    /// Returns `Some(element)` for element nodes.
    pub fn as_element(&self) -> Option<&Element> {
        match self {
            NodeKind::Element(e) => Some(e),
            _ => None,
        }
    }

    /// Returns `Some(text)` for text nodes.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            NodeKind::Text(t) => Some(t),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct Slot {
    parent: Option<NodeId>,
    first_child: Option<NodeId>,
    last_child: Option<NodeId>,
    prev_sibling: Option<NodeId>,
    next_sibling: Option<NodeId>,
    kind: NodeKind,
}

/// An entire DOM tree.
///
/// The `Dom` owns every node. All structural operations are `O(1)` apart from
/// tree walks. Mutations bump [`Dom::version`] so downstream stages can
/// detect invalidation cheaply.
#[derive(Debug)]
pub struct Dom {
    slots: Vec<Slot>,
    /// Generation per slot; bumped whenever the slot is recycled. Used to
    /// validate externally-held handles (e.g. from JavaScript).
    generations: Vec<u32>,
    free: Vec<NodeId>,
    document: NodeId,
    /// Monotonic mutation counter.
    pub version: u64,
    /// Document quirks mode.
    pub quirks: QuirksMode,
    /// Overflow sentinel: returned by `alloc` once the node cap is hit.
    /// Runaway page scripts (a 10s watchdog window of unbounded
    /// appendChild) otherwise grow the tree to millions of nodes and the
    /// next layout wedges the page thread for hours. All post-cap
    /// allocations alias this one detached node, keeping the tree bounded.
    overflow: NodeId,
    /// `<template>` elements → their detached contents holder node.
    /// Populated by the HTML parser; the holder lives outside the document
    /// tree so ordinary walks never see template content.
    template_contents: std::collections::HashMap<NodeId, NodeId>,
    /// Shadow-DOM hosts → their detached shadow root node (one per host,
    /// v1). The root is a comment-kind holder whose subtree is the shadow
    /// tree; `flat_children` composes it back in.
    shadow_roots: std::collections::HashMap<NodeId, NodeId>,
    /// Reverse map (shadow root → host) for flat-tree parent resolution.
    shadow_hosts: std::collections::HashMap<NodeId, NodeId>,
}

/// Hard cap on live DOM nodes (real pages use 5–20k; heavy JS hydration
/// can legitimately reach ~50k). Beyond this, allocations degrade to the
/// overflow sentinel.
pub const MAX_NODES: usize = 100_000;

impl Default for Dom {
    fn default() -> Self {
        Self::new()
    }
}

impl Dom {
    /// Creates an empty document.
    pub fn new() -> Self {
        let mut dom = Dom {
            slots: Vec::with_capacity(256),
            generations: Vec::with_capacity(256),
            free: Vec::new(),
            document: 0,
            version: 1,
            quirks: QuirksMode::NoQuirks,
            overflow: 0,
            template_contents: std::collections::HashMap::new(),
            shadow_roots: std::collections::HashMap::new(),
            shadow_hosts: std::collections::HashMap::new(),
        };
        dom.document = dom.alloc(NodeKind::Document);
        dom.overflow = dom.create_html_element("rowser-overflow");
        dom
    }

    /// Node id of the document node.
    pub fn document(&self) -> NodeId {
        self.document
    }

    /// Number of live nodes (approximates DOM size for memory accounting).
    pub fn node_count(&self) -> usize {
        self.slots.len() - self.free.len()
    }

    /// Generation of the slot at `id`; used to validate handles.
    pub fn generation(&self, id: NodeId) -> Option<u32> {
        self.generations.get(id as usize).copied()
    }

    /// True when `id` currently refers to a live (non-recycled) node.
    pub fn is_valid(&self, id: NodeId) -> bool {
        let idx = id as usize;
        idx < self.slots.len() && self.free.binary_search(&id).is_err()
    }

    fn alloc(&mut self, kind: NodeKind) -> NodeId {
        self.version += 1;
        if self.slots.len() - self.free.len() >= MAX_NODES {
            // Document + overflow are exempt so the sentinel always exists.
            return self.overflow;
        }
        if let Some(id) = self.free.pop() {
            let idx = id as usize;
            self.generations[idx] = self.generations[idx].wrapping_add(1);
            self.slots[idx] = Slot {
                parent: None,
                first_child: None,
                last_child: None,
                prev_sibling: None,
                next_sibling: None,
                kind,
            };
            id
        } else {
            self.slots.push(Slot {
                parent: None,
                first_child: None,
                last_child: None,
                prev_sibling: None,
                next_sibling: None,
                kind,
            });
            self.generations.push(0);
            (self.slots.len() - 1) as NodeId
        }
    }

    fn free_slot(&mut self, id: NodeId) {
        let idx = id as usize;
        self.slots[idx].kind = NodeKind::Comment(String::new());
        let pos = self.free.binary_search(&id).unwrap_or_else(|e| e);
        self.free.insert(pos, id);
        self.version += 1;
    }

    /// Kind/data of the node; panics for stale ids (internal invariant).
    pub fn kind(&self, id: NodeId) -> &NodeKind {
        &self.slots[id as usize].kind
    }

    /// Element data for the node, or `None` if it is not an element.
    pub fn element(&self, id: NodeId) -> Option<&Element> {
        self.kind(id).as_element()
    }

    /// Text of a text node, or `None`.
    pub fn text(&self, id: NodeId) -> Option<&str> {
        self.kind(id).as_text()
    }

    /// Parent of the node (`None` for the document and detached nodes).
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.slots[id as usize].parent
    }

    /// First child of the node.
    pub fn first_child(&self, id: NodeId) -> Option<NodeId> {
        self.slots[id as usize].first_child
    }

    /// Last child of the node.
    pub fn last_child(&self, id: NodeId) -> Option<NodeId> {
        self.slots[id as usize].last_child
    }

    /// Next sibling of the node.
    pub fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.slots[id as usize].next_sibling
    }

    /// Previous sibling of the node.
    pub fn prev_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.slots[id as usize].prev_sibling
    }

    /// Creates an element node (unattached).
    pub fn create_element(&mut self, name: QualName, attrs: Vec<Attr>) -> NodeId {
        let mut element = Element {
            name,
            attrs,
            id: None,
            classes: Vec::new(),
        };
        refresh_caches(&mut element);
        self.alloc(NodeKind::Element(element))
    }

    /// Creates an unattached element in the HTML namespace by tag name.
    pub fn create_html_element(&mut self, local: &str) -> NodeId {
        let name = QualName {
            prefix: None,
            ns: ns!(html),
            local: LocalName::from(local),
        };
        self.create_element(name, Vec::new())
    }

    /// Creates a text node (unattached).
    pub fn create_text(&mut self, text: impl Into<String>) -> NodeId {
        self.alloc(NodeKind::Text(text.into()))
    }

    /// Creates a comment node (unattached).
    pub fn create_comment(&mut self, text: impl Into<String>) -> NodeId {
        self.alloc(NodeKind::Comment(text.into()))
    }

    /// Creates a doctype node (unattached).
    pub fn create_doctype(&mut self, name: &str, public_id: &str, system_id: &str) -> NodeId {
        self.alloc(NodeKind::Doctype {
            name: name.to_owned(),
            public_id: public_id.to_owned(),
            system_id: system_id.to_owned(),
        })
    }

    /// Appends `child` as the last child of `parent`, detaching it from any
    /// previous parent first. Adjacent text nodes are merged.
    pub fn append(&mut self, parent: NodeId, child: NodeId) {
        if parent == child {
            return;
        }
        self.detach(child);
        if self.is_text(child) {
            if let Some(last) = self.slots[parent as usize].last_child {
                if last != child && self.is_text(last) {
                    // Merge into the previous text sibling.
                    let moved = match std::mem::replace(
                        &mut self.slots[child as usize].kind,
                        NodeKind::Comment(String::new()),
                    ) {
                        NodeKind::Text(t) => t,
                        other => other_text_or_default(other),
                    };
                    if let NodeKind::Text(t) = &mut self.slots[last as usize].kind {
                        t.push_str(&moved);
                    }
                    self.free_slot(child);
                    return;
                }
            }
        }
        self.link_append(parent, child);
        self.version += 1;
    }

    /// Appends a fresh run of text to `parent`, merging with a trailing text
    /// child when present. Returns the (new or merged) text node.
    pub fn append_text(&mut self, parent: NodeId, text: &str) -> NodeId {
        if text.is_empty() {
            // Still create an (empty) text node to mirror the HTML parser.
            let id = self.create_text(text);
            self.link_append(parent, id);
            return id;
        }
        if let Some(last) = self.slots[parent as usize].last_child {
            if self.is_text(last) {
                if let NodeKind::Text(t) = &mut self.slots[last as usize].kind {
                    t.push_str(text);
                }
                self.version += 1;
                return last;
            }
        }
        let id = self.create_text(text);
        self.link_append(parent, id);
        self.version += 1;
        id
    }

    /// Inserts `child` into `parent` immediately before `ref_child`
    /// (`None` appends). Adjacent text nodes are merged.
    pub fn insert_before(&mut self, parent: NodeId, child: NodeId, ref_child: Option<NodeId>) {
        let Some(reference) = ref_child else {
            self.append(parent, child);
            return;
        };
        if reference == child {
            return;
        }
        self.detach(child);
        if self.is_text(child) {
            let prev = self.slots[reference as usize].prev_sibling;
            if let Some(prev) = prev.filter(|p| self.is_text(*p) && *p != child) {
                let moved = match std::mem::replace(
                    &mut self.slots[child as usize].kind,
                    NodeKind::Comment(String::new()),
                ) {
                    NodeKind::Text(t) => t,
                    other => other_text_or_default(other),
                };
                if let NodeKind::Text(t) = &mut self.slots[prev as usize].kind {
                    t.push_str(&moved);
                }
                self.free_slot(child);
                return;
            }
        }
        let prev = self.slots[reference as usize].prev_sibling;
        self.slots[child as usize].parent = Some(parent);
        self.slots[child as usize].prev_sibling = prev;
        self.slots[child as usize].next_sibling = Some(reference);
        if let Some(p) = prev {
            self.slots[p as usize].next_sibling = Some(child);
        } else {
            self.slots[parent as usize].first_child = Some(child);
        }
        self.slots[reference as usize].prev_sibling = Some(child);
        self.version += 1;
    }

    /// Detaches `node` from its parent (the node stays alive and can be
    /// re-attached).
    pub fn detach(&mut self, node: NodeId) {
        let (parent, prev, next) = {
            let s = &self.slots[node as usize];
            (s.parent, s.prev_sibling, s.next_sibling)
        };
        if parent.is_none() && prev.is_none() && next.is_none() {
            return;
        }
        if let Some(p) = prev {
            self.slots[p as usize].next_sibling = next;
        }
        if let Some(n) = next {
            self.slots[n as usize].prev_sibling = prev;
        }
        if let Some(par) = parent {
            if self.slots[par as usize].first_child == Some(node) {
                self.slots[par as usize].first_child = next;
            }
            if self.slots[par as usize].last_child == Some(node) {
                self.slots[par as usize].last_child = prev;
            }
        }
        let s = &mut self.slots[node as usize];
        s.parent = None;
        s.prev_sibling = None;
        s.next_sibling = None;
        self.version += 1;
    }

    /// Removes `node` and its whole subtree from the document and frees the
    /// slots for reuse.
    pub fn remove_subtree(&mut self, node: NodeId) {
        let children: Vec<NodeId> = self.children(node).collect();
        for child in children {
            self.remove_subtree(child);
        }
        self.detach(node);
        self.free_slot(node);
    }

    /// Moves all children of `from` to the end of `to`'s child list.
    pub fn reparent_children(&mut self, from: NodeId, to: NodeId) {
        let children: Vec<NodeId> = self.children(from).collect();
        for child in children {
            self.detach(child);
            // No text merging here: HTML tree building relies on order
            // preservation.
            self.link_append(to, child);
        }
        self.slots[from as usize].first_child = None;
        self.slots[from as usize].last_child = None;
        self.version += 1;
    }

    fn link_append(&mut self, parent: NodeId, child: NodeId) {
        let prev = self.slots[parent as usize].last_child;
        self.slots[child as usize].parent = Some(parent);
        self.slots[child as usize].prev_sibling = prev;
        self.slots[child as usize].next_sibling = None;
        match prev {
            Some(p) => self.slots[p as usize].next_sibling = Some(child),
            None => self.slots[parent as usize].first_child = Some(child),
        }
        self.slots[parent as usize].last_child = Some(child);
    }

    fn is_text(&self, id: NodeId) -> bool {
        matches!(self.kind(id), NodeKind::Text(_))
    }

    /// Iterates the children of `id` in order.
    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            dom: self,
            next: self.first_child(id),
        }
    }

    /// Iterates `root`'s subtree in document order, **excluding** `root`.
    pub fn descendants(&self, root: NodeId) -> Descendants<'_> {
        // Seed the stack with the full sibling chain of `root`'s direct
        // children in reverse document order (documents commonly have several
        // root-level children: doctype, comments and the `<html>` element).
        let mut stack = Vec::new();
        let mut child = self.last_child(root);
        while let Some(id) = child {
            stack.push(id);
            child = self.prev_sibling(id);
        }
        Descendants { dom: self, stack }
    }

    /// Iterates all element descendants of `root` (excluding `root` unless it
    /// is an element itself passed via [`Dom::subtree_elements`]).
    pub fn elements(&self, root: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.descendants(root)
            .filter(|id| self.element(*id).is_some())
    }

    /// Iterates the element subtree of `root` **including** `root` itself.
    pub fn subtree_elements(&self, root: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        std::iter::once(root)
            .chain(self.descendants(root))
            .filter(|id| self.element(*id).is_some())
    }

    /// First element child of `id`.
    pub fn first_element_child(&self, id: NodeId) -> Option<NodeId> {
        self.children(id).find(|c| self.element(*c).is_some())
    }

    /// Nearest ancestor that is an element.
    pub fn parent_element(&self, id: NodeId) -> Option<NodeId> {
        let mut cur = self.parent(id);
        while let Some(p) = cur {
            if self.element(p).is_some() {
                return Some(p);
            }
            cur = self.parent(p);
        }
        None
    }

    /// Nearest preceding sibling that is an element.
    pub fn prev_sibling_element(&self, id: NodeId) -> Option<NodeId> {
        let mut cur = self.prev_sibling(id);
        while let Some(p) = cur {
            if self.element(p).is_some() {
                return Some(p);
            }
            cur = self.prev_sibling(p);
        }
        None
    }

    /// Nearest following sibling that is an element.
    pub fn next_sibling_element(&self, id: NodeId) -> Option<NodeId> {
        let mut cur = self.next_sibling(id);
        while let Some(n) = cur {
            if self.element(n).is_some() {
                return Some(n);
            }
            cur = self.next_sibling(n);
        }
        None
    }

    /// Looks up an attribute value by name.
    pub fn get_attr(&self, id: NodeId, name: &str) -> Option<&str> {
        self.element(id)?
            .attrs
            .iter()
            .find(|a| &*a.name == name)
            .map(|a| a.value.as_str())
    }

    /// Sets an attribute (creating it when missing), refreshing caches.
    pub fn set_attr(&mut self, id: NodeId, name: &str, value: &str) {
        let Some(NodeKind::Element(el)) = self.slots.get_mut(id as usize).map(|s| &mut s.kind)
        else {
            return;
        };
        let atom = LocalName::from(name);
        if let Some(attr) = el.attrs.iter_mut().find(|a| a.name == atom) {
            attr.value = value.to_owned();
        } else {
            el.attrs.push(Attr {
                name: atom,
                value: value.to_owned(),
            });
        }
        let mut cloned = el.clone();
        refresh_caches(&mut cloned);
        if let NodeKind::Element(el) = &mut self.slots[id as usize].kind {
            *el = cloned;
        }
        self.version += 1;
    }

    /// Removes an attribute, refreshing caches.
    pub fn remove_attr(&mut self, id: NodeId, name: &str) {
        let Some(NodeKind::Element(el)) = self.slots.get_mut(id as usize).map(|s| &mut s.kind)
        else {
            return;
        };
        let atom = LocalName::from(name);
        el.attrs.retain(|a| a.name != atom);
        let mut cloned = el.clone();
        refresh_caches(&mut cloned);
        if let NodeKind::Element(el) = &mut self.slots[id as usize].kind {
            *el = cloned;
        }
        self.version += 1;
    }

    /// Replaces the text of a text node.
    pub fn set_text(&mut self, id: NodeId, text: &str) {
        if let Some(slot) = self.slots.get_mut(id as usize) {
            if matches!(slot.kind, NodeKind::Text(_)) {
                slot.kind = NodeKind::Text(text.to_owned());
                self.version += 1;
            }
        }
    }

    /// Concatenated descendant text (the `textContent` semantics).
    pub fn text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.collect_text(id, &mut out);
        out
    }

    fn collect_text(&self, id: NodeId, out: &mut String) {
        if let NodeKind::Text(t) = self.kind(id) {
            out.push_str(t);
        }
        for child in self.children(id) {
            self.collect_text(child, out);
        }
    }

    // ------------------------------------------------------------------
    // WebComponents: templates, shadow DOM, flat tree, cloning, serialization

    /// Records the contents holder of a `<template>` (used by the parser).
    pub fn set_template_contents(&mut self, template: NodeId, contents: NodeId) {
        self.template_contents.insert(template, contents);
    }

    /// The detached contents holder of a `<template>`, if `id` is one.
    pub fn template_contents(&self, id: NodeId) -> Option<NodeId> {
        self.template_contents.get(&id).copied()
    }

    /// Creates a shadow root for `host` and returns it. Returns `None` when
    /// the host already has one (spec: `attachShadow` throws — v1 answers
    /// by refusing).
    pub fn attach_shadow(&mut self, host: NodeId) -> Option<NodeId> {
        if self.shadow_roots.contains_key(&host) || self.element(host).is_none() {
            return None;
        }
        let root = self.create_comment("shadow-root");
        self.shadow_roots.insert(host, root);
        self.shadow_hosts.insert(root, host);
        self.version += 1;
        Some(root)
    }

    /// The shadow root attached to `host`, if any.
    pub fn shadow_root(&self, host: NodeId) -> Option<NodeId> {
        self.shadow_roots.get(&host).copied()
    }

    /// The host element owning shadow root `root` (flat-tree parent jump).
    pub fn shadow_host(&self, root: NodeId) -> Option<NodeId> {
        self.shadow_hosts.get(&root).copied()
    }

    /// All shadow root holder nodes currently attached (for cascade and
    /// style collection walks).
    pub fn all_shadow_roots(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.shadow_hosts.keys().copied()
    }

    /// True when `id` is a shadow root holder node.
    pub fn is_shadow_root(&self, id: NodeId) -> bool {
        self.shadow_hosts.contains_key(&id)
    }

    /// True when the parent chain of `id` reaches this document's root —
    /// the `Node.isConnected` semantics.
    pub fn is_connected(&self, id: NodeId) -> bool {
        let mut cur = Some(id);
        while let Some(n) = cur {
            if n == self.document {
                return true;
            }
            cur = self.parent(n);
        }
        false
    }

    /// All children of `id` (elements and text) as a vector — the JS
    /// `childNodes` bridge.
    pub fn child_handles(&self, id: NodeId) -> Vec<NodeId> {
        self.children(id).collect()
    }

    /// The **flat tree** children of `id` (shadow DOM composed).
    ///
    /// * An element with a shadow root renders its *shadow* children
    ///   instead of its light children; light children reappear only via
    ///   `<slot>` assignment (matching `name` attrs; unnamed slot takes the
    ///   unslotted children), and a slot with no assignment renders its
    ///   fallback content.
    /// * Plain nodes return their ordinary children.
    pub fn flat_children(&self, id: NodeId) -> Vec<NodeId> {
        let Some(root) = self.shadow_roots.get(&id).copied() else {
            return self.children(id).collect();
        };
        let mut out = Vec::new();
        for child in self.children(root) {
            let is_slot = self
                .element(child)
                .is_some_and(|e| &*e.name.local == "slot");
            if is_slot {
                let slot_name = self.get_attr(child, "name");
                let assigned: Vec<NodeId> = self
                    .children(id)
                    .filter(|light| {
                        let light_slot = self.get_attr(*light, "slot");
                        match (slot_name, light_slot) {
                            (None, None) => true, // default slot
                            (Some(n), Some(ls)) => *n == *ls,
                            _ => false,
                        }
                    })
                    .collect();
                if !assigned.is_empty() {
                    out.extend(assigned);
                } else {
                    // Slot fallback content.
                    out.extend(self.children(child));
                }
            } else {
                out.push(child);
            }
        }
        out
    }

    /// Flat-tree parent (composed parent): the host for shadow-root
    /// children, the ordinary parent otherwise. Used for style
    /// inheritance and event paths.
    pub fn flat_parent_element(&self, id: NodeId) -> Option<NodeId> {
        let parent = self.parent(id)?;
        if let Some(host) = self.shadow_hosts.get(&parent) {
            return Some(*host);
        }
        if self.element(parent).is_some() {
            return Some(parent);
        }
        self.parent_element(parent)
    }

    /// Deep-clones the subtree rooted at `id` into a fresh set of nodes
    /// (same arena) and returns the clone of `id`. Template contents and
    /// shadow trees are NOT copied (per spec: clones carry neither).
    pub fn clone_subtree(&mut self, id: NodeId) -> NodeId {
        let kind = self.kind(id).clone();
        let clone = match &kind {
            NodeKind::Element(el) => self.create_element(el.name.clone(), el.attrs.clone()),
            NodeKind::Text(t) => self.create_text(t.clone()),
            NodeKind::Comment(c) => self.create_comment(c.clone()),
            NodeKind::Document | NodeKind::Doctype { .. } => {
                // Cloning a document node yields a fragment-ish container.
                self.create_comment("clone")
            }
        };
        if let NodeKind::Element(_) = &kind {
            // `template` clones are inert: per spec cloneNode does not copy
            // content, but the template element carries an empty contents
            // holder so `clone.content` stays usable.
            if let Some(contents) = self.template_contents.get(&id).copied() {
                let new_contents = self.create_comment("template-contents");
                self.set_template_contents(clone, new_contents);
                let _ = contents;
            }
        }
        let children: Vec<NodeId> = self.children(id).collect();
        for child in children {
            let child_clone = self.clone_subtree(child);
            self.append(clone, child_clone);
        }
        clone
    }

    /// Imports a subtree from a *different* `Dom` arena into this one
    /// (deep copy) and returns the new root. Used by `innerHTML` (parse a
    /// standalone fragment, then import its nodes) and `importNode`.
    pub fn import_subtree(&mut self, other: &Dom, id: NodeId) -> NodeId {
        let kind = other.kind(id).clone();
        let local = match &kind {
            NodeKind::Element(el) => self.create_element(el.name.clone(), el.attrs.clone()),
            NodeKind::Text(t) => self.create_text(t.clone()),
            NodeKind::Comment(c) => self.create_comment(c.clone()),
            NodeKind::Document | NodeKind::Doctype { .. } => self.create_comment("import"),
        };
        // Template contents travel with the template on import (innerHTML
        // round-trips must keep `<template>` inert content usable).
        if let NodeKind::Element(el) = &kind {
            if &*el.name.local == "template" {
                if let Some(src_contents) = other.template_contents.get(&id) {
                    let dst_contents = self.create_comment("template-contents");
                    self.set_template_contents(local, dst_contents);
                    let src_children: Vec<NodeId> = other.children(*src_contents).collect();
                    for child in src_children {
                        let child_clone = self.import_subtree(other, child);
                        self.append(dst_contents, child_clone);
                    }
                }
            }
        }
        let children: Vec<NodeId> = other.children(id).collect();
        for child in children {
            let child_clone = self.import_subtree(other, child);
            self.append(local, child_clone);
        }
        local
    }

    /// Serializes the subtree of `id` as HTML (the `innerHTML` semantics).
    pub fn serialize_subtree(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.serialize_node(id, &mut out);
        out
    }

    fn serialize_node(&self, id: NodeId, out: &mut String) {
        match self.kind(id) {
            NodeKind::Text(t) => {
                for c in t.chars() {
                    match c {
                        '&' => out.push_str("&amp;"),
                        '<' => out.push_str("&lt;"),
                        '>' => out.push_str("&gt;"),
                        _ => out.push(c),
                    }
                }
            }
            NodeKind::Comment(c) => {
                out.push_str("<!--");
                out.push_str(c);
                out.push_str("-->");
            }
            NodeKind::Doctype { name, .. } => {
                out.push_str("<!DOCTYPE ");
                out.push_str(name);
                out.push('>');
            }
            NodeKind::Document => {
                for child in self.children(id) {
                    self.serialize_node(child, out);
                }
            }
            NodeKind::Element(el) => {
                let tag = el.name.local.to_string();
                out.push('<');
                out.push_str(&tag);
                for attr in &el.attrs {
                    out.push(' ');
                    out.push_str(&attr.name);
                    out.push_str("=\"");
                    for c in attr.value.chars() {
                        match c {
                            '&' => out.push_str("&amp;"),
                            '"' => out.push_str("&quot;"),
                            '<' => out.push_str("&lt;"),
                            _ => out.push(c),
                        }
                    }
                    out.push('"');
                }
                out.push('>');
                if !Self::VOID_ELEMENTS.contains(&tag.as_str()) {
                    // <template>: serialize the detached contents holder's
                    // children (innerHTML round-trips must preserve them).
                    if tag == "template" {
                        if let Some(contents) = self.template_contents.get(&id) {
                            for child in self.children(*contents) {
                                self.serialize_node(child, out);
                            }
                        }
                    } else {
                        for child in self.children(id) {
                            self.serialize_node(child, out);
                        }
                    }
                    out.push_str("</");
                    out.push_str(&tag);
                    out.push('>');
                }
            }
        }
    }

    /// Elements whose content model is empty — never serialized with a
    /// closing tag, never given children by the serializer.
    const VOID_ELEMENTS: &'static [&'static str] = &[
        "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
        "source", "track", "wbr",
    ];

    /// Finds the first element with the given `id` in `root`'s subtree.
    pub fn find_by_id(&self, root: NodeId, id: &str) -> Option<NodeId> {
        self.subtree_elements(root)
            .find(|n| self.element(*n).and_then(|e| e.id.as_deref()) == Some(id))
    }

    /// Frees nodes that became unreachable from the document or any pinned
    /// root. Called by the engine after JS-heavy mutations to emulate DOM
    /// garbage collection.
    pub fn sweep(&mut self, pins: &[NodeId]) {
        let mut reachable = vec![false; self.slots.len()];
        let mut stack = vec![self.document];
        stack.extend(
            pins.iter()
                .copied()
                .chain(pins.iter().flat_map(|p| self.first_child(*p).into_iter())),
        );
        // Pin whole pinned subtrees.
        for &pin in pins {
            if self.is_valid(pin) && (pin as usize) < reachable.len() {
                reachable[pin as usize] = true;
            }
            for n in self.descendants(pin) {
                if self.is_valid(n) {
                    reachable[n as usize] = true;
                }
            }
        }
        while let Some(n) = stack.pop() {
            if (n as usize) < reachable.len() {
                reachable[n as usize] = true;
            }
            for child in self.children(n) {
                stack.push(child);
            }
        }
        let garbage: Vec<NodeId> = (0..self.slots.len() as NodeId)
            .filter(|id| self.is_valid(*id) && !reachable[*id as usize])
            .collect();
        for id in garbage {
            self.detach(id);
            self.free_slot(id);
        }
    }
}

fn other_text_or_default(kind: NodeKind) -> String {
    match kind {
        NodeKind::Text(t) => t,
        _ => String::new(),
    }
}

fn refresh_caches(element: &mut Element) {
    element.id = element
        .attrs
        .iter()
        .find(|a| a.name == local_name!("id"))
        .map(|a| a.value.clone());
    element.classes = element
        .attrs
        .iter()
        .find(|a| a.name == local_name!("class"))
        .map(|a| a.value.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
}

/// Iterator over the children of a node.
#[derive(Debug)]
pub struct Children<'d> {
    dom: &'d Dom,
    next: Option<NodeId>,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        let cur = self.next?;
        self.next = self.dom.next_sibling(cur);
        Some(cur)
    }
}

/// Pre-order iterator over a subtree (excluding the root).
#[derive(Debug)]
pub struct Descendants<'d> {
    dom: &'d Dom,
    stack: Vec<NodeId>,
}

impl Iterator for Descendants<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        let cur = self.stack.pop()?;
        let mut child = self.dom.last_child(cur);
        while let Some(id) = child {
            self.stack.push(id);
            child = self.dom.prev_sibling(id);
        }
        Some(cur)
    }
}

#[cfg(test)]
mod wc_tests {
    use super::*;

    /// Template contents are recorded and stay invisible to document walks.
    #[test]
    fn template_contents_recorded() {
        let mut dom = Dom::new();
        let template = dom.create_html_element("template");
        dom.append(dom.document(), template);
        let contents = dom.template_contents(template);
        assert!(
            contents.is_none(),
            "no contents until set_template_contents"
        );
        let holder = dom.create_comment("template-contents");
        dom.set_template_contents(template, holder);
        let inner = dom.create_html_element("b");
        dom.append(holder, inner);
        assert_eq!(dom.template_contents(template), Some(holder));
        assert!(!dom.is_connected(holder));
        // Document walk sees the template element but not its contents.
        let seen: Vec<String> = dom
            .subtree_elements(dom.document())
            .filter_map(|n| dom.element(n).map(|e| e.name.local.to_string()))
            .collect();
        assert!(seen.contains(&"template".to_string()));
        assert!(!seen.contains(&"b".to_string()));
    }

    /// attachShadow + flat_children: shadow children compose in place of
    /// light children; slots pull light children back in.
    #[test]
    fn shadow_flat_tree() {
        let mut dom = Dom::new();
        let host = dom.create_html_element("my-el");
        dom.append(dom.document(), host);
        let light_named = dom.create_html_element("span");
        dom.set_attr(light_named, "slot", "a");
        dom.append(host, light_named);
        // Before attach: flat children are the light children.
        assert_eq!(dom.flat_children(host), vec![light_named]);
        let root = dom.attach_shadow(host).expect("attach");
        let s1 = dom.create_html_element("div");
        let slot = dom.create_html_element("slot");
        dom.set_attr(slot, "name", "a");
        dom.append(root, s1);
        dom.append(root, slot);
        // Composed: div + the slotted <span slot=a>.
        let flat = dom.flat_children(host);
        assert_eq!(flat, vec![s1, light_named]);
        // Shadow host resolution.
        assert_eq!(dom.shadow_host(root), Some(host));
        assert_eq!(dom.flat_parent_element(s1), Some(host));
        // A second attachShadow is refused.
        assert!(dom.attach_shadow(host).is_none());
    }

    /// Unnamed slot collects light children with no slot attribute;
    /// named slots fall back to their own children.
    #[test]
    fn default_slot_assignment() {
        let mut dom = Dom::new();
        let host = dom.create_html_element("my-el");
        dom.append(dom.document(), host);
        let unslotted = dom.create_text("X");
        dom.append(host, unslotted);
        let root = dom.attach_shadow(host).unwrap();
        let slot = dom.create_html_element("slot");
        dom.append(root, slot);
        assert_eq!(dom.flat_children(host), vec![unslotted]);
        // Named slot with no assignment renders fallback content.
        let named = dom.create_html_element("slot");
        dom.set_attr(named, "name", "zzz");
        let fb = dom.create_text("fallback");
        dom.append(named, fb);
        dom.append(root, named);
        let flat = dom.flat_children(host);
        assert_eq!(flat, vec![unslotted, fb]);
    }

    /// clone_subtree deep-clones elements, attributes and text.
    #[test]
    fn clone_subtree_copies() {
        let mut dom = Dom::new();
        let list = dom.create_html_element("ul");
        dom.append(dom.document(), list);
        let li = dom.create_html_element("li");
        dom.set_attr(li, "class", "a");
        dom.append(list, li);
        let text = dom.create_text("one");
        dom.append(li, text);
        let clone = dom.clone_subtree(list);
        assert_ne!(clone, list);
        assert_eq!(dom.serialize_subtree(clone), dom.serialize_subtree(list));
        dom.set_attr(clone_first(&dom, clone), "class", "changed");
        assert_eq!(dom.get_attr(clone_first(&dom, list), "class"), Some("a"));
    }

    fn clone_first(dom: &Dom, parent: NodeId) -> NodeId {
        dom.children(parent).next().unwrap()
    }

    /// serialize_subtree escapes text and attributes, void elements have
    /// no closing tag.
    #[test]
    fn serialize_escapes_and_void() {
        let mut dom = Dom::new();
        let p = dom.create_html_element("p");
        dom.append(dom.document(), p);
        dom.set_attr(p, "title", "a\"b <c>");
        let t = dom.create_text("x & y < z >");
        dom.append(p, t);
        let img = dom.create_html_element("img");
        dom.set_attr(img, "src", "a.png");
        dom.append(p, img);
        let out = dom.serialize_subtree(p);
        assert!(out.contains("title=\"a&quot;b &lt;c>\""), "out: {out}");
        assert!(out.contains("x &amp; y &lt; z &gt;"), "out: {out}");
        assert!(out.contains("<img src=\"a.png\">"), "out: {out}");
        assert!(!out.contains("</img>"), "out: {out}");
        assert!(out.ends_with("</p>"), "out: {out}");
    }

    /// is_connected walks to the document root.
    #[test]
    fn connectivity() {
        let mut dom = Dom::new();
        let div = dom.create_html_element("div");
        dom.append(dom.document(), div);
        let p = dom.create_html_element("p");
        dom.append(div, p);
        assert!(dom.is_connected(p));
        let orphan = dom.create_html_element("x");
        assert!(!dom.is_connected(orphan));
    }

    /// import_subtree copies across arenas.
    #[test]
    fn import_across_arenas() {
        let mut src = Dom::new();
        let ul = src.create_html_element("ul");
        src.append(src.document(), ul);
        let li = src.create_html_element("li");
        src.append(ul, li);
        let text = src.create_text("hi");
        src.append(li, text);

        let mut dst = Dom::new();
        let imported = dst.import_subtree(&src, ul);
        assert!(dst
            .element(imported)
            .is_some_and(|e| &*e.name.local == "ul"));
        assert_eq!(dst.serialize_subtree(imported), src.serialize_subtree(ul));
    }
}
