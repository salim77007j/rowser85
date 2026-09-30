//! CSS selector parsing and matching on the arena DOM.
//!
//! Powered by the `selectors` crate — the same selector engine used by
//! Servo/stylo — parameterized over [`DomSelectorImpl`].

use std::borrow::Borrow;
use std::fmt;

use cssparser::ToCss;
use markup5ever::interface::QuirksMode as DomQuirksMode;
use markup5ever::{LocalName, Namespace};
use precomputed_hash::PrecomputedHash;
use selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use selectors::context::{
    MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags, QuirksMode,
    SelectorCaches,
};
use selectors::matching::{ElementSelectorFlags, matches_selector_list};
use selectors::parser::{
    NonTSPseudoClass, ParseRelative, Parser, PseudoElement as PseudoElementTrait, SelectorImpl,
    SelectorList as InnerSelectorList, SelectorParseErrorKind,
};
use selectors::{Element, OpaqueElement};

use crate::tree::{Dom, NodeId};

/// A parsed selector list (comma-separated selectors).
pub type SelectorList = InnerSelectorList<DomSelectorImpl>;

/// Atom wrapper satisfying the `selectors` crate type bounds
/// (`Clone + Eq + From<&str> + ToCss + PrecomputedHash + Borrow<str>`).
#[derive(Clone, Debug, PartialEq, Eq, Default, Hash)]
pub struct DomName(pub LocalName);

impl std::ops::Deref for DomName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for DomName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<&str> for DomName {
    fn from(s: &str) -> Self {
        DomName(LocalName::from(s))
    }
}

impl AsRef<str> for DomName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl ToCss for DomName {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        dest.write_str(&self.0)
    }
}

impl PrecomputedHash for DomName {
    fn precomputed_hash(&self) -> u32 {
        // Delegate to the string_cache atom's own precomputed hash.
        self.0.precomputed_hash()
    }
}

/// Selector implementation bound to the Rrowser DOM.
#[derive(Clone, Debug)]
pub struct DomSelectorImpl;

impl SelectorImpl for DomSelectorImpl {
    type ExtraMatchingData<'a> = ();
    type AttrValue = DomName;
    type Identifier = DomName;
    type LocalName = DomName;
    type NamespaceUrl = Namespace;
    type NamespacePrefix = DomName;
    type BorrowedNamespaceUrl = Namespace;
    type BorrowedLocalName = str;
    type NonTSPseudoClass = PseudoClass;
    type PseudoElement = PseudoElement;
}

/// Pseudo-classes the engine understands. V1 intentionally supports none:
/// rules using unsupported pseudo-classes are dropped, per CSS error
/// recovery rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PseudoClass {}

impl ToCss for PseudoClass {
    fn to_css<W: fmt::Write>(&self, _dest: &mut W) -> fmt::Result {
        match *self {}
    }
}

impl NonTSPseudoClass for PseudoClass {
    fn is_active_or_hover(&self) -> bool {
        match *self {}
    }
    fn is_user_action_state(&self) -> bool {
        match *self {}
    }
}

/// Pseudo-elements understood by the engine (none in v1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PseudoElement {}

impl ToCss for PseudoElement {
    fn to_css<W: fmt::Write>(&self, _dest: &mut W) -> fmt::Result {
        match *self {}
    }
}

impl PseudoElementTrait for PseudoElement {}

/// Selector parser configuration.
#[derive(Debug, Clone, Copy)]
pub struct DomSelectorParser;

impl<'i> Parser<'i> for DomSelectorParser {
    type Impl = DomSelectorImpl;
    type Error = SelectorParseErrorKind;
}

/// Parses a comma-separated selector list. Returns `None` for invalid
/// selectors (the caller drops the rule, as CSS requires).
pub fn parse_selector_list(source: &str) -> Option<SelectorList> {
    let mut parser = cssparser::Parser::new(source);
    SelectorList::parse(&DomSelectorParser, &mut parser, ParseRelative::No).ok()
}

/// A borrow of a DOM element for selector matching.
#[derive(Debug, Clone, Copy)]
pub struct ElementRef<'a> {
    /// The DOM the node belongs to.
    pub dom: &'a Dom,
    /// The node id, guaranteed to be an element by the constructor.
    pub node: NodeId,
}

impl<'a> ElementRef<'a> {
    /// Borrows `node` as an element reference; `None` for non-elements.
    pub fn new(dom: &'a Dom, node: NodeId) -> Option<Self> {
        dom.element(node).is_some().then_some(ElementRef { dom, node })
    }
}

fn quirks(mode: DomQuirksMode) -> QuirksMode {
    match mode {
        DomQuirksMode::NoQuirks => QuirksMode::NoQuirks,
        DomQuirksMode::LimitedQuirks => QuirksMode::LimitedQuirks,
        DomQuirksMode::Quirks => QuirksMode::Quirks,
    }
}

/// Returns true when `element` matches the selector list.
pub fn matches(list: &SelectorList, element: &ElementRef<'_>) -> bool {
    let mut caches = SelectorCaches::default();
    let mut context = MatchingContext::new(
        MatchingMode::Normal,
        None,
        &mut caches,
        quirks(element.dom.quirks),
        NeedsSelectorFlags::No,
        MatchingForInvalidation::No,
    );
    matches_selector_list(list, element, &mut context)
}

impl<'a> Element for ElementRef<'a> {
    type Impl = DomSelectorImpl;

    fn opaque(&self) -> OpaqueElement {
        OpaqueElement::new(self)
    }

    fn parent_element(&self) -> Option<Self> {
        self.dom.parent_element(self.node).and_then(|p| ElementRef::new(self.dom, p))
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        false
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        None
    }

    fn is_pseudo_element(&self) -> bool {
        false
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        self.dom.prev_sibling_element(self.node).and_then(|p| ElementRef::new(self.dom, p))
    }

    fn next_sibling_element(&self) -> Option<Self> {
        self.dom.next_sibling_element(self.node).and_then(|p| ElementRef::new(self.dom, p))
    }

    fn first_element_child(&self) -> Option<Self> {
        self.dom.first_element_child(self.node).and_then(|c| ElementRef::new(self.dom, c))
    }

    fn is_html_element_in_html_document(&self) -> bool {
        self.dom
            .element(self.node)
            .map(|e| e.name.ns == markup5ever::ns!(html))
            .unwrap_or(false)
    }

    fn has_local_name(&self, local_name: &str) -> bool {
        self.dom
            .element(self.node)
            .map(|e| &*e.name.local == local_name)
            .unwrap_or(false)
    }

    fn has_namespace(&self, ns: &Namespace) -> bool {
        self.dom.element(self.node).map(|e| &e.name.ns == ns).unwrap_or(false)
    }

    fn is_same_type(&self, other: &Self) -> bool {
        match (self.dom.element(self.node), self.dom.element(other.node)) {
            (Some(a), Some(b)) => a.name == b.name,
            _ => false,
        }
    }

    fn attr_matches(
        &self,
        ns: &NamespaceConstraint<&Namespace>,
        local_name: &DomName,
        operation: &AttrSelectorOperation<&DomName>,
    ) -> bool {
        let Some(el) = self.dom.element(self.node) else { return false };
        if let NamespaceConstraint::Specific(specific) = ns {
            // Attributes are stored without namespace; only the empty
            // namespace matches.
            if !specific.is_empty() {
                return false;
            }
        }
        el.attrs
            .iter()
            .find(|a| *a.name == **local_name)
            .map(|a| operation.eval_str(&a.value))
            .unwrap_or(false)
    }

    fn match_non_ts_pseudo_class(
        &self,
        pc: &PseudoClass,
        _context: &mut MatchingContext<DomSelectorImpl>,
    ) -> bool {
        match *pc {}
    }

    fn match_pseudo_element(
        &self,
        pe: &PseudoElement,
        _context: &mut MatchingContext<DomSelectorImpl>,
    ) -> bool {
        match *pe {}
    }

    fn apply_selector_flags(&self, _flags: ElementSelectorFlags) {}

    fn is_link(&self) -> bool {
        self.has_local_name("a") || self.has_local_name("area")
    }

    fn is_html_slot_element(&self) -> bool {
        self.has_local_name("slot")
    }

    fn has_id(&self, id: &DomName, case_sensitivity: CaseSensitivity) -> bool {
        let Some(el) = self.dom.element(self.node) else { return false };
        let Some(actual) = el.id.as_deref() else { return false };
        match case_sensitivity {
            CaseSensitivity::CaseSensitive => actual == &**id,
            CaseSensitivity::AsciiCaseInsensitive => actual.eq_ignore_ascii_case(id),
        }
    }

    fn has_class(&self, name: &DomName, case_sensitivity: CaseSensitivity) -> bool {
        let Some(el) = self.dom.element(self.node) else { return false };
        match case_sensitivity {
            CaseSensitivity::CaseSensitive => {
                el.classes.iter().any(|c| c.as_str() == &**name)
            }
            CaseSensitivity::AsciiCaseInsensitive => {
                el.classes.iter().any(|c| c.eq_ignore_ascii_case(name))
            }
        }
    }

    fn has_custom_state(&self, _name: &DomName) -> bool {
        false
    }

    fn imported_part(&self, _name: &DomName) -> Option<DomName> {
        None
    }

    fn is_part(&self, _name: &DomName) -> bool {
        false
    }

    fn is_empty(&self) -> bool {
        !self.dom.children(self.node).any(|child| {
            matches!(self.dom.kind(child), crate::tree::NodeKind::Element(_))
                || self.dom.text(child).map(|t| !t.is_empty()).unwrap_or(false)
        })
    }

    fn is_root(&self) -> bool {
        self.dom.parent(self.node).map(|p| p == self.dom.document()).unwrap_or(false)
    }

    fn add_element_unique_hashes(&self, _filter: &mut selectors::bloom::BloomFilter) -> bool {
        // The Rrowser matcher does not use bloom filters; report that no
        // hashes were added so the engine falls back to direct matching.
        false
    }
}

impl Dom {
    /// `document.querySelectorAll` style query under `root`.
    pub fn query_selector_all(&self, root: NodeId, selector: &str) -> Option<Vec<NodeId>> {
        let list = parse_selector_list(selector)?;
        let mut hits = Vec::new();
        for node in self.subtree_elements(root) {
            if let Some(element) = ElementRef::new(self, node) {
                if matches(&list, &element) {
                    hits.push(node);
                }
            }
        }
        Some(hits)
    }

    /// `document.querySelector` style query under `root`.
    pub fn query_selector(&self, root: NodeId, selector: &str) -> Option<NodeId> {
        let list = parse_selector_list(selector)?;
        self.subtree_elements(root).find(|node| {
            ElementRef::new(self, *node).map(|e| matches(&list, &e)).unwrap_or(false)
        })
    }
}

/// Reusable selector matching caches.
///
/// Creating a `SelectorCaches` per match dominates matching cost, so the
/// cascade reuses one wrap across the whole document. This mirrors how
/// Servo reuses its caches across a traversal; entries are keyed per
/// element, so cross-element reuse is sound for a read-only pass.
#[derive(Default)]
pub struct CachesWrap {
    inner: SelectorCaches,
}

/// Matches `element` against `list`, reusing `caches`.
pub fn matches_with_caches(
    list: &SelectorList,
    element: &ElementRef<'_>,
    caches: &mut CachesWrap,
) -> bool {
    let mut context = MatchingContext::new(
        MatchingMode::Normal,
        None,
        &mut caches.inner,
        quirks(element.dom.quirks),
        NeedsSelectorFlags::No,
        MatchingForInvalidation::No,
    );
    matches_selector_list(list, element, &mut context)
}
