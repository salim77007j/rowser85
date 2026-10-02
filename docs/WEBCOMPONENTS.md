# WebComponents subsystem: custom elements, shadow DOM, templates

**Status:** implemented and validated (session 5, 2026-10-02) — the
documented unlock for YouTube-class Polymer applications.
**Validation:** `validation/media/wc.html` — 15 self-verifying checks
(identity, instanceof, upgrade callbacks, shadow trees, slots, innerHTML,
cloneNode, whenDefined, currentScript), all green through the real browser
via the frame-attested battery stage `m4-webcomponents`.

## 1. The problem

The engine's JS surface was a flat wrapper-per-access model: every
`getElementById` constructed a *new* `Element`, element listeners were
stored on discarded objects (never fired), `customElements` was a stub,
`<template>` content was dropped at parse finish, and nothing composed
shadow trees. Polymer-class applications (YouTube) register dozens of
custom elements, stamp templates into shadow roots, and drive the whole
UI through those mechanisms — without this stack the page renders its
server skeleton and hydration stalls silently.

## 2. The architecture

```text
        ┌────────────────────────── js prelude (Web API layer) ─────────────────────┐
        │  wraps: Map<NodeId, wrapper>  ← THE identity map (one JS object per node) │
        │  registry: tag → {ctor, observed}           customElements v1            │
        │  __wcUpgradeHandle: constructor-time node binding for upgrades           │
        └───────────────┬───────────────────────────────────────────────────────────┘
                        │ __native_* bridges (numbers + strings only)
        ┌───────────────▼──────────────────────── dom arena ────────────────────────┐
        │  template_contents: template → detached holder (survives parse)           │
        │  shadow_roots / shadow_hosts: host ↔ root maps                            │
        │  flat_children(node): shadow children at the host,                        │
        │      slot resolution (named/default/fallback), slotted light children     │
        │  clone_subtree / import_subtree / serialize_subtree (template-safe)       │
        └───────────────┬───────────────────────────────────────────────────────────┘
                        │ flat-tree walks
        ┌───────────────▼───────────────────────────────────────────────────────────┐
        │ cascade (compute_styles + shadow subtrees, flat parent inheritance)       │
        │ layout (build_box/collect_inline over flat_children)                      │
        │ display list (walk over flat_children)                                    │
        └──────────────────────────────────────────────────────────────────────────┘
```

### Identity first

The **NodeId→wrapper map** is the load-bearing change. Every accessor
(`getElementById`, `querySelector`, event dispatch, `childNodes`) resolves
to the *same* JS object, so listeners, custom-element instances and
framework caches persist. Natives that free nodes (`removeChild`,
`setTextContent`, `innerHTML` setter) report the freed handle list so
stale wrappers are dropped before the arena recycles ids.

### Custom elements, callback-complete

`customElements.define` upgrades every matching element already in the
document; `document.createElement` routes registered tags straight into
the constructor with the fresh node pre-bound (`__wcUpgradeHandle`).
`appendChild`/`insertBefore`/`innerHTML` fire `connectedCallback` for
newly-connected custom descendants via a **Rust-side subtree scan**
(`__native_dom_findCustomTags` — one native walk instead of a JS walk per
mutation), `removeChild` collects and fires `disconnectedCallback`, and
`setAttribute`/`removeAttribute` drive `attributeChangedCallback` for
`observedAttributes`. `whenDefined` returns real per-tag promises;
`polyfillWrapFlushCallback` (the webcomponents.js hook YouTube's
webcomponents-sd shim calls) runs flushes eagerly.

### Shadow DOM as flat tree

`attachShadow` creates a detached holder node and maps it to the host.
`Dom::flat_children` composes the **flat tree**: a host renders its
shadow children; `<slot name>` renders matching light children (the
default slot takes unslotted ones); slots without assignment render their
fallback content. The cascade computes styles for shadow subtrees with
the host as the inheritance parent (`flat_parent_element`), layout and
the display list walk the same flat tree, so shadow content paints at
the host's box. Shadow-root `<style>` elements are collected per render
(dynamic styles), which is what makes Polymer-injected styles apply.

### Templates, innerHTML, cloning

Template contents live in a detached holder recorded on the `Dom` itself
(surviving `DomSink::finish`); `template.content` returns a
DocumentFragment over it; `cloneNode(true)` deep-clones; the serializer
emits template children between the tags (innerHTML round-trips keep
them); `import_subtree` carries contents across arenas. `innerHTML`
set = parse + import + replace children + connect callbacks.

## 3. The expanded DOM surface (site-compatibility unlocks)

Beyond WebComponents, the prelude gained the surface that YouTube's
bootstrap concretely probed (each addition was the fix for a traced
error): the full HTML element class family (40+ classes), the Node
family (`Comment`, `CDATASection`, `ProcessingInstruction`,
`DocumentType`, `CharacterData`, `Attr`, `Window`), real
`TreeWalker`/`NodeIterator`, `DOMImplementation.createHTMLDocument`
(fully settable document-lite — polyfills assign `body`/
`documentElement`), scoped `querySelector` on elements and shadow roots,
`matches`/`closest`, `classList`, `Range`/`Selection`, `Intl`
(NumberFormat/DateTimeFormat/Collator + `supportedLocalesOf`),
`MessageChannel`/`MessagePort`/`MessageEvent`/`postMessage`/
`BroadcastChannel`, `document.currentScript` (script execution carries
its node id), a writable `document.readyState`, and per-run style
collection that includes shadow trees.

## 4. Known limits (honest list)

* **YouTube hydration stalls at ~529 DOM nodes** — the full page loads,
  147 scripts execute cleanly, hydration begins, then two long-tail
  errors stop the cascade (the Cast-extension loader tripping on an
  undefined ytcfg string, and an uberproxy URL check receiving
  undefined). Everything before those errors is fixed; the remaining
  grind is documented in the session addendum of `UI-WORKLOG.md`.
* **Shadow styles apply document-wide** (no per-shadow-root scoping);
  `:host`/`::slotted` selectors are dropped by the selector engine
  (empty pseudo-class enums).
* **No MutationObserver** (still a stub) and **no CSS custom
  properties** (`--x` / `var()`), both used by Polymer theming.
* **No ES modules** — classic scripts only; `type=module` scripts fail
  with SyntaxErrors.
* `attachShadow` mode is always open; `is="..."` customized built-ins
  are not routed; slotted nodes keep light-tree inheritance (correct)
  but `assignedSlot` returns null.
* Whole-document re-cascade+layout per mutation batch remains the
  invalidation model; full Polymer hydration will want subtree
  granularity for interactive-speed response.
