//! HTML parsing: bytes → arena DOM.
//!
//! Uses html5ever (the Servo-derived WHATWG parser) with a custom
//! [`TreeSink`] that writes directly into [`rowser_dom::Dom`].

use std::cell::RefCell;

use encoding_rs::Encoding;
use html5ever::driver::{parse_document, ParseOpts};
use html5ever::interface::ElemName;
use html5ever::interface::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::stream::TendrilSink;
use html5ever::tendril::StrTendril;
use html5ever::{Attribute, QualName};
use html5ever::{LocalName, Namespace};
use rowser_dom::{Attr, Dom, NodeId};

/// A parsed HTML document plus document-level metadata.
#[derive(Debug)]
pub struct Document {
    /// The DOM tree.
    pub dom: Dom,
    /// Document URL, when known (set by the engine after navigation).
    pub url: Option<String>,
    /// Document title (text of `<title>`).
    pub title: String,
}

impl Document {
    /// Collects the inline `<style>` element contents in document order.
    pub fn style_blocks(&self) -> Vec<String> {
        let mut out = Vec::new();
        for node in self.dom.descendants(self.dom.document()) {
            if let Some(el) = self.dom.element(node) {
                if &*el.name.local == "style" {
                    out.push(self.dom.text_content(node));
                }
            }
        }
        out
    }

    /// Collects scripts: inline bodies and external sources.
    pub fn scripts(&self) -> Vec<ScriptInfo> {
        let mut out = Vec::new();
        for node in self.dom.descendants(self.dom.document()) {
            if let Some(el) = self.dom.element(node) {
                if &*el.name.local == "script" {
                    let src = self.dom.get_attr(node, "src").map(str::to_owned);
                    let r#type = self.dom.get_attr(node, "type").map(str::to_owned);
                    let body = if src.is_none() {
                        Some(self.dom.text_content(node))
                    } else {
                        None
                    };
                    out.push(ScriptInfo { src, r#type, body });
                }
            }
        }
        out
    }

    /// Stylesheet link targets: `<link rel~="stylesheet" href>`.
    pub fn stylesheet_links(&self) -> Vec<String> {
        let mut out = Vec::new();
        for node in self.dom.descendants(self.dom.document()) {
            if let Some(el) = self.dom.element(node) {
                if &*el.name.local == "link" {
                    let rel = self.dom.get_attr(node, "rel").unwrap_or_default();
                    let is_css = rel
                        .split_whitespace()
                        .any(|r| r.eq_ignore_ascii_case("stylesheet"));
                    if is_css {
                        if let Some(href) = self.dom.get_attr(node, "href") {
                            out.push(href.to_owned());
                        }
                    }
                }
            }
        }
        out
    }

    fn extract_title(&mut self) {
        for node in self.dom.descendants(self.dom.document()) {
            if let Some(el) = self.dom.element(node) {
                if &*el.name.local == "title" {
                    self.title = self.dom.text_content(node);
                    return;
                }
            }
        }
    }
}

/// A `<script>` element reduced to what the JS runtime needs.
#[derive(Debug, Clone)]
pub struct ScriptInfo {
    /// `src` attribute, when the script is external.
    pub src: Option<String>,
    /// `type` attribute (e.g. `module`); `None` means classic script.
    pub r#type: Option<String>,
    /// Inline script text, when present.
    pub body: Option<String>,
}

/// Charset detection: BOM first, then `<meta charset>` in the first 2 KiB,
/// mirroring the WHATWG encoding sniffing algorithm (simplified).
fn sniff_encoding(bytes: &[u8]) -> &'static Encoding {
    if let Some((enc, _)) = Encoding::for_bom(bytes) {
        return enc;
    }
    let window = &bytes[..bytes.len().min(2048)];
    if let Some(meta) = find_meta_charset(window) {
        if let Some(enc) = Encoding::for_label(meta.as_bytes()) {
            return enc;
        }
    }
    encoding_rs::UTF_8
}

/// Tiny case-insensitive scan for `charset=...` inside a `<meta ...>` tag.
fn find_meta_charset(window: &[u8]) -> Option<String> {
    let lower: Vec<u8> = window.iter().map(|b| b.to_ascii_lowercase()).collect();
    let mut pos = 0;
    while let Some(start) = find_sub(&lower[pos..], b"<meta") {
        let abs = pos + start;
        let end = find_sub(&lower[abs..], b">")
            .map(|e| abs + e)
            .unwrap_or(lower.len());
        let tag = &lower[abs..end];
        if let Some(cpos) = find_sub(tag, b"charset") {
            let rest = &tag[cpos + b"charset".len()..];
            let rest = trim_start(rest, b" \t\r\n=\"'");
            let value_end = rest
                .iter()
                .position(|b| b.is_ascii_whitespace() || *b == b'"' || *b == b'\'')
                .unwrap_or(rest.len());
            if value_end > 0 {
                return Some(String::from_utf8_lossy(&rest[..value_end]).into_owned());
            }
        }
        pos = end;
        if pos >= lower.len() {
            break;
        }
    }
    None
}

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn trim_start<'a>(mut bytes: &'a [u8], skip: &[u8]) -> &'a [u8] {
    while let Some(&first) = bytes.first() {
        if skip.contains(&first) {
            bytes = &bytes[1..];
        } else {
            break;
        }
    }
    bytes
}

/// Parses HTML bytes into a [`Document`].
pub fn parse_html(bytes: &[u8]) -> Document {
    let encoding = sniff_encoding(bytes);
    let (text, _, _) = encoding.decode(bytes);
    let sink = DomSink::new();
    let parser = parse_document(sink, ParseOpts::default());
    let dom = parser.one(String::from(text));
    let mut document = Document {
        dom,
        url: None,
        title: String::new(),
    };
    document.extract_title();
    document
}

/// An [`ElemName`] implementation for the arena DOM.
///
/// Atoms are cloned (reference-counted) so the value can outlive the
/// `RefCell` borrow that produced it.
#[derive(Debug)]
pub struct DomElemName {
    ns: Namespace,
    local: LocalName,
}

impl ElemName for DomElemName {
    fn ns(&self) -> &Namespace {
        &self.ns
    }
    fn local_name(&self) -> &LocalName {
        &self.local
    }
}

/// The html5ever tree sink writing into the arena DOM.
///
/// All methods receive `&self` (per the html5ever contract) so the DOM is
/// held in a `RefCell` and released by [`TreeSink::finish`].
pub struct DomSink {
    dom: RefCell<Dom>,
}

impl DomSink {
    /// Creates a sink with a fresh document.
    pub fn new() -> Self {
        DomSink {
            dom: RefCell::new(Dom::new()),
        }
    }

    fn with_dom<R>(&self, f: impl FnOnce(&mut Dom) -> R) -> R {
        f(&mut self.dom.borrow_mut())
    }

    fn append_node_or_text(&self, parent: NodeId, child: NodeOrText<NodeId>) {
        match child {
            NodeOrText::AppendNode(node) => self.with_dom(|dom| dom.append(parent, node)),
            NodeOrText::AppendText(text) => {
                self.with_dom(|dom| dom.append_text(parent, &text));
            }
        }
    }
}

impl Default for DomSink {
    fn default() -> Self {
        Self::new()
    }
}

fn convert_attrs(attrs: Vec<Attribute>) -> Vec<Attr> {
    attrs
        .into_iter()
        .map(|a| Attr {
            name: a.name.local,
            value: a.value.as_ref().to_owned(),
        })
        .collect()
}

impl TreeSink for DomSink {
    type Handle = NodeId;
    type Output = Dom;
    type ElemName<'a>
        = DomElemName
    where
        Self: 'a;

    fn finish(self) -> Self::Output {
        self.dom.into_inner()
    }

    fn parse_error(&self, msg: std::borrow::Cow<'static, str>) {
        tracing::debug!(target: "rowser::parse", "html5ever: {msg}");
    }

    fn get_document(&self) -> Self::Handle {
        self.dom.borrow().document()
    }

    fn elem_name<'a>(&'a self, target: &'a Self::Handle) -> Self::ElemName<'a> {
        let dom = self.dom.borrow();
        let el = dom
            .element(*target)
            .expect("elem_name called on non-element");
        DomElemName {
            ns: el.name.ns.clone(),
            local: el.name.local.clone(),
        }
    }

    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<Attribute>,
        flags: ElementFlags,
    ) -> Self::Handle {
        let node = self.with_dom(|dom| dom.create_element(name, convert_attrs(attrs)));
        if flags.template {
            // The template contents live in a detached (invisible) holder.
            // The association is persisted in the Dom itself so post-parse
            // consumers (template.content, cloneNode) can reach it.
            self.with_dom(|dom| {
                let contents = dom.create_comment("template-contents");
                dom.set_template_contents(node, contents);
            });
        }
        node
    }

    fn create_comment(&self, text: StrTendril) -> Self::Handle {
        self.with_dom(|dom| dom.create_comment(text.as_ref()))
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> Self::Handle {
        self.with_dom(|dom| {
            dom.create_comment(format!("<?{} {}?>", target.as_ref(), data.as_ref()))
        })
    }

    fn append(&self, parent: &Self::Handle, child: NodeOrText<Self::Handle>) {
        self.append_node_or_text(*parent, child);
    }

    fn append_based_on_parent_node(
        &self,
        element: &Self::Handle,
        prev_element: &Self::Handle,
        child: NodeOrText<Self::Handle>,
    ) {
        let parent = self.dom.borrow().parent(*element);
        match parent {
            Some(p) => self.append_node_or_text(p, child),
            None => self.append_node_or_text(*prev_element, child),
        }
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        let doc = self.get_document();
        let node = self.with_dom(|dom| {
            dom.create_doctype(name.as_ref(), public_id.as_ref(), system_id.as_ref())
        });
        self.with_dom(|dom| dom.append(doc, node));
    }

    fn get_template_contents(&self, target: &Self::Handle) -> Self::Handle {
        self.dom
            .borrow()
            .template_contents(*target)
            .expect("template contents requested for non-template")
    }

    fn same_node(&self, x: &Self::Handle, y: &Self::Handle) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.with_dom(|dom| dom.quirks = mode);
    }

    fn append_before_sibling(&self, sibling: &Self::Handle, new_node: NodeOrText<Self::Handle>) {
        let parent = self
            .dom
            .borrow()
            .parent(*sibling)
            .expect("append_before_sibling: sibling has no parent");
        match new_node {
            NodeOrText::AppendNode(node) => {
                self.with_dom(|dom| dom.insert_before(parent, node, Some(*sibling)));
            }
            NodeOrText::AppendText(text) => {
                let node = self.with_dom(|dom| dom.create_text(text.as_ref()));
                self.with_dom(|dom| dom.insert_before(parent, node, Some(*sibling)));
            }
        }
    }

    fn add_attrs_if_missing(&self, target: &Self::Handle, attrs: Vec<Attribute>) {
        let new_attrs = convert_attrs(attrs);
        self.with_dom(|dom| {
            for attr in new_attrs {
                let name = attr.name.to_string();
                if dom.get_attr(*target, &name).is_none() {
                    dom.set_attr(*target, &name, &attr.value);
                }
            }
        });
    }

    fn remove_from_parent(&self, target: &Self::Handle) {
        self.with_dom(|dom| dom.detach(*target));
    }

    fn reparent_children(&self, node: &Self::Handle, new_parent: &Self::Handle) {
        self.with_dom(|dom| dom.reparent_children(*node, *new_parent));
    }

    fn is_mathml_annotation_xml_integration_point(&self, handle: &Self::Handle) -> bool {
        let dom = self.dom.borrow();
        dom.element(*handle)
            .map(|el| el.name.ns == *html5ever::ns!(mathml) && &*el.name.local == "annotation-xml")
            .unwrap_or(false)
    }

    fn allow_declarative_shadow_roots(&self, _intended_parent: &Self::Handle) -> bool {
        true
    }
}
