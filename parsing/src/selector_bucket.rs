//! Rule bucket index for fast candidate selection during cascade.
//!
//! Every rule is bucketed by the identifiers it mentions (tag names, `#ids`,
//! `.classes`). An element only needs to inspect the union of buckets for
//! its own tag, id and classes (plus the universal bucket). Because the
//! final decision is a full selector match, over-bucketing is safe; the
//! scan below errs on the side of inclusion (attribute selectors and
//! `*` fall back to the universal bucket).

use std::collections::HashMap;

use crate::css::StyleRuleEntry;

/// Bucketed rule index.
#[derive(Debug, Default)]
pub struct RuleIndex {
    by_id: HashMap<String, Vec<usize>>,
    by_class: HashMap<String, Vec<usize>>,
    by_tag: HashMap<String, Vec<usize>>,
    universal: Vec<usize>,
}

impl RuleIndex {
    /// Buckets `entries` (order preserved).
    pub fn build(entries: &[StyleRuleEntry]) -> Self {
        let mut index = RuleIndex::default();
        for (i, entry) in entries.iter().enumerate() {
            index.add_selector(i, &entry.selector_text);
        }
        index
    }

    fn add_selector(&mut self, i: usize, text: &str) {
        let mut has_attr = false;
        let mut bucket_kinds: Vec<(u8, String)> = Vec::new();
        let bytes = text.as_bytes();
        let mut pos = 0;
        while pos < bytes.len() {
            let b = bytes[pos];
            match b {
                b'#' | b'.' => {
                    let kind = if b == b'#' { 1 } else { 2 };
                    let (ident, next) = scan_ident(bytes, pos + 1);
                    if !ident.is_empty() {
                        bucket_kinds.push((kind, ident));
                    }
                    pos = next;
                }
                b'[' => {
                    has_attr = true;
                    pos += 1;
                }
                b'*' => {
                    self.universal.push(i);
                    pos += 1;
                }
                b if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'\\' => {
                    let (ident, next) = scan_ident(bytes, pos);
                    if !ident.is_empty() {
                        // Bare identifiers may be tags or pseudo-class
                        // arguments; treat as tags (safe superset).
                        bucket_kinds.push((0, ident.to_ascii_lowercase()));
                    }
                    pos = next;
                }
                _ => {
                    pos += 1;
                }
            }
        }
        if bucket_kinds.is_empty() && !has_attr {
            self.universal.push(i);
            return;
        }
        for (kind, ident) in bucket_kinds {
            match kind {
                0 => self.by_tag.entry(ident).or_default().push(i),
                1 => self.by_id.entry(ident).or_default().push(i),
                _ => self.by_class.entry(ident).or_default().push(i),
            }
        }
        if has_attr {
            self.universal.push(i);
        }
    }

    /// Candidate rule indices for an element with the given tag/id/classes,
    /// in ascending rule order.
    pub fn lookup_indices<'c>(
        &self,
        tag: &str,
        id: Option<&str>,
        classes: impl Iterator<Item = &'c str>,
    ) -> Vec<usize> {
        let mut hits: Vec<usize> = Vec::new();
        let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut push = |v: Option<&Vec<usize>>| {
            if let Some(list) = v {
                for &idx in list {
                    if seen.insert(idx) {
                        hits.push(idx);
                    }
                }
            }
        };
        push(self.by_tag.get(&tag.to_ascii_lowercase()));
        if let Some(id) = id {
            push(self.by_id.get(&id.to_ascii_lowercase()));
        }
        for class in classes {
            push(self.by_class.get(&class.to_ascii_lowercase()));
        }
        for &idx in &self.universal {
            if seen.insert(idx) {
                hits.push(idx);
            }
        }
        hits.sort_unstable();
        hits
    }
}

fn scan_ident(bytes: &[u8], mut pos: usize) -> (String, usize) {
    let start = pos;
    while pos < bytes.len() {
        let b = bytes[pos];
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b >= 0x80 {
            pos += 1;
        } else {
            break;
        }
    }
    (String::from_utf8_lossy(&bytes[start..pos]).into_owned(), pos)
}
