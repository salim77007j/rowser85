//! Rrowser DOM: arena-based document object model with CSS selector matching.
//!
//! The DOM is the shared vocabulary type of the whole engine. It is
//! intentionally dependency-light so that every later stage (parsing, layout,
//! rendering, JS bindings) can depend on it without cycles.
//!
//! Design notes:
//!
//! * Nodes live in a flat [`Vec`] arena (`slots`) addressed by a [`NodeId`]
//!   index. This is cache friendly, has no `Rc` cycles, and frees the tree
//!   with a single allocation drop.
//! * Freed slots are recycled through a free-list; each slot carries a
//!   generation counter so stale JS-side handles can be detected
//!   (`Dom::generation` / `Dom::is_valid`).
//! * Every mutation bumps a `version` counter. Layout and rendering use it
//!   as a cheap invalidation signal.
//! * Selector matching is implemented on top of the `selectors` crate —
//!   the same selector engine Servo/stylo uses — exposed through
//!   [`Dom::query_selector_all`] and [`crate::selector::matches`].

pub mod selector;
pub mod tree;

pub use selector::{
    matches, matches_for_pseudo_with_caches, matches_with_caches, parse_selector_list, CachesWrap,
    DomSelectorImpl, ElementRef, Selector, SelectorList,
};
pub use tree::{Attr, Dom, Element, NodeId, NodeKind};
