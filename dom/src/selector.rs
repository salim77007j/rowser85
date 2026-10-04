//! CSS selector parsing and matching on the arena DOM.
//!
//! Powered by the `selectors` crate — the same selector engine used by
//! Servo/stylo — parameterized over [`DomSelectorImpl`].

use std::borrow::Borrow;
use std::fmt;

use cssparser::{CowRcStr, ParseError, ToCss};
use markup5ever::interface::QuirksMode as DomQuirksMode;
use markup5ever::{LocalName, Namespace};
use precomputed_hash::PrecomputedHash;
use selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use selectors::context::{
    MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags, QuirksMode,
    SelectorCaches,
};
use selectors::matching::{matches_selector_list, ElementSelectorFlags};
use selectors::parser::{
    NonTSPseudoClass, ParseRelative, Parser, PseudoElement as PseudoElementTrait, SelectorImpl,
    SelectorList as InnerSelectorList, SelectorParseErrorKind,
};
use selectors::{Element, OpaqueElement};

use crate::tree::{Dom, NodeId};

/// A parsed selector list (comma-separated selectors).
pub type SelectorList = InnerSelectorList<DomSelectorImpl>;

/// One parsed complex selector.
pub type Selector = selectors::parser::Selector<DomSelectorImpl>;

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

/// Pseudo-classes the engine understands: user-interaction state plus
/// form/structure state that needs external knowledge. Tree-structural
/// pseudo-classes (:first-child, :nth-child(), …) are handled entirely by
/// the `selectors` crate and need no engine support.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PseudoClass {
    /// The element under the pointer.
    Hover,
    /// The element being pressed.
    Active,
    /// The focused element.
    Focus,
    /// Focus with visible ring semantics.
    FocusVisible,
    /// An element whose subtree contains the focus.
    FocusWithin,
    /// A visited link.
    Visited,
    /// An anchor.
    Link,
    /// A disabled form control.
    Disabled,
    /// An enabled form control.
    Enabled,
    /// A checked form control.
    Checked,
    /// A form control in an indeterminate state.
    Indeterminate,
    /// Element with no children at all (matching :empty semantics needs
    /// state only when JS mutates — handled by the crate directly).
    PlaceholderShown,
}

impl ToCss for PseudoClass {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        let name = match self {
            PseudoClass::Hover => "hover",
            PseudoClass::Active => "active",
            PseudoClass::Focus => "focus",
            PseudoClass::FocusVisible => "focus-visible",
            PseudoClass::FocusWithin => "focus-within",
            PseudoClass::Visited => "visited",
            PseudoClass::Link => "link",
            PseudoClass::Disabled => "disabled",
            PseudoClass::Enabled => "enabled",
            PseudoClass::Checked => "checked",
            PseudoClass::Indeterminate => "indeterminate",
            PseudoClass::PlaceholderShown => "placeholder-shown",
        };
        dest.write_str(":")?;
        dest.write_str(name)
    }
}

impl NonTSPseudoClass for PseudoClass {
    fn is_active_or_hover(&self) -> bool {
        matches!(self, PseudoClass::Active | PseudoClass::Hover)
    }
    fn is_user_action_state(&self) -> bool {
        matches!(
            self,
            PseudoClass::Hover | PseudoClass::Active | PseudoClass::Focus
        )
    }
}

/// Pseudo-elements understood by the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PseudoElement {
    /// ::before.
    Before,
    /// ::after.
    After,
    /// ::first-line (treated as an inline overlay; text only).
    FirstLine,
    /// ::first-letter (first-letter styling).
    FirstLetter,
    /// ::selection highlight.
    Selection,
    /// ::placeholder for form controls.
    Placeholder,
    /// ::marker for list bullets.
    Marker,
}

impl ToCss for PseudoElement {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        let name = match self {
            PseudoElement::Before => "before",
            PseudoElement::After => "after",
            PseudoElement::FirstLine => "first-line",
            PseudoElement::FirstLetter => "first-letter",
            PseudoElement::Selection => "selection",
            PseudoElement::Placeholder => "placeholder",
            PseudoElement::Marker => "marker",
        };
        dest.write_str("::")?;
        dest.write_str(name)
    }
}

impl PseudoElementTrait for PseudoElement {}

/// Selector parser configuration.
#[derive(Debug, Clone, Copy)]
pub struct DomSelectorParser;

impl<'i> Parser<'i> for DomSelectorParser {
    type Impl = DomSelectorImpl;
    type Error = SelectorParseErrorKind;

    fn parse_non_ts_pseudo_class(
        &self,
        name: CowRcStr<'i>,
    ) -> Result<PseudoClass, ParseError<SelectorParseErrorKind>> {
        let class = match name.as_ref().to_ascii_lowercase().as_str() {
            "hover" => PseudoClass::Hover,
            "active" => PseudoClass::Active,
            "focus" => PseudoClass::Focus,
            "focus-visible" => PseudoClass::FocusVisible,
            "focus-within" => PseudoClass::FocusWithin,
            "visited" => PseudoClass::Visited,
            "link" => PseudoClass::Link,
            "disabled" => PseudoClass::Disabled,
            "enabled" => PseudoClass::Enabled,
            "checked" => PseudoClass::Checked,
            "indeterminate" => PseudoClass::Indeterminate,
            "placeholder-shown" => PseudoClass::PlaceholderShown,
            _ => {
                return Err(ParseError::custom(
                    SelectorParseErrorKind::UnsupportedPseudoClassOrElement,
                ))
            }
        };
        Ok(class)
    }

    fn parse_pseudo_element(
        &self,
        name: CowRcStr<'i>,
    ) -> Result<PseudoElement, ParseError<SelectorParseErrorKind>> {
        let element = match name.as_ref().to_ascii_lowercase().as_str() {
            "before" => PseudoElement::Before,
            "after" => PseudoElement::After,
            "first-line" => PseudoElement::FirstLine,
            "first-letter" => PseudoElement::FirstLetter,
            "selection" => PseudoElement::Selection,
            "placeholder" => PseudoElement::Placeholder,
            "marker" => PseudoElement::Marker,
            _ => {
                return Err(ParseError::custom(
                    SelectorParseErrorKind::UnsupportedPseudoClassOrElement,
                ))
            }
        };
        Ok(element)
    }
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
        dom.element(node)
            .is_some()
            .then_some(ElementRef { dom, node })
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
        // Identity for selector caches: point at the element's arena slot.
        // The arena does not move or mutate during a matching pass, so the
        // address is unique per node and stable for the pass. (Using the
        // ElementRef's own address was a bug: those are stack temporaries
        // reused across loop iterations, colliding cache entries across
        // different elements.)
        OpaqueElement::new(self.dom.element(self.node).expect("element"))
    }

    fn parent_element(&self) -> Option<Self> {
        self.dom
            .parent_element(self.node)
            .and_then(|p| ElementRef::new(self.dom, p))
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
        self.dom
            .prev_sibling_element(self.node)
            .and_then(|p| ElementRef::new(self.dom, p))
    }

    fn next_sibling_element(&self) -> Option<Self> {
        self.dom
            .next_sibling_element(self.node)
            .and_then(|p| ElementRef::new(self.dom, p))
    }

    fn first_element_child(&self) -> Option<Self> {
        self.dom
            .first_element_child(self.node)
            .and_then(|c| ElementRef::new(self.dom, c))
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
        self.dom
            .element(self.node)
            .map(|e| &e.name.ns == ns)
            .unwrap_or(false)
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
        let Some(el) = self.dom.element(self.node) else {
            return false;
        };
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
        match pc {
            PseudoClass::Hover => self.dom.is_hovered(self.node),
            PseudoClass::Active => self.dom.interaction_state.borrow().active == Some(self.node),
            PseudoClass::Focus => self.dom.interaction_state.borrow().focus == Some(self.node),
            PseudoClass::FocusVisible => {
                self.dom.interaction_state.borrow().focus == Some(self.node)
            }
            PseudoClass::FocusWithin => self.dom.is_focus_within(self.node),
            PseudoClass::Visited => self
                .dom
                .interaction_state
                .borrow()
                .visited
                .contains(&self.node),
            PseudoClass::Link => self.is_link(),
            PseudoClass::Disabled => self.dom.get_attr(self.node, "disabled").is_some(),
            PseudoClass::Enabled => {
                let form_control = self.has_local_name("input")
                    || self.has_local_name("button")
                    || self.has_local_name("select")
                    || self.has_local_name("textarea")
                    || self.has_local_name("option");
                form_control && self.dom.get_attr(self.node, "disabled").is_none()
            }
            PseudoClass::Checked => {
                self.dom.get_attr(self.node, "checked").is_some()
                    || self.dom.get_attr(self.node, "selected").is_some()
            }
            PseudoClass::Indeterminate => false,
            PseudoClass::PlaceholderShown => false,
        }
    }

    fn match_pseudo_element(
        &self,
        pe: &PseudoElement,
        _context: &mut MatchingContext<DomSelectorImpl>,
    ) -> bool {
        // Pseudo-element matching is handled structurally (the cascade
        // strips and records them); a bare query never matches directly.
        let _ = pe;
        false
    }

    fn apply_selector_flags(&self, _flags: ElementSelectorFlags) {}

    fn is_link(&self) -> bool {
        self.has_local_name("a") || self.has_local_name("area")
    }

    fn is_html_slot_element(&self) -> bool {
        self.has_local_name("slot")
    }

    fn has_id(&self, id: &DomName, case_sensitivity: CaseSensitivity) -> bool {
        let Some(el) = self.dom.element(self.node) else {
            return false;
        };
        let Some(actual) = el.id.as_deref() else {
            return false;
        };
        match case_sensitivity {
            CaseSensitivity::CaseSensitive => actual == &**id,
            CaseSensitivity::AsciiCaseInsensitive => actual.eq_ignore_ascii_case(id),
        }
    }

    fn has_class(&self, name: &DomName, case_sensitivity: CaseSensitivity) -> bool {
        let Some(el) = self.dom.element(self.node) else {
            return false;
        };
        match case_sensitivity {
            CaseSensitivity::CaseSensitive => el.classes.iter().any(|c| c.as_str() == &**name),
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
        self.dom
            .parent(self.node)
            .map(|p| p == self.dom.document())
            .unwrap_or(false)
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
            ElementRef::new(self, *node)
                .map(|e| matches(&list, &e))
                .unwrap_or(false)
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

/// Matches `element` against a pseudo-element rule's selector list (the
/// trailing ::before/::after is ignored; the element part must match).
pub fn matches_for_pseudo_with_caches(
    list: &SelectorList,
    element: &ElementRef<'_>,
    caches: &mut CachesWrap,
) -> bool {
    let mut context = MatchingContext::new(
        MatchingMode::ForStatelessPseudoElement,
        None,
        &mut caches.inner,
        quirks(element.dom.quirks),
        NeedsSelectorFlags::No,
        MatchingForInvalidation::No,
    );
    matches_selector_list(list, element, &mut context)
}
