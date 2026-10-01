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
}

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
        };
        dom.document = dom.alloc(NodeKind::Document);
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
        self.descendants(root).filter(|id| self.element(*id).is_some())
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
        self.element(id)?.attrs.iter().find(|a| &*a.name == name).map(|a| a.value.as_str())
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
        stack.extend(pins.iter().copied().chain(
            pins.iter().flat_map(|p| self.first_child(*p).into_iter()),
        ));
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
