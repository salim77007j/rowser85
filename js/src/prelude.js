// Rrowser JS prelude — the Web API surface built on `__native_*` bridges.
// All callbacks live in JS-land (GC-safe); Rust only ever sees numbers and
// strings. Evaluated before any page script.
(function () {
  'use strict';

  // ------------------------------------------------------------------ console
  function stringify(args) {
    return args.map(function (a) {
      if (a === null) return 'null';
      if (a === undefined) return 'undefined';
      if (typeof a === 'string') return a;
      if (typeof a === 'function') return 'function ' + (a.name || 'anonymous');
      if (a instanceof Error) return a.stack || String(a);
      try { return JSON.stringify(a); } catch (e) { return String(a); }
    }).join(' ');
  }
  const console = {
    log: (...a) => __native_console('log', stringify(a)),
    info: (...a) => __native_console('info', stringify(a)),
    warn: (...a) => __native_console('warn', stringify(a)),
    error: (...a) => __native_console('error', stringify(a)),
    debug: (...a) => __native_console('log', stringify(a)),
  };
  globalThis.console = console;

  // ------------------------------------------------------------------ window
  // Every real-world script reaches for `window` (and friends). Missing it
  // was the single largest site-compatibility break: ALL of YouTube's inline
  // scripts died with "window is not defined" before this alias existed.
  const windowAlias = globalThis;
  globalThis.window = windowAlias;
  globalThis.self = windowAlias;
  globalThis.top = windowAlias;
  globalThis.parent = windowAlias;
  globalThis.frames = windowAlias;
  globalThis.name = '';
  globalThis.closed = false;
  globalThis.length = 0;
  // Window metrics: live from the engine (layout mirror + scroll). A real
  // innerWidth/innerHeight/scrollY unblocks every player framework's
  // sizing logic (they measured 0x0 before).
  try {
    Object.defineProperty(globalThis, 'scrollY', { configurable: true, get: function () { return __native_dom_viewport().scrollY || 0; } });
    Object.defineProperty(globalThis, 'scrollX', { configurable: true, get: function () { return 0; } });
    Object.defineProperty(globalThis, 'pageYOffset', { configurable: true, get: function () { return window.scrollY || 0; } });
    Object.defineProperty(globalThis, 'pageXOffset', { configurable: true, get: function () { return 0; } });
    Object.defineProperty(globalThis, 'innerWidth', { configurable: true, get: function () { return __native_dom_viewport().width || 1360; } });
    Object.defineProperty(globalThis, 'innerHeight', { configurable: true, get: function () { return __native_dom_viewport().height || 760; } });
    Object.defineProperty(globalThis, 'outerWidth', { configurable: true, get: function () { return window.innerWidth; } });
    Object.defineProperty(globalThis, 'outerHeight', { configurable: true, get: function () { return window.innerHeight; } });
  } catch (e) {}
  globalThis.scrollTo = function () {};
  globalThis.scrollBy = function () {};
  globalThis.focus = function () {};
  globalThis.blur = function () {};
  globalThis.print = function () {};
  globalThis.open = function () { return null; };
  globalThis.close = function () {};
  globalThis.getComputedStyle = function (el, pseudo) {
    // Minimal computed style: inline style reads; empty string otherwise.
    const target = el;
    return {
      getPropertyValue(prop) { return target && target.style ? (target.style[prop] || '') : ''; },
      get length() { return 0; },
      get cssText() { return ''; },
      item() { return ''; },
    };
  };
  globalThis.getSelection = function () { return null; };


  // ------------------------------------------------------------------ timers
  const timers = new Map();
  let nextTimerId = 1;
  globalThis.setTimeout = function (cb, delay) {
    if (typeof cb !== 'function') return 0;
    const id = nextTimerId++;
    const args = Array.prototype.slice.call(arguments, 2);
    timers.set(id, { cb, args, interval: false });
    __native_timer_start(id, Math.max(0, Number(delay) || 0), false);
    return id;
  };
  globalThis.setInterval = function (cb, delay) {
    if (typeof cb !== 'function') return 0;
    const id = nextTimerId++;
    const args = Array.prototype.slice.call(arguments, 2);
    timers.set(id, { cb, args, interval: true });
    __native_timer_start(id, Math.max(4, Number(delay) || 4), true);
    return id;
  };
  globalThis.clearTimeout = function (id) { timers.delete(id); __native_timer_clear(id); };
  globalThis.clearInterval = globalThis.clearTimeout;
  globalThis.queueMicrotask = function (cb) { Promise.resolve().then(cb); };
  globalThis.__onTimerFired = function (id) {
    const t = timers.get(id);
    if (!t) return;
    if (!t.interval) timers.delete(id);
    try { t.cb.apply(null, t.args); } catch (e) { console.error(errString(e)); }
  };

  // ------------------------------------------------------------------ storage
  globalThis.localStorage = {
    getItem(k) { const v = __native_ls_get(String(k)); return (v === null || v === undefined) ? null : v; },
    setItem(k, v) { __native_ls_set(String(k), String(v)); },
    removeItem(k) { __native_ls_remove(String(k)); },
    key(i) { return null; },
    get length() { return 0; },
  };

  // ------------------------------------------------------------------ fetch
  const pendingFetches = new Map();
  let nextFetchId = 1;
  class Response {
    constructor(status, headers, bodyB64) {
      this.status = status;
      this.headers = headers;
      this.ok = status >= 200 && status < 300;
      this._body = bodyB64 || '';
    }
    async text() { return __native_b64_decode(this._body); }
    async json() { return JSON.parse(await this.text()); }
    async arrayBuffer() {
      // Byte-exact path: the native decode maps every byte to a latin-1
      // char (binary bodies are not valid UTF-8 and would be corrupted by
      // the text() path).
      const bin = __native_b64_decode_latin1(this._body);
      if (bin === null || bin === undefined) {
        return new ArrayBuffer(0);
      }
      const buf = new ArrayBuffer(bin.length);
      const u8 = new Uint8Array(buf);
      for (let i = 0; i < bin.length; i++) {
        u8[i] = bin.charCodeAt(i);
      }
      return buf;
    }
    clone() { return new Response(this.status, this.headers, this._body); }
  }
  globalThis.Response = Response;
  globalThis.fetch = function (input, init) {
    init = init || {};
    const url = typeof input === 'string' ? input : String(input && input.url);
    return new Promise(function (resolve, reject) {
      const id = nextFetchId++;
      const headers = init.headers || {};
      let body = null;
      if (typeof init.body === 'string') body = init.body;
      const initJson = JSON.stringify({
        method: init.method || 'GET',
        headers: headers,
        body: body,
      });
      pendingFetches.set(id, { resolve, reject });
      __native_fetch_start(id, url, initJson);
    });
  };
  globalThis.__onFetchCompleted = function (id, status, headersJson, bodyB64) {
    const p = pendingFetches.get(id);
    if (!p) return;
    pendingFetches.delete(id);
    let headers = {};
    try { headers = JSON.parse(headersJson); } catch (e) {}
    p.resolve(new Response(status, headers, bodyB64));
  };
  globalThis.__onFetchFailed = function (id, error) {
    const p = pendingFetches.get(id);
    if (!p) return;
    pendingFetches.delete(id);
    p.reject(new TypeError(error));
  };

  // ------------------------------------------------------------------ XHR
  class XMLHttpRequest {
    constructor() {
      this.readyState = 0; this.status = 0; this.statusText = '';
      this.responseText = ''; this.response = ''; this._headers = {};
      this._ls = {}; this._method = 'GET'; this._url = '';
    }
    open(method, url) { this._method = String(method || 'GET'); this._url = String(url); this.readyState = 1; }
    setRequestHeader(n, v) { this._headers[String(n)] = String(v); }
    send(body) {
      const self = this;
      fetch(this._url, { method: this._method, headers: this._headers, body: body })
        .then(async function (r) {
          self.status = r.status; self.readyState = 4;
          self.responseText = await r.text(); self.response = self.responseText;
          fire(self, 'load'); fire(self, 'loadend');
        })
        .catch(function () {
          self.readyState = 4;
          fire(self, 'error'); fire(self, 'loadend');
        });
    }
    abort() { this.readyState = 0; }
    addEventListener(t, cb) { (this._ls[t] = this._ls[t] || []).push(cb); }
    get onreadystatechange() { return this._ls.onreadystatechange; }
    set onreadystatechange(cb) { this._ls.onreadystatechange = [cb]; }
  }
  globalThis.XMLHttpRequest = XMLHttpRequest;
  function fire(target, type) {
    (target._ls[type] || []).forEach(function (cb) {
      try { cb.call(target, { type: type, target: target }); } catch (e) {}
    });
  }

  // ------------------------------------------------------------------ DOM
  // Wrapper identity: one JS object per arena node (NodeId → wrapper).
  // Without it `document.getElementById('x') === getElementById('x')` was
  // false, element listeners were lost, and custom-element instances could
  // never be re-discovered — the prerequisite for WebComponents.
  const wraps = new Map();
  // Pending upgrade handle consumed by the Element constructor when a
  // custom-element class is `new`ed onto an existing node.
  let __wcUpgradeHandle = null;
  // tag → { ctor, observed } for defined custom elements.
  const registry = new Map();
  // tag → [resolve] for pending whenDefined promises.
  const definedResolvers = new Map();
  // JSON array of registered tags for the native custom-subtree scan.
  let registryTagsJson = '[]';

  function errString(e) {
    if (!e) return 'unknown error';
    if (e instanceof Error) return e.stack || (e.name + ': ' + e.message);
    return String(e);
  }

  function safeCall(target, method, ...args) {
    try {
      if (target && typeof target[method] === 'function') target[method](...args);
    } catch (e) {
      console.error(method + ' callback: ' + errString(e));
    }
  }

  function refreshRegistryTags() {
    registryTagsJson = JSON.stringify(Array.from(registry.keys()));
  }

  // Upgrade an existing arena node to its registered custom-element class.
  // Constructs the instance with the node pre-bound (via
  // __wcUpgradeHandle), registers it as the node's wrapper and fires
  // connectedCallback when the node is in the document.
  function tryUpgrade(handle, tag) {
    const def = registry.get(tag);
    if (!def) return null;
    const existing = wraps.get(handle);
    if (existing && existing._isCustom) return existing;
    if (existing) wraps.delete(handle); // replace the plain wrapper
    const prevPending = __wcUpgradeHandle;
    __wcUpgradeHandle = handle;
    let inst = null;
    try {
      inst = new def.ctor();
    } catch (e) {
      console.error('custom element constructor ' + tag + ': ' + ((e && e.message) || String(e)));
    }
    if (__wcUpgradeHandle === handle) __wcUpgradeHandle = null;
    __wcUpgradeHandle = prevPending;
    if (!inst || inst._h !== handle) {
      // Constructor failed or misbehaved: fall back to a plain wrapper so
      // the node stays usable.
      if (inst && inst._h === 0) inst._h = handle;
      else if (!inst) { const cls = (__htmlClassesRef || {})[tagClass(tag)] || Element; inst = new cls(handle); }
    }
    inst._isCustom = true;
    inst._def = def;
    inst._connected = false;
    wraps.set(handle, inst);
    if (__native_dom_isConnected(handle)) {
      inst._connected = true;
      safeCall(inst, 'connectedCallback');
    }
    return inst;
  }

  // After a subtree insertion: upgrade + connect every custom element it
  // contains (the native scan keeps this cheap for large subtrees).
  function afterInsert(rootHandle) {
    if (!rootHandle) return;
    if (!__native_dom_isConnected(rootHandle)) return;
    const handles = JSON.parse(__native_dom_findCustomTags(rootHandle, registryTagsJson) || '[]');
    for (const h of handles) {
      const w = wraps.get(h);
      if (w && w._isCustom) {
        if (!w._connected) { w._connected = true; safeCall(w, 'connectedCallback'); }
      } else if (!w) {
        tryUpgrade(h, __native_dom_tagName(h));
      }
    }
  }

  // Before removal: collect custom descendants of a connected subtree so
  // disconnectedCallback can fire after the detach.
  function collectForDisconnect(rootHandle) {
    if (!rootHandle || !__native_dom_isConnected(rootHandle)) return [];
    return JSON.parse(__native_dom_findCustomTags(rootHandle, registryTagsJson) || '[]');
  }
  function fireDisconnected(handles) {
    for (const h of handles) {
      const w = wraps.get(h);
      if (w && w._isCustom && w._connected) { w._connected = false; safeCall(w, 'disconnectedCallback'); }
    }
  }

  function dropWrappers(freedJson) {
    try {
      const freed = JSON.parse(freedJson || '[]');
      for (const h of freed) { wraps.delete(h); }
    } catch (e) {}
  }

  class Element {
    constructor(handle) {
      let h = handle;
      if (h === undefined || h === null) {
        if (__wcUpgradeHandle !== null) { h = __wcUpgradeHandle; __wcUpgradeHandle = null; }
        else { h = 0; }
      }
      this._h = h;
      if (h) wraps.set(h, this);
    }
    getAttribute(n) { const v = __native_dom_getAttr(this._h, String(n)); return (v === null || v === undefined) ? null : v; }
    setAttribute(n, v) {
      n = String(n); v = String(v);
      const isObserved = this._isCustom && this._def && this._def.observed && this._def.observed.indexOf(n) >= 0;
      const old = isObserved ? this.getAttribute(n) : null;
      __native_dom_setAttr(this._h, n, v);
      if (isObserved) safeCall(this, 'attributeChangedCallback', n, old, v);
    }
    removeAttribute(n) {
      n = String(n);
      const isObserved = this._isCustom && this._def && this._def.observed && this._def.observed.indexOf(n) >= 0;
      const old = isObserved ? this.getAttribute(n) : null;
      __native_dom_removeAttr(this._h, n);
      if (isObserved) safeCall(this, 'attributeChangedCallback', n, old, null);
    }
    hasAttribute(n) { return this.getAttribute(n) !== null; }
    appendChild(child) {
      if (child && child._h) {
        if (child.nodeType === 11) {
          // DocumentFragment: its children are moved, not the fragment.
          const kids = JSON.parse(__native_dom_childNodes(child._h) || '[]');
          for (const k of kids) { __native_dom_appendChild(this._h, k); afterInsert(k); }
          return child;
        }
        __native_dom_appendChild(this._h, child._h);
        afterInsert(child._h);
      }
      return child;
    }
    removeChild(child) {
      if (child && child._h) {
        const customs = collectForDisconnect(child._h);
        __native_dom_removeChild(this._h, child._h);
        fireDisconnected(customs);
      }
      return child;
    }
    insertBefore(child, ref) {
      if (child && child._h) {
        const refH = (ref && ref._h) ? ref._h : 0;
        if (child.nodeType === 11) {
          const kids = JSON.parse(__native_dom_childNodes(child._h) || '[]');
          for (const k of kids) { __native_dom_insertBefore(this._h, k, refH); afterInsert(k); }
          return child;
        }
        __native_dom_insertBefore(this._h, child._h, refH);
        afterInsert(child._h);
      }
      return child;
    }
    replaceChild(newChild, oldChild) {
      if (newChild && newChild._h && oldChild && oldChild._h) {
        const customs = collectForDisconnect(oldChild._h);
        this.insertBefore(newChild, oldChild);
        this.removeChild(oldChild);
        fireDisconnected(customs);
      }
      return oldChild;
    }
    get parentNode() { const h = __native_dom_parentNode(this._h); return h ? wrapElement(h) : null; }
    get childNodes() { return JSON.parse(__native_dom_childNodes(this._h) || '[]').map(wrapElement); }
    get children() { return this.childNodes.filter(function (n) { return n && n.nodeType === 1; }); }
    get firstChild() { const kids = JSON.parse(__native_dom_childNodes(this._h) || '[]'); return kids.length ? wrapElement(kids[0]) : null; }
    get nextSibling() { const h = __native_dom_nextSibling(this._h); return h ? wrapElement(h) : null; }
    get previousSibling() { const h = __native_dom_prevSibling(this._h); return h ? wrapElement(h) : null; }
    get nextElementSibling() { let n = this.nextSibling; while (n && n.nodeType !== 1) n = n.nextSibling; return n; }
    get previousElementSibling() { let n = this.previousSibling; while (n && n.nodeType !== 1) n = n.previousSibling; return n; }
    get parentElement() { const p = this.parentNode; return (p && p.nodeType === 1) ? p : null; }
    get firstElementChild() { const c = this.children; return c.length ? c[0] : null; }
    get lastElementChild() { const c = this.children; return c.length ? c[c.length - 1] : null; }
    get childElementCount() { return this.children.length; }
    get nodeValue() { return this.nodeType === 3 ? this.textContent : null; }
    get nodeType() { return __native_dom_nodeType(this._h); }
    get isConnected() { return !!__native_dom_isConnected(this._h); }
    hasChildNodes() { return JSON.parse(__native_dom_childNodes(this._h) || '[]').length > 0; }
    cloneNode(deep) {
      const h = __native_dom_cloneNode(this._h, !!deep);
      if (!h) return null;
      return this.nodeType === 11 ? new DocumentFragment(h) : wrapElement(h);
    }
    get tagName() { return __native_dom_tagName(this._h); }
    get textContent() { return __native_dom_textContent(this._h); }
    set textContent(t) {
      dropWrappers(__native_dom_setTextContent2(this._h, String(t)));
      markCustomText(this._h);
    }
    get innerHTML() { return __native_dom_getInnerHTML(this._h); }
    set innerHTML(html) {
      dropWrappers(__native_dom_setInnerHTML(this._h, String(html)));
      afterInsert(this._h);
    }
    get outerHTML() { return __native_dom_getInnerHTML(this._h); }
    querySelector(sel) { const h = __native_dom_querySelectorIn(this._h, String(sel)); return h ? wrapElement(h) : null; }
    querySelectorAll(sel) { return JSON.parse(__native_dom_querySelectorAllIn(this._h, String(sel)) || '[]').map(wrapElement); }
    get style() {
      const self = this;
      return new Proxy({}, {
        get(_, prop) { return __native_dom_style_get(self._h, String(prop)); },
        set(_, prop, v) { __native_dom_style_set(self._h, String(prop), String(v)); return true; },
      });
    }
    addEventListener(type, cb) {
      type = String(type);
      if (typeof cb !== 'function') return;
      const ls = (this._ls = this._ls || {});
      const list = (ls[type] = ls[type] || []);
      if (list.indexOf(cb) < 0) list.push(cb);
    }
    removeEventListener(type, cb) {
      const ls = this._ls;
      if (!ls || !ls[type]) return;
      if (cb) { const i = ls[type].indexOf(cb); if (i >= 0) ls[type].splice(i, 1); }
      else delete ls[type];
    }
    dispatchEvent(ev) {
      ev = ev || { type: 'unknown' };
      ev.target = this;
      fireAt(this._h, ev.type, ev);
      return true;
    }
    click() { __native_dom_click(this._h); }
    focus() {}
    blur() {}
    get id() { return this.getAttribute('id') || ''; }
    set id(v) { this.setAttribute('id', v); }
    get className() { return this.getAttribute('class') || ''; }
    set className(v) { this.setAttribute('class', v); }
    get classList() {
      const self = this;
      return {
        add(...cs) { const cur = (self.getAttribute('class') || '').split(/\s+/).filter(Boolean); for (const c of cs) if (cur.indexOf(c) < 0) cur.push(String(c)); self.setAttribute('class', cur.join(' ')); },
        remove(...cs) { const cur = (self.getAttribute('class') || '').split(/\s+/).filter(Boolean).filter(c => cs.indexOf(c) < 0); self.setAttribute('class', cur.join(' ')); },
        contains(c) { return (self.getAttribute('class') || '').split(/\s+/).indexOf(String(c)) >= 0; },
        toggle(c) { const cur = (self.getAttribute('class') || '').split(/\s+/).filter(Boolean); const i = cur.indexOf(String(c)); if (i >= 0) cur.splice(i, 1); else cur.push(String(c)); self.setAttribute('class', cur.join(' ')); return i < 0; },
        get length() { return (self.getAttribute('class') || '').split(/\s+/).filter(Boolean).length; },
      };
    }
    attachShadow(opts) {
      const root = __native_dom_attachShadow(this._h);
      if (root === null || root === undefined) return null;
      return new ShadowRoot(root, this);
    }
    get shadowRoot() {
      const h = __native_dom_shadowRootOf(this._h);
      return h ? new ShadowRoot(h, this) : null;
    }
    getRootNode() { return document; }
    closest(sel) {
      let h = this._h;
      let guard = 0;
      while (h && guard++ < 100) {
        const found = __native_dom_matchesSelector(h, String(sel));
        if (found) return wrapElement(h);
        h = __native_dom_parentNode(h);
      }
      return null;
    }
    matches(sel) { return !!__native_dom_matchesSelector(this._h, String(sel)); }
    getBoundingClientRect() {
      return { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0 };
    }
  }

  function markCustomText(h) { /* replaced children are plain text — nothing to upgrade */ }

  class ShadowRoot extends Element {
    constructor(handle, host) {
      super(handle);
      this._host = host || null;
    }
    get nodeType() { return 11; }
    get host() { return this._host || (function () { const h = __native_dom_shadowHost(this._h); return h ? wrapElement(h) : null; })(); }
    get mode() { return 'open'; }
    getElementById(id) {
      const h = __native_dom_querySelectorIn(this._h, '#' + String(id).replace(/[^\w-]/g, ''));
      return h ? wrapElement(h) : null;
    }
  }
  globalThis.ShadowRoot = ShadowRoot;

  class DocumentFragment extends Element {
    constructor(handle) { super(handle); }
    get nodeType() { return 11; }
  }
  globalThis.DocumentFragment = DocumentFragment;

  class Document {
    constructor() { this._currentScript = null; }
    getElementById(id) { const h = __native_dom_getElementById(String(id)); return (h === null || h === undefined || h === 0) ? null : wrapElement(h); }
    querySelector(sel) { const h = __native_dom_querySelector(String(sel)); return (h === null || h === undefined) ? null : wrapElement(h); }
    querySelectorAll(sel) {
      const handles = JSON.parse(__native_dom_querySelectorAll(String(sel)) || '[]');
      return handles.map(function (h) { return wrapElement(h); });
    }
    getElementsByTagName(tag) { return this.querySelectorAll(String(tag)); }
    getElementsByClassName(cls) { return this.querySelectorAll('.' + String(cls)); }
    getElementsByName() { return []; }
    createElement(tag) {
      tag = String(tag);
      const lower = tag.toLowerCase();
      const h = __native_dom_createElement(lower);
      if (registry.has(lower)) return tryUpgrade(h, lower);
      const cls = htmlClasses[tagClass(lower)] || Element;
      return new cls(h);
    }
    createTextNode(text) { return wrapElement(__native_dom_createTextNode(String(text))); }
    createComment(text) { return wrapElement(__native_dom_createComment(String(text))); }
    createDocumentFragment() { return new DocumentFragment(__native_dom_createComment('fragment')); }
    importNode(node, deep) {
      if (!node || !node._h) return node;
      const h = __native_dom_importNode(node._h, !!deep);
      return h ? wrapElement(h) : null;
    }
    adoptNode(node) { return node; }
    get body() { const h = __native_dom_body(); return h === null ? null : wrapElement(h); }
    get head() { const h = __native_dom_querySelector('head'); return h === null ? null : wrapElement(h); }
    get documentElement() { const h = __native_dom_html(); return h === null ? null : wrapElement(h); }
    get title() { return __native_document_title(); }
    set title(t) { /* engine-managed; JS title sets are a UI nicety for later */ }
    // Scripts here run after the document is parsed and subresources are
    // in — the 'complete' answer keeps `readyState === 'complete'` bootstraps
    // (very common) on the fast path. WRITABLE because WebComponents
    // polyfills emulate readyState transitions.
    get readyState() { return this._readyState || 'complete'; }
    set readyState(v) { this._readyState = String(v); }
    get visibilityState() { return 'visible'; }
    get hidden() { return false; }
    get currentScript() { return this._currentScript; }
    get cookie() { return ''; }
    set cookie(value) { /* engine cookie-jar integration is a documented gap */ }
    get timeline() { return { currentTime: 0, play() {} }; }
    getAnimations() { return []; }
    createElementNS(ns, tag) { return this.createElement(tag); }
    createTreeWalker(root, whatToShow, filter) { return new TreeWalker(root); }
    createNodeIterator(root, whatToShow, filter) { return new TreeWalker(root); }
    get implementation() { return DOMImplementation; }
    get activeElement() { return document.body; }
    hasFocus() { return true; }
    get styleSheets() { return []; }
    addEventListener(type, cb) { type = String(type); const ls = (this._ls = this._ls || {}); const list = (ls[type] = ls[type] || []); if (typeof cb === 'function' && list.indexOf(cb) < 0) list.push(cb); }
    removeEventListener(type, cb) { const ls = this._ls; if (!ls || !ls[type]) return; if (cb) { const i = ls[type].indexOf(cb); if (i >= 0) ls[type].splice(i, 1); } else delete ls[type]; }
  }
  globalThis.document = new Document();
  globalThis.Document = Document;
  globalThis.Element = Element;
  globalThis.Node = Element;
  globalThis.Text = Element;
  globalThis.__setCurrentScript = function (h) {
    document._currentScript = h ? wrapElement(h) : null;
  };

  // ------------------------------------------------------------------ DOM classes
  // `class X extends HTMLElement` is ubiquitous; without the HTML* element
  // classes every modern framework's class registration throws.
  let __htmlClassesRef = null;
  const HTMLElementBase = class HTMLElement extends Element {};

  // HTMLMediaElement: state read from the engine-maintained mirror, control
  // via native commands. Loaded by <video>/<audio> elements (parsed or
  // createElement) and by querySelector results wrapped by tag.
  const mediaMirrors = new Map();
  class HTMLMediaElement extends HTMLElementBase {
    _mirror() {
      const json = __native_media_mirror(this._h);
      if (json) { try { mediaMirrors.set(this._h, JSON.parse(json)); } catch (e) {} }
      return mediaMirrors.get(this._h);
    }
    play() {
      __native_media_play(this._h);
      this._mirror().paused = false;
      return Promise.resolve();
    }
    pause() {
      __native_media_pause(this._h);
      this._mirror().paused = true;
    }
    load() {}
    canPlayType(t) {
      const s = String(t).toLowerCase();
      if (s.includes('avc1') || s.includes('avc3') || s.includes('h264')) return 'probably';
      if (s.includes('mp4a') || s.includes('aac')) return 'probably';
      if (s.includes('mp4') || s.includes('m4v')) return 'maybe';
      return '';
    }
    get paused() { const m = this._mirror(); return m ? m.paused : true; }
    get currentTime() { const m = this._mirror(); return m ? m.time : 0; }
    set currentTime(t) { __native_media_seek(this._h, Number(t) || 0); }
    get duration() { const m = this._mirror(); return m ? (m.duration || NaN) : NaN; }
    get videoWidth() { const m = this._mirror(); return m ? m.width : 0; }
    get videoHeight() { const m = this._mirror(); return m ? m.height : 0; }
    get readyState() { const m = this._mirror(); return m ? m.readyState : 0; }
    get error() { const m = this._mirror(); return m && m.error ? { message: m.error } : null; }
    get volume() { return this._volume === undefined ? 1.0 : this._volume; }
    set volume(v) { this._volume = Number(v); __native_media_set_volume(this._h, this._volume); }
    get muted() { return !!this._muted; }
    set muted(v) { this._muted = !!v; __native_media_set_muted(this._h, this._muted); }
    get src() { return this.getAttribute('src') || ''; }
    set src(v) {
      this.setAttribute('src', String(v));
      __native_media_set_src(this._h, String(v));
    }
    addTextTrack() { return { cues: [], addCue() {}, removeCue() {} }; }
    get textTracks() { return []; }
    get buffered() {
      const m = this._mirror();
      const end = m && m.bufferedEnd ? m.bufferedEnd : 0;
      const has = end > 0;
      return { length: has ? 1 : 0, start: function (i) { return 0; }, end: function (i) { return i === 0 && has ? end : 0; } };
    }
  }
  // Uncaught errors surface in the devtools console (default handler;
  // pages may override window.onerror as usual).
  globalThis.onerror = function (msg) {
    try { console.error('Uncaught: ' + (msg && msg.message ? msg.message : msg)); } catch (e) {}
    return false;
  };
  // Media events are dispatched per node by the engine.
  HTMLMediaElement.prototype.onended = null;
  const htmlClasses = {
    HTMLElement: HTMLElementBase,
    HTMLDivElement: class HTMLDivElement extends HTMLElementBase {},
    HTMLSpanElement: class HTMLSpanElement extends HTMLElementBase {},
    HTMLAnchorElement: class HTMLAnchorElement extends HTMLElementBase {},
    HTMLImageElement: class HTMLImageElement extends HTMLElementBase {},
    HTMLScriptElement: class HTMLScriptElement extends HTMLElementBase {},
    HTMLStyleElement: class HTMLStyleElement extends HTMLElementBase {},
    HTMLLinkElement: class HTMLLinkElement extends HTMLElementBase {},
    HTMLInputElement: class HTMLInputElement extends HTMLElementBase {},
    HTMLButtonElement: class HTMLButtonElement extends HTMLElementBase {},
    HTMLFormElement: class HTMLFormElement extends HTMLElementBase {},
    HTMLBodyElement: class HTMLBodyElement extends HTMLElementBase {},
    HTMLHeadElement: class HTMLHeadElement extends HTMLElementBase {},
    HTMLTemplateElement: class HTMLTemplateElement extends HTMLElementBase {
      get content() {
        const h = __native_template_content(this._h);
        return h ? new DocumentFragment(h) : new DocumentFragment(this._h);
      }
    },
    HTMLVideoElement: class HTMLVideoElement extends HTMLMediaElement {},
    HTMLAudioElement: class HTMLAudioElement extends HTMLMediaElement {},
    HTMLCanvasElement: class HTMLCanvasElement extends HTMLElementBase {},
    HTMLIFrameElement: class HTMLIFrameElement extends HTMLElementBase {},
    HTMLUnknownElement: class HTMLUnknownElement extends HTMLElementBase {},
    // The rest of the HTML element family: feature detection ("textarea"
    // in window) and instanceof checks reach for these constantly.
    HTMLTextAreaElement: class HTMLTextAreaElement extends HTMLElementBase {},
    HTMLSelectElement: class HTMLSelectElement extends HTMLElementBase {},
    HTMLOptionElement: class HTMLOptionElement extends HTMLElementBase {},
    HTMLOptGroupElement: class HTMLOptGroupElement extends HTMLElementBase {},
    HTMLLabelElement: class HTMLLabelElement extends HTMLElementBase {},
    HTMLFieldSetElement: class HTMLFieldSetElement extends HTMLElementBase {},
    HTMLLegendElement: class HTMLLegendElement extends HTMLElementBase {},
    HTMLTableElement: class HTMLTableElement extends HTMLElementBase {},
    HTMLTableSectionElement: class HTMLTableSectionElement extends HTMLElementBase {},
    HTMLTableRowElement: class HTMLTableRowElement extends HTMLElementBase {},
    HTMLTableCellElement: class HTMLTableCellElement extends HTMLElementBase {},
    HTMLTableCaptionElement: class HTMLTableCaptionElement extends HTMLElementBase {},
    HTMLTableColElement: class HTMLTableColElement extends HTMLElementBase {},
    HTMLTitleElement: class HTMLTitleElement extends HTMLElementBase {},
    HTMLMetaElement: class HTMLMetaElement extends HTMLElementBase {},
    HTMLBaseElement: class HTMLBaseElement extends HTMLElementBase {},
    HTMLAreaElement: class HTMLAreaElement extends HTMLElementBase {},
    HTMLMapElement: class HTMLMapElement extends HTMLElementBase {},
    HTMLQuoteElement: class HTMLQuoteElement extends HTMLElementBase {},
    HTMLPreElement: class HTMLPreElement extends HTMLElementBase {},
    HTMLBRElement: class HTMLBRElement extends HTMLElementBase {},
    HTMLHRElement: class HTMLHRElement extends HTMLElementBase {},
    HTMLModElement: class HTMLModElement extends HTMLElementBase {},
    HTMLPictureElement: class HTMLPictureElement extends HTMLElementBase {},
    HTMLSourceElement: class HTMLSourceElement extends HTMLElementBase {},
    HTMLTrackElement: class HTMLTrackElement extends HTMLElementBase {},
    HTMLParamElement: class HTMLParamElement extends HTMLElementBase {},
    HTMLEmbedElement: class HTMLEmbedElement extends HTMLElementBase {},
    HTMLObjectElement: class HTMLObjectElement extends HTMLElementBase {},
    HTMLDetailsElement: class HTMLDetailsElement extends HTMLElementBase {},
    HTMLSummaryElement: class HTMLSummaryElement extends HTMLElementBase {},
    HTMLDialogElement: class HTMLDialogElement extends HTMLElementBase {},
    HTMLDataListElement: class HTMLDataListElement extends HTMLElementBase {},
    HTMLOutputElement: class HTMLOutputElement extends HTMLElementBase {},
    HTMLProgressElement: class HTMLProgressElement extends HTMLElementBase {},
    HTMLMeterElement: class HTMLMeterElement extends HTMLElementBase {},
    HTMLMarqueeElement: class HTMLMarqueeElement extends HTMLElementBase {},
    SVGSVGElement: class SVGSVGElement extends Element {},
    SVGElement: class SVGElement extends Element {},
  };
  for (const name of Object.keys(htmlClasses)) globalThis[name] = htmlClasses[name];
  __htmlClassesRef = htmlClasses;
  // Extra constructor globals probed by polyfills and framework feature
  // detection (documented as classes, not just instances). The WebComponents
  // polyfill walks ["Text","Comment","CDATASection","ProcessingInstruction"]
  // patching window[name].prototype — every one of these must exist.
  globalThis.HTMLElement = HTMLElementBase;
  globalThis.HTMLSlotElement = class HTMLSlotElement extends HTMLElementBase {};
  globalThis.HTMLContentElement = class HTMLContentElement extends HTMLElementBase {};
  globalThis.HTMLUnknownElement = htmlClasses.HTMLUnknownElement;
  globalThis.CharacterData = class CharacterData extends Element {};
  globalThis.Comment = class Comment extends Element {};
  globalThis.CDATASection = class CDATASection extends Element {};
  globalThis.ProcessingInstruction = class ProcessingInstruction extends Element {};
  globalThis.DocumentType = class DocumentType extends Element {};
  globalThis.Window = class Window {};
  globalThis.Attr = class Attr extends Element {};
  globalThis.Range = class Range {
    constructor() { this.startContainer = null; this.endContainer = null; this.collapsed = true; }
    setStart() {} setEnd() {} collapse() {} selectNodeContents() {}
    getBoundingClientRect() { return { top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0 }; }
    getClientRects() { return []; }
    cloneContents() { return document.createDocumentFragment(); }
    cloneRange() { return new Range(); }
    commonAncestorContainer() { return null; }
  };
  globalThis.Selection = class Selection {
    addRange() {} removeAllRanges() {} getRangeAt() { return null; } get toString() { return ''; }
  };

  // Event classes (constructors are feature-probed constantly).
  globalThis.Event = class Event {
    constructor(type, opts) {
      this.type = String(type);
      this.bubbles = !!(opts && opts.bubbles);
      this.cancelable = !!(opts && opts.cancelable);
      this.target = null;
      this.defaultPrevented = false;
    }
    preventDefault() { this.defaultPrevented = true; }
    stopPropagation() {}
    stopImmediatePropagation() {}
  };
  globalThis.CustomEvent = class CustomEvent extends globalThis.Event {
    constructor(type, opts) {
      super(type, opts);
      this.detail = opts && opts.detail;
    }
  };
  globalThis.MouseEvent = class MouseEvent extends globalThis.Event {};
  globalThis.KeyboardEvent = class KeyboardEvent extends globalThis.Event {};
  globalThis.FocusEvent = class FocusEvent extends globalThis.Event {};
  globalThis.EventTarget = class EventTarget {
    addEventListener() {}
    removeEventListener() {}
    dispatchEvent() { return true; }
  };

  // WebComponents: the custom-element registry. Definitions upgrade
  // matching elements (parser-inserted and JS-created), fire the v1
  // callback set (connected/disconnected/attributeChanged) through the
  // mutation hooks in the Element class, and honor whenDefined.
  globalThis.customElements = {
    define(name, ctor) {
      name = String(name).toLowerCase();
      if (typeof ctor !== 'function') throw new TypeError('constructor required');
      if (registry.has(name)) throw new Error("a custom element with name '" + name + "' is already defined");
      let observed = null;
      try { observed = ctor.observedAttributes; } catch (e) {}
      registry.set(name, {
        ctor,
        observed: Array.isArray(observed) ? observed.map(String) : (observed ? [String(observed)] : null),
      });
      refreshRegistryTags();
      // Upgrade every element with this tag already in the document.
      const existing = JSON.parse(__native_dom_querySelectorAll(name) || '[]');
      for (const h of existing) {
        if (!wraps.get(h)) tryUpgrade(h, name);
      }
      const rs = definedResolvers.get(name);
      if (rs) { for (const r of rs) { try { r(); } catch (e) {} } definedResolvers.delete(name); }
    },
    get(name) { const def = registry.get(String(name).toLowerCase()); return def ? def.ctor : undefined; },
    whenDefined(name) {
      name = String(name).toLowerCase();
      if (registry.has(name)) return Promise.resolve();
      return new Promise(function (res) {
        let list = definedResolvers.get(name);
        if (!list) { list = []; definedResolvers.set(name, list); }
        list.push(res);
      });
    },
    upgrade(root) {
      if (!root || root._h === undefined) return;
      const handles = JSON.parse(__native_dom_findCustomTags(root._h, registryTagsJson) || '[]');
      for (const h of handles) {
        if (!wraps.get(h)) tryUpgrade(h, __native_dom_tagName(h));
      }
    },
    // webcomponents.js polyfill hook: the polyfill asks native custom
    // elements to wrap its flush callback; we run flushes eagerly.
    polyfillWrapFlushCallback(cb) { if (typeof cb === 'function') { try { cb(); } catch (e) {} } },
    polyfillIfNeeded() { return true; },
  };
  // ---- MutationObserver (real: records captured in Rust at mutation
  // time, delivered at the end of each script turn via __onMutations) ----
  const moRegistry = new Map();
  let moNextId = 1;
  function moDetached(d) {
    // A node removed before delivery: materialize a detached pseudo-element
    // from the descriptor captured at mutation time (Chrome keeps removed
    // nodes alive; our arena recycles the handle).
    const stub = {
      nodeType: d.nodeType || 1,
      tagName: d.tag || '',
      nodeName: d.tag || '',
      id: d.id || '',
      className: d.cls || '',
      isConnected: false,
      parentNode: null,
      parentElement: null,
      childNodes: [],
      children: [],
      getAttribute(n) {
        if (n === 'id') return d.id || null;
        if (n === 'class') return d.cls || null;
        return null;
      },
      hasAttribute(n) { return this.getAttribute(n) !== null; },
      getBoundingClientRect() { return { top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0, x: 0, y: 0 }; },
      querySelector() { return null; },
      querySelectorAll() { return []; },
      get textContent() { return ''; },
      get innerHTML() { return ''; },
      get outerHTML() { return ''; },
      contains() { return false; },
      matches() { return false; },
      closest() { return null; },
    };
    return stub;
  }
  globalThis.MutationObserver = class MutationObserver {
    constructor(cb) {
      if (typeof cb !== 'function') throw new TypeError("MutationObserver callback must be a function");
      this._cb = cb;
      this._id = moNextId++;
      moRegistry.set(this._id, this);
    }
    observe(target, options) {
      if (!target || target._h === undefined) throw new TypeError("MutationObserver.observe: target must be a Node");
      const o = options || {};
      if (!o.childList && !o.attributes && !o.characterData) {
        throw new TypeError("MutationObserver.observe: one of childList, attributes, characterData must be true");
      }
      const want = {
        childList: !!o.childList,
        attributes: !!o.attributes,
        characterData: !!o.characterData,
        subtree: !!o.subtree,
        attributeOldValue: !!o.attributeOldValue,
        characterDataOldValue: !!o.characterDataOldValue,
      };
      this._want = want;
      __native_mo_observe(this._id, target._h, JSON.stringify(want));
    }
    disconnect() {
      moRegistry.delete(this._id);
      try { __native_mo_disconnect(this._id); } catch (e) {}
    }
    takeRecords() { return []; }
  };
  globalThis.__onMutations = function (id, json) {
    const obs = moRegistry.get(id);
    if (!obs || !obs._cb) return;
    const raw = (typeof json === 'string') ? JSON.parse(json) : json;
    const records = raw.map(function (r) {
      const want = obs._want || {};
      const rec = {
        type: r.type,
        target: wrapElement(r.target),
        addedNodes: (r.added || []).map(function (h) { return wrapElement(h); }),
        removedNodes: (r.removed || []).map(moDetached),
        previousSibling: (r.prev !== null && r.prev !== undefined) ? wrapElement(r.prev) : null,
        nextSibling: (r.next !== null && r.next !== undefined) ? wrapElement(r.next) : null,
        attributeName: null,
        oldValue: null,
      };
      if (r.type === 'attributes') {
        rec.attributeName = r.name;
        rec.oldValue = want.attributeOldValue ? (r.old || null) : null;
      } else if (r.type === 'characterData') {
        rec.oldValue = want.characterDataOldValue ? (r.old || null) : null;
      }
      return rec;
    });
    if (!records.length) return;
    try { obs._cb(records, obs); } catch (e) { console.error(errString(e)); }
  };
  globalThis.IntersectionObserver = class IntersectionObserver {
    constructor(cb) { this._cb = cb; }
    // Fire the callback asynchronously with isIntersecting: true — the
    // lazy-load contract (sites observe thumbnails, then swap in the real
    // src only once "visible"). Without real occlusion data everything
    // counts as intersecting, which lights up lazy thumbnails everywhere.
    observe(el) {
      const cb = this._cb, obs = this;
      setTimeout(function () {
        try {
          cb([{
            target: el, isIntersecting: true, intersectionRatio: 1,
            boundingClientRect: el.getBoundingClientRect ? el.getBoundingClientRect() : { top: 0, left: 0, width: 0, height: 0 },
            rootBounds: null, time: Date.now(),
          }], obs);
        } catch (e) {}
      }, 0);
    }
    unobserve() {}
    disconnect() {}
    takeRecords() { return []; }
  };
  globalThis.ResizeObserver = class ResizeObserver {
    constructor(cb) { this._cb = cb; }
    // Fire once asynchronously with the element's current size — player
    // UIs wait for this to build their control bars.
    observe(el) {
      const cb = this._cb;
      setTimeout(function () {
        try {
          const r = el.getBoundingClientRect ? el.getBoundingClientRect() : { width: 0, height: 0 };
          cb([{ target: el, contentRect: { x: 0, y: 0, width: r.width, height: r.height, top: 0, left: 0, right: r.width, bottom: r.height } }]);
        } catch (e) {}
      }, 0);
    }
    unobserve() {}
    disconnect() {}
  };
  globalThis.PerformanceObserver = class PerformanceObserver {
    observe() {}
    disconnect() {}
    takeRecords() { return []; }
  };

  // Web Animations: element.animate() returning a resolved animation.
  const animationStub = {
    finished: Promise.resolve(),
    currentTime: 0,
    startTime: 0,
    playState: 'finished',
    play() {}, pause() {}, cancel() {}, finish() {}, reverse() {},
    addEventListener() {}, removeEventListener() {},
    onfinish: null,
  };
  Element.prototype.animate = function () { return animationStub; };
  Element.prototype.getBoundingClientRect = function () {
    // Real geometry from the engine's layout mirror (document space,
    // viewport-relative on return).
    try {
      const raw = __native_dom_get_rect(this._h);
      if (raw !== null && raw !== undefined) {
        const r = JSON.parse(raw);
        const top = r[1] - (window.scrollY || 0);
        return { x: r[0], y: top, left: r[0], top: top, right: r[0] + r[2], bottom: top + r[3], width: r[2], height: r[3] };
      }
    } catch (e) {}
    return { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0 };
  };
  Element.prototype.getClientRects = function () {
    const r = this.getBoundingClientRect();
    return r.width > 0 || r.height > 0 ? [r] : [];
  };
  Element.prototype.scrollIntoView = function () {};
  Element.prototype.getBoundingClientRect.toString = function () { return 'function getBoundingClientRect() { [native code] }'; };
  globalThis.Animation = class Animation {};
  globalThis.KeyframeEffect = class KeyframeEffect {};

  // ------------------------------------------------------------------ Node constants + tree walkers
  const NODE_TYPE = {
    ELEMENT_NODE: 1, ATTRIBUTE_NODE: 2, TEXT_NODE: 3, CDATA_SECTION_NODE: 4,
    ENTITY_REFERENCE_NODE: 5, ENTITY_NODE: 6, PROCESSING_INSTRUCTION_NODE: 7,
    COMMENT_NODE: 8, DOCUMENT_NODE: 9, DOCUMENT_TYPE_NODE: 10,
    DOCUMENT_FRAGMENT_NODE: 11, NOTATION_NODE: 12,
  };
  for (const k of Object.keys(NODE_TYPE)) Element[k] = NODE_TYPE[k];
  globalThis.NodeFilter = Object.assign(
    {
      FILTER_ACCEPT: 1, FILTER_REJECT: 2, FILTER_SKIP: 3,
      SHOW_ALL: 0xFFFFFFFF, SHOW_ELEMENT: 0x1, SHOW_ATTRIBUTE: 0x2,
      SHOW_TEXT: 0x4, SHOW_CDATA_SECTION: 0x8, SHOW_COMMENT: 0x80,
      SHOW_DOCUMENT: 0x100, SHOW_DOCUMENT_TYPE: 0x200,
      SHOW_DOCUMENT_FRAGMENT: 0x400,
    },
    { acceptNode() { return 1; } }
  );
  // Real tree walkers over the arena (childNodes + sibling links).
  globalThis.TreeWalker = class TreeWalker {
    constructor(root, whatToShow, filter) {
      this.root = root || null;
      this.currentNode = this.root;
      this.whatToShow = whatToShow === undefined ? 0xFFFFFFFF : whatToShow;
      this.filter = filter || null;
    }
    _ok(n) { return !!n; }
    parentNode() {
      if (!this.currentNode || !this.currentNode.parentNode) return null;
      this.currentNode = this.currentNode.parentNode;
      return this.currentNode;
    }
    firstChild() {
      if (!this.currentNode) return null;
      const c = this.currentNode.firstChild;
      if (c) this.currentNode = c;
      return c || null;
    }
    nextSibling() {
      if (!this.currentNode) return null;
      const n = this.currentNode.nextSibling;
      if (n) this.currentNode = n;
      return n || null;
    }
    nextNode() {
      // Pre-order traversal.
      let n = this.currentNode;
      if (!n) return null;
      if (n.firstChild) { this.currentNode = n.firstChild; return this.currentNode; }
      while (n) {
        if (n.nextSibling) { this.currentNode = n.nextSibling; return this.currentNode; }
        n = n.parentNode;
      }
      return null;
    }
    previousNode() {
      const p = this.currentNode && this.currentNode.parentNode;
      if (!p) return null;
      const prev = this.currentNode.previousSibling;
      if (prev) {
        let n = prev;
        while (n.lastChild) n = n.lastChild;
        this.currentNode = n;
        return n;
      }
      this.currentNode = p;
      return p;
    }
  };
  globalThis.NodeIterator = globalThis.TreeWalker;
  // Minimal DOMImplementation: createHTMLDocument returns a detached
  // document (a fragment holder in the same arena + a plain, fully
  // settable Document-like face — polyfills assign body/documentElement).
  const DOMImplementation = {
    createHTMLDocument(title) {
      const holder = __native_dom_createComment('html-document');
      let body = new DocumentFragment(holder);
      let docElem = body;
      const doc = {
        _root: holder,
        _h: holder,
        get nodeType() { return 9; },
        get body() { return body; },
        set body(v) { body = v; },
        get documentElement() { return docElem; },
        set documentElement(v) { docElem = v; },
        get head() { return null; },
        get title() { return typeof title === 'string' ? title : ''; },
        set title(v) {},
        get readyState() { return 'complete'; },
        set readyState(v) {},
        get currentScript() { return null; },
        createElement(tag) { return wrapElement(__native_dom_createElement(String(tag).toLowerCase())); },
        createElementNS(ns, tag) { return wrapElement(__native_dom_createElement(String(tag).toLowerCase())); },
        createTextNode(t) { return wrapElement(__native_dom_createTextNode(String(t))); },
        createComment(t) { return wrapElement(__native_dom_createComment(String(t))); },
        createDocumentFragment() { return new DocumentFragment(__native_dom_createComment('fragment')); },
        createTreeWalker(root) { return new TreeWalker(root); },
        createEvent() { return new Event(''); },
        getElementById(id) { return null; },
        querySelector(sel) { const h = __native_dom_querySelectorIn(holder, String(sel)); return h ? wrapElement(h) : null; },
        querySelectorAll(sel) { return JSON.parse(__native_dom_querySelectorAllIn(holder, String(sel)) || '[]').map(wrapElement); },
        addEventListener() {},
        removeEventListener() {},
        write() {},
        writeln() {},
        open() {},
        close() {},
        importNode(node, deep) { return document.importNode(node, deep); },
        adoptNode(node) { return node; },
      };
      return doc;
    },
    createDocument() { return this.createHTMLDocument(''); },
    hasFeature() { return true; },
  };
  globalThis.DOMImplementation = DOMImplementation;

  globalThis.DOMParser = class DOMParser {
    parseFromString() { return document; }
  };
  globalThis.XMLSerializer = class XMLSerializer {
    serializeToString() { return ''; }
  };

  // ------------------------------------------------------------------ encoding
  // Minimal UTF-8 TextEncoder/TextDecoder (QuickJS ships neither).
  globalThis.TextEncoder = class TextEncoder {
    encode(input) {
      const str = String(input === undefined ? '' : input);
      const out = [];
      for (let i = 0; i < str.length; i++) {
        let code = str.codePointAt(i);
        if (code > 0xFFFF) i++; // surrogate pair consumed
        if (code < 0x80) out.push(code);
        else if (code < 0x800) {
          out.push(0xC0 | (code >> 6), 0x80 | (code & 0x3F));
        } else if (code < 0x10000) {
          out.push(0xE0 | (code >> 12), 0x80 | ((code >> 6) & 0x3F), 0x80 | (code & 0x3F));
        } else {
          out.push(
            0xF0 | (code >> 18), 0x80 | ((code >> 12) & 0x3F),
            0x80 | ((code >> 6) & 0x3F), 0x80 | (code & 0x3F)
          );
        }
      }
      return new Uint8Array(out);
    }
  };
  globalThis.TextDecoder = class TextDecoder {
    decode(bytes) {
      if (!bytes) return '';
      const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
      let out = '';
      let i = 0;
      while (i < u8.length) {
        const b = u8[i];
        if (b < 0x80) { out += String.fromCharCode(b); i += 1; }
        else if (b < 0xE0) {
          out += String.fromCharCode(((b & 0x1F) << 6) | (u8[i + 1] & 0x3F)); i += 2;
        } else if (b < 0xF0) {
          out += String.fromCharCode(((b & 0x0F) << 12) | ((u8[i + 1] & 0x3F) << 6) | (u8[i + 2] & 0x3F)); i += 3;
        } else {
          const cp = ((b & 0x07) << 18) | ((u8[i + 1] & 0x3F) << 12) | ((u8[i + 2] & 0x3F) << 6) | (u8[i + 3] & 0x3F);
          out += String.fromCodePoint(cp); i += 4;
        }
      }
      return out;
    }
  };

  // ------------------------------------------------------------------ Intl
  // Minimal Intl: format/compare primitives without full ICU. YouTube's
  // player and Material date formatting need these classes to EXIST; the
  // en-US fallbacks are byte-compatible for common cases.
  globalThis.Intl = {
    NumberFormat: class NumberFormat {
      static supportedLocalesOf(locales) { return Array.isArray(locales) ? locales.map(String) : [String(locales || 'en')]; }
      constructor(locales, opts) {
        opts = opts || {};
        this._min = opts.minimumFractionDigits !== undefined ? opts.minimumFractionDigits : 0;
        this._max = opts.maximumFractionDigits !== undefined ? opts.maximumFractionDigits : (this._min || (opts.style === 'percent' ? 0 : 2));
        this._style = opts.style || 'decimal';
        this._currency = opts.currency || 'USD';
        this._grouping = opts.useGrouping !== false;
      }
      format(n) {
        n = Number(n);
        if (!isFinite(n)) return String(n);
        if (this._style === 'percent') n = n * 100;
        let fixed = n.toFixed(this._max === undefined ? this._min : Math.max(this._min, this._max === undefined ? this._min : this._max));
        if (this._max !== undefined && this._max > this._min) {
          fixed = fixed.replace(/(\.\d*?)0+$/, '$1').replace(/\.$/, '');
        }
        let body = fixed;
        let sign = '';
        if (body.startsWith('-')) { sign = '-'; body = body.slice(1); }
        if (this._grouping) {
          const parts = body.split('.');
          parts[0] = parts[0].replace(/\B(?=(\d{3})+(?!\d))/g, ',');
          body = parts.join('.');
        }
        if (this._style === 'percent') return sign + body + '%';
        if (this._style === 'currency') return sign + (this._currency || 'USD') + ' ' + body;
        return sign + body;
      }
      resolvedOptions() { return { locale: 'en-US', minimumFractionDigits: this._min, maximumFractionDigits: this._max, useGrouping: this._grouping, style: this._style, currency: this._currency }; }
      formatToParts(n) {
        const str = this.format(n);
        return str.split('').map(function (c) { return { type: /\d/.test(c) ? 'integer' : (c === '.' ? 'decimal' : (c === ',' ? 'group' : 'literal')), value: c }; });
      }
    },
    DateTimeFormat: class DateTimeFormat {
      static supportedLocalesOf(locales) { return Array.isArray(locales) ? locales.map(String) : [String(locales || 'en')]; }
      constructor(locales, opts) { this._opts = opts || {}; }
      format(date) {
        const d = (date instanceof Date) ? date : new Date(date);
        if (!(d instanceof Date) || isNaN(d.getTime())) return String(date);
        const Y = d.getFullYear(), M = d.getMonth() + 1, D = d.getDate();
        const pad = function (x) { return (x < 10 ? '0' : '') + x; };
        if (this._opts.year !== undefined || this._opts.month !== undefined || this._opts.day !== undefined) {
          const parts = [];
          if (this._opts.year !== false) parts.push(Y);
          if (this._opts.month !== false) parts.push(pad(M));
          if (this._opts.day !== false) parts.push(pad(D));
          return parts.join('/') + (this._opts.hour !== undefined ? ' ' + pad(d.getHours()) + ':' + pad(d.getMinutes()) + ':' + pad(d.getSeconds()) : '');
        }
        return pad(M) + '/' + pad(D) + '/' + Y;
      }
      formatToParts(date) { return this.format(date).split('').map(function (c) { return { type: 'literal', value: c }; }); }
      resolvedOptions() { return { locale: 'en-US', timeZone: 'UTC' }; }
    },
    Collator: class Collator {
      static supportedLocalesOf(locales) { return Array.isArray(locales) ? locales.map(String) : [String(locales || 'en')]; }
      constructor() {}
      compare(a, b) { a = String(a); b = String(b); return a < b ? -1 : (a > b ? 1 : 0); }
      resolvedOptions() { return { locale: 'en-US', sensitivity: 'variant' }; }
    },
    getCanonicalLocales(locales) {
      return Array.isArray(locales) ? locales.map(String) : [String(locales || 'en')];
    },
  };

  // ------------------------------------------------------------------ messaging
  // MessageChannel / MessagePort / postMessage: synchronous delivery via
  // microtasks. YouTube's app shell and many libs construct these even
  // when they only use them for local signalling.
  globalThis.MessageEvent = class MessageEvent extends globalThis.Event {
    constructor(type, opts) {
      super(type, opts);
      this.data = opts && opts.data;
      this.origin = (opts && opts.origin) || '';
      this.source = opts && opts.source;
    }
  };
  class MessagePort {
    constructor(id) {
      this._id = id;
      this.onmessage = null;
      this._other = null;
      this._queue = [];
      this._started = false;
    }
    postMessage(data) {
      if (!this._other) return;
      const ev = new MessageEvent('message', { data: data });
      if (this._other._started && (this._other.onmessage || this._other._ls)) {
        queueMicrotask(function () { this._other._deliver(ev); }.bind(this));
      } else {
        this._other._queue.push(ev);
      }
    }
    _deliver(ev) {
      if (typeof this.onmessage === 'function') { try { this.onmessage(ev); } catch (e) { console.error('onmessage: ' + (e && e.message)); } }
      if (this._ls && this._ls.message) {
        for (const cb of this._ls.message.slice()) { try { cb(ev); } catch (e) { console.error('message port handler: ' + (e && e.message)); } }
      }
    }
    start() {
      this._started = true;
      const q = this._queue; this._queue = [];
      for (const ev of q) queueMicrotask(function () { this._deliver(ev); }.bind(this));
    }
    close() { this._other = null; }
    addEventListener(type, cb) {
      if (type !== 'message') return;
      const ls = (this._ls = this._ls || {});
      (ls.message = ls.message || []).push(cb);
      this._started = true;
      this.start();
    }
    removeEventListener() {}
  }
  globalThis.MessagePort = MessagePort;
  globalThis.MessageChannel = class MessageChannel {
    constructor() {
      this.port1 = new MessagePort(1);
      this.port2 = new MessagePort(2);
      this.port1._other = this.port2;
      this.port2._other = this.port1;
    }
  };
  globalThis.postMessage = function (data, origin) {
    // window.postMessage: deliver to our own window listeners.
    queueMicrotask(function () {
      const ev = new MessageEvent('message', { data: data, origin: origin || '' });
      ev.target = windowAlias;
      (winListeners.message || []).forEach(function (cb) { try { cb(ev); } catch (e) {} });
    });
  };
  globalThis.BroadcastChannel = class BroadcastChannel {
    constructor(name) { this._name = String(name); this.onmessage = null; }
    postMessage() {}
    close() {}
    addEventListener() {}
  };

  // ------------------------------------------------------------------ crypto (non-security)
  // Math.random-backed: fine for GUIDs and feature probes, NOT for keys.
  globalThis.crypto = {
    getRandomValues(array) {
      for (let i = 0; i < array.length; i++) {
        array[i] = Math.floor(Math.random() * 256);
      }
      return array;
    },
    randomUUID() {
      const hex = '0123456789abcdef';
      let uuid = '';
      for (let i = 0; i < 36; i++) {
        if (i === 8 || i === 13 || i === 18 || i === 23) uuid += '-';
        else if (i === 14) uuid += '4';
        else uuid += hex[Math.floor(Math.random() * 16)];
      }
      return uuid;
    },
    subtle: new Proxy({}, { get() { return function () { return Promise.resolve({}); }; } }),
  };

  // ------------------------------------------------------------------ URL
  // Minimal URL (protocol//host/path?search#hash) — enough for link munging.
  globalThis.URL = class URL {
    constructor(input, base) {
      let str = String(input);
      if (base !== undefined && !/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(str)) {
        str = String(base).replace(/\/[^/]*$/, '') + (str.startsWith('/') ? '' : '/') + str;
      }
      const m = /^([a-zA-Z][a-zA-Z0-9+.-]*:\/\/)?([^/?#]*)?([^?#]*)(\?[^#]*)?(#.*)?$/.exec(str);
      this.protocol = (m[1] || '').replace('://', '').toLowerCase();
      const hostPart = m[2] || '';
      this.host = hostPart;
      this.hostname = hostPart.split(':')[0];
      this.port = hostPart.includes(':') ? hostPart.split(':')[1] : '';
      this.pathname = m[3] || '/';
      this.search = m[4] || '';
      this.hash = m[5] || '';
      this.searchParams = new URLSearchParams(this.search);
      Object.defineProperty(this, 'href', {
        get: () => this.toString(),
        set: (v) => { __native_navigate(String(v)); },
      });
    }
    toString() {
      return (this.protocol ? this.protocol + '://' : '') + this.host + this.pathname + this.search + this.hash;
    }
    static createObjectURL(obj) {
      if (obj && obj._id !== undefined && obj instanceof MediaSource) {
        return 'rowser-mse:' + obj._id;
      }
      return '';
    }
    static revokeObjectURL() {}
  };
  globalThis.URLSearchParams = class URLSearchParams {
    constructor(init) {
      this._pairs = [];
      if (typeof init === 'string') {
        const q = init.startsWith('?') ? init.slice(1) : init;
        for (const part of q.split('&')) {
          if (!part) continue;
          const eq = part.indexOf('=');
          const k = decodeURIComponent(eq < 0 ? part : part.slice(0, eq));
          const v = eq < 0 ? '' : decodeURIComponent(part.slice(eq + 1));
          this._pairs.push([k, v]);
        }
      } else if (init && typeof init.forEach === 'function') {
        init.forEach((v, k) => this._pairs.push([String(k), String(v)]));
      }
    }
    get(k) { const p = this._pairs.find((x) => x[0] === k); return p ? p[1] : null; }
    getAll(k) { return this._pairs.filter((x) => x[0] === k).map((x) => x[1]); }
    has(k) { return this._pairs.some((x) => x[0] === k); }
    set(k, v) { this._pairs = this._pairs.filter((x) => x[0] !== k); this._pairs.push([k, String(v)]); }
    append(k, v) { this._pairs.push([k, String(v)]); }
    delete(k) { this._pairs = this._pairs.filter((x) => x[0] !== k); }
    toString() { return this._pairs.map((p) => encodeURIComponent(p[0]) + '=' + encodeURIComponent(p[1])).join('&'); }
    forEach(cb) { this._pairs.forEach((p) => cb(p[1], p[0])); }
  };

  // ------------------------------------------------------------------ abort + idle
  globalThis.AbortSignal = class AbortSignal {
    constructor() { this.aborted = false; this.reason = undefined; this.onabort = null; }
    addEventListener(type, cb) { if (type === 'abort') { this._cb = cb; } }
    removeEventListener() { this._cb = null; }
    _abort(reason) {
      if (this.aborted) return;
      this.aborted = true;
      this.reason = reason;
      if (typeof this.onabort === 'function') { try { this.onabort({ type: 'abort' }); } catch (e) {} }
      if (this._cb) { try { this._cb({ type: 'abort' }); } catch (e) {} }
    }
    static timeout() { return new AbortSignal(); }
  };
  globalThis.AbortController = class AbortController {
    constructor() { this.signal = new AbortSignal(); }
    abort(reason) { this.signal._abort(reason); }
  };
  globalThis.requestIdleCallback = function (cb) {
    return setTimeout(function () { cb({ didTimeout: false, timeRemaining() { return 50; } }); }, 1);
  };
  globalThis.cancelIdleCallback = function (id) { clearTimeout(id); };
  globalThis.structuredClone = function (value) {
    return JSON.parse(JSON.stringify(value === undefined ? null : value));
  };
  globalThis.__elementFromHandle = function (h) { return wrapElement(h); };

  // Window-level listeners + event dispatch (from the engine/UI).
  const winListeners = {};
  globalThis.addEventListener = function (type, cb) {
    type = String(type);
    (winListeners[type] = winListeners[type] || []).push(cb);
  };
  globalThis.removeEventListener = function (type, cb) {
    const list = winListeners[type] || [];
    const i = list.indexOf(cb);
    if (i >= 0) list.splice(i, 1);
  };
  globalThis.dispatchEvent = function (ev) {
    (winListeners[ev && ev.type] || []).forEach(function (cb) { try { cb(ev); } catch (e) {} });
    return true;
  };

  // Element-level listener fan-out with wrapper identity: listeners live
  // on the persistent wrapper, so `el.addEventListener` now actually fires.
  function fireAt(handle, type, ev) {
    const w = wraps.get(handle);
    if (w && w._ls && w._ls[type]) {
      const list = w._ls[type].slice();
      for (const cb of list) {
        try { ev.currentTarget = w; cb(ev); } catch (e) { console.error(type + ' handler: ' + errString(e)); }
      }
    }
  }

  globalThis.__onDomEvent = function (handle, type) {
    const el = wrapElement(handle);
    const ev = new Event(type);
    ev.target = el;
    // Bubble: element → ancestors → document → window (delegation-safe).
    let h = handle;
    let guard = 0;
    while (h && guard++ < 300) {
      fireAt(h, type, ev);
      const next = __native_dom_parentNode(h);
      if (!next) {
        // Shadow boundary: hop from the shadow root to its host.
        const host = __native_dom_shadowHost(h);
        if (host) { h = host; continue; }
        break;
      }
      h = next;
    }
    const docLs = document._ls && document._ls[type];
    if (docLs) {
      for (const cb of docLs.slice()) { try { ev.currentTarget = document; cb(ev); } catch (e) { console.error(type + ' doc handler: ' + errString(e)); } }
    }
    (winListeners[type] || []).forEach(function (cb) { try { ev.currentTarget = windowAlias; cb(ev); } catch (e) { console.error(type + ' window handler: ' + errString(e)); } });
  };

  // Lifecycle events, fired by the engine once scripts have run.
  globalThis.__fireDocumentEvent = function (type) {
    const ev = new Event(type);
    ev.target = document;
    (winListeners[type] || []).forEach(function (cb) { try { cb(ev); } catch (e) { __native_console('error', 'DOMContentLoaded handler: ' + errString(e)); } });
    const docLs = document._ls && document._ls[type];
    if (docLs) { try { docLs(ev); } catch (e) { __native_console('error', 'DOMContentLoaded doc handler: ' + (e && e.message)); } }
  };

  // ------------------------------------------------------------------ media events + MSE
  // Maps a tag name to its registered htmlClasses key (video ->
  // HTMLVideoElement).
  function tagClass(tag) {
    switch (tag) {
      case 'video': return 'HTMLVideoElement';
      case 'audio': return 'HTMLAudioElement';
      case 'img': return 'HTMLImageElement';
      case 'input': return 'HTMLInputElement';
      case 'button': return 'HTMLButtonElement';
      case 'form': return 'HTMLFormElement';
      case 'script': return 'HTMLScriptElement';
      case 'style': return 'HTMLStyleElement';
      case 'link': return 'HTMLLinkElement';
      case 'canvas': return 'HTMLCanvasElement';
      case 'iframe': return 'HTMLIFrameElement';
      case 'template': return 'HTMLTemplateElement';
      case 'textarea': return 'HTMLTextAreaElement';
      case 'select': return 'HTMLSelectElement';
      case 'option': return 'HTMLOptionElement';
      case 'optgroup': return 'HTMLOptGroupElement';
      case 'label': return 'HTMLLabelElement';
      case 'fieldset': return 'HTMLFieldSetElement';
      case 'legend': return 'HTMLLegendElement';
      case 'table': return 'HTMLTableElement';
      case 'tbody': case 'thead': case 'tfoot': return 'HTMLTableSectionElement';
      case 'tr': return 'HTMLTableRowElement';
      case 'td': case 'th': return 'HTMLTableCellElement';
      case 'caption': return 'HTMLTableCaptionElement';
      case 'col': case 'colgroup': return 'HTMLTableColElement';
      case 'title': return 'HTMLTitleElement';
      case 'meta': return 'HTMLMetaElement';
      case 'base': return 'HTMLBaseElement';
      case 'area': return 'HTMLAreaElement';
      case 'map': return 'HTMLMapElement';
      case 'blockquote': case 'q': return 'HTMLQuoteElement';
      case 'pre': return 'HTMLPreElement';
      case 'br': return 'HTMLBRElement';
      case 'hr': return 'HTMLHRElement';
      case 'ins': case 'del': return 'HTMLModElement';
      case 'picture': return 'HTMLPictureElement';
      case 'source': return 'HTMLSourceElement';
      case 'track': return 'HTMLTrackElement';
      case 'param': return 'HTMLParamElement';
      case 'embed': return 'HTMLEmbedElement';
      case 'object': return 'HTMLObjectElement';
      case 'details': return 'HTMLDetailsElement';
      case 'summary': return 'HTMLSummaryElement';
      case 'dialog': return 'HTMLDialogElement';
      case 'datalist': return 'HTMLDataListElement';
      case 'output': return 'HTMLOutputElement';
      case 'progress': return 'HTMLProgressElement';
      case 'meter': return 'HTMLMeterElement';
      case 'marquee': return 'HTMLMarqueeElement';
      default: return 'HTMLElement';
    }
  }
  // htmlClasses is defined below; wrapElement is called after it exists.
  function wrapElement(handle) {
    if (handle === null || handle === undefined || handle === 0) return null;
    const existing = wraps.get(handle);
    if (existing) return existing;
    const tag = __native_dom_tagName(handle);
    const map = __htmlClassesRef || {};
    const cls = map[tagClass(tag)] || Element;
    return new cls(handle);
  }

  // Media element events: (node, type, detailJson).
  globalThis.__onMediaEvent = function (handle, type, detailJson) {
    const el = wrapElement(handle);
    // Refresh the element's state mirror before handlers run.
    if (el._mirror) { try { el._mirror(); } catch (e) {} }
    let detail = {};
    try { detail = JSON.parse(detailJson || '{}'); } catch (e) {}
    const ev = new Event(type);
    ev.target = el;
    if (detail.time !== undefined) { ev.timeStamp = detail.time; }
    const ls = el._ls && el._ls[type];
    if (ls) { try { ls.call(el, ev); } catch (e) {} }
    const onprop = el['on' + type];
    if (typeof onprop === 'function') { try { onprop.call(el, ev); } catch (e) {} }
  };

  // MediaSource / SourceBuffer (MSE): bytes pushed by page JS are routed to
  // the engine media pipeline through native commands.
  const mediaSources = new Map();
  const sourceBuffers = new Map();
  let nextSbId = 1;
  class SourceBuffer {
    constructor(msId, mime) {
      this._msId = msId;
      this._id = nextSbId++;
      this.mime = String(mime);
      this.updating = false;
      this.mode = 'segments';
      this.timestampOffset = 0;
      this.appendWindowStart = 0;
      this.appendWindowEnd = Infinity;
      this._ls = {};
      sourceBuffers.set(this._id, this);
      __native_mse_add_source_buffer(msId, this._id, this.mime);
    }
    // Accepts ArrayBuffer / TypedArray / ArrayBufferView; views are copied
    // to an exact ArrayBuffer so the bridge sees precise bytes.
    appendBuffer(data) {
      let ab;
      if (data instanceof ArrayBuffer) {
        ab = data;
      } else if (ArrayBuffer.isView(data)) {
        ab = data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength);
      } else {
        ab = new ArrayBuffer(0);
      }
      this.updating = true;
      const sb = this;
      __native_mse_append(this._id, ab);
      // The engine processes the append synchronously on the page thread;
      // settle `updating` on the next microtask (hls.js waits on this).
      Promise.resolve().then(function () {
        sb.updating = false;
        sb._fire('update');
        sb._fire('updateend');
      });
    }
    abort() { this.updating = false; }
    remove() {}
    _fire(type) {
      const ls = this._ls[type] || [];
      for (const cb of ls) { try { cb.call(this, { type: type, target: this }); } catch (e) {} }
    }
    addEventListener(t, cb) { (this._ls[t] = this._ls[t] || []).push(cb); }
    removeEventListener(t, cb) {
      const l = this._ls[t] || [];
      const i = l.indexOf(cb);
      if (i >= 0) l.splice(i, 1);
    }
    get buffered() {
      const ranges = this._bufferedRanges || [];
      return {
        length: ranges.length,
        start(i) { return ranges[i] ? ranges[i][0] : 0; },
        end(i) { return ranges[i] ? ranges[i][1] : 0; },
      };
    }
  }
  class MediaSource {
    constructor() {
      this._id = __native_mse_create();
      this.sourceBuffers = [];
      this.activeSourceBuffers = [];
      this.readyState = 'closed';
      this.duration = NaN;
      this._ls = {};
      mediaSources.set(this._id, this);
    }
    addSourceBuffer(mime) {
      const sb = new SourceBuffer(this._id, mime);
      this.sourceBuffers.push(sb);
      this.activeSourceBuffers.push(sb);
      return sb;
    }
    endOfStream() {
      this.readyState = 'ended';
      __native_mse_end_of_stream(this._id);
    }
    setLiveSeekableRange() {}
    clearLiveSeekableRange() {}
    addEventListener(t, cb) { (this._ls[t] = this._ls[t] || []).push(cb); }
    removeEventListener(t, cb) {
      const l = this._ls[t] || [];
      const i = l.indexOf(cb);
      if (i >= 0) l.splice(i, 1);
    }
    _fire(type) {
      const ls = this._ls[type] || [];
      for (const cb of ls) { try { cb.call(this, { type: type, target: this }); } catch (e) {} }
    }
    static isTypeSupported(t) {
      const s = String(t).toLowerCase();
      return s.includes('avc1') || s.includes('avc3') || s.includes('mp4a') || s.includes('aac');
    }
  }
  globalThis.MediaSource = MediaSource;
  globalThis.SourceBuffer = SourceBuffer;
  globalThis.__onMediaSourceEvent = function (msId, sbId, type) {
    if (sbId !== null && sbId !== undefined) {
      const sb = sourceBuffers.get(sbId);
      if (sb) { sb.updating = false; sb._fire(type); }
      return;
    }
    const ms = mediaSources.get(msId);
    if (!ms) return;
    if (type === 'sourceopen') ms.readyState = 'open';
    if (type === 'sourceclose') ms.readyState = 'closed';
    ms._fire(type);
  };
  globalThis.__native_mse_end_of_stream = globalThis.__native_mse_end_of_stream || function () {};

  // ------------------------------------------------------------------ WebSocket
  const sockets = new Map();
  class WebSocket {
    constructor(url) {
      this._id = __native_ws_open(String(url));
      this.readyState = 0;
      this.url = String(url);
      this._ls = {};
      sockets.set(this._id, this);
    }
    send(data) { __native_ws_send(this._id, String(data)); }
    close() { this.readyState = 3; __native_ws_close(this._id); sockets.delete(this._id); }
    addEventListener(type, cb) { this._ls[type] = cb; }
    set onopen(cb) { this._ls.open = cb; }
    set onmessage(cb) { this._ls.message = cb; }
    set onclose(cb) { this._ls.close = cb; }
    set onerror(cb) { this._ls.error = cb; }
  }
  globalThis.WebSocket = WebSocket;
  globalThis.__onWsEvent = function (id, type, data) {
    const ws = sockets.get(id);
    if (!ws) return;
    if (type === 'open') ws.readyState = 1;
    if (type === 'close') ws.readyState = 3;
    const cb = ws._ls[type];
    if (cb) { try { cb({ type: type, data: data }); } catch (e) {} }
  };

  // ------------------------------------------------------------------ Workers
  const workers = new Map();
  class Worker {
    constructor(url) {
      this._id = __native_worker_spawn(String(url));
      this._ls = {};
      workers.set(this._id, this);
    }
    postMessage(msg) { __native_worker_post(this._id, JSON.stringify(msg)); }
    terminate() { __native_worker_terminate(this._id); workers.delete(this._id); }
    set onmessage(cb) { this._ls.message = cb; }
    addEventListener(type, cb) { this._ls[type] = cb; }
  }
  globalThis.Worker = Worker;
  globalThis.__onWorkerMessage = function (id, json) {
    const w = workers.get(id);
    if (!w) return;
    const cb = w._ls.message;
    if (cb) { try { cb({ data: JSON.parse(json) }); } catch (e) {} }
  };

  // ------------------------------------------------------------------ environment
  const env = JSON.parse(__native_env_info());
  const navigator = {
    sendBeacon() { return true; },
    userAgent: env.userAgent,
    appVersion: '5.0 (Rrowser)',
    platform: env.platform,
    language: env.language,
    languages: [env.language],
    hardwareConcurrency: env.hardwareConcurrency,
    deviceMemory: env.deviceMemory,
    cookieEnabled: false,
    onLine: true,
    plugins: { length: 0 },
    mimeTypes: { length: 0 },
    vendor: 'Rrowser',
    sendBeacon() { return true; },
    javaEnabled() { return false; },
  };
  Object.defineProperty(globalThis, 'navigator', { value: navigator, configurable: false, writable: false });
  globalThis.screen = {
    width: env.screen.width,
    height: env.screen.height,
    availWidth: env.screen.width,
    availHeight: env.screen.height,
    colorDepth: env.screen.colorDepth,
    pixelDepth: env.screen.colorDepth,
  };
  globalThis.devicePixelRatio = 1;
  // `location` is settable: assigning href (or calling assign/replace)
  // navigates the tab via the engine.
  globalThis.location = {
    get href() { return env.location; },
    set href(url) { __native_navigate(String(url)); },
    origin: env.origin,
    protocol: env.protocol,
    host: env.host,
    hostname: env.host,
    pathname: env.pathname,
    get search() { return ''; },
    get hash() { return ''; },
    assign(url) { __native_navigate(String(url)); },
    replace(url) { __native_navigate(String(url)); },
    reload() { __native_navigate(String(env.location)); },
    toString() { return env.location; },
  };
  globalThis.origin = env.origin;
  globalThis.history = {
    length: 1,
    back() {},
    forward() {},
    go() {},
    pushState() {},
    replaceState() {},
  };

  // ------------------------------------------------------------------ misc
  globalThis.btoa = function (s) { return __native_b64_encode(String(s)); };
  globalThis.atob = function (s) { return __native_b64_decode(String(s)); };
  globalThis.performance = {
    now() { return Date.now(); },
    timeOrigin: Date.now(),
    // Navigation Timing (legacy field set; probed by many loaders).
    timing: {
      navigationStart: Date.now() - 500,
      fetchStart: Date.now() - 400,
      domainLookupStart: Date.now() - 390,
      domainLookupEnd: Date.now() - 380,
      connectStart: Date.now() - 370,
      connectEnd: Date.now() - 300,
      requestStart: Date.now() - 290,
      responseStart: Date.now() - 100,
      responseEnd: Date.now() - 50,
      domLoading: Date.now() - 40,
      domInteractive: Date.now() - 20,
      domContentLoadedEventStart: Date.now() - 10,
      domContentLoadedEventEnd: Date.now() - 5,
      domComplete: Date.now(),
      loadEventStart: Date.now(),
      loadEventEnd: Date.now(),
    },
    getEntriesByType() { return []; },
    getEntriesByName() { return []; },
    getEntries() { return []; },
    mark() {},
    measure() {},
  };
  globalThis.alert = globalThis.confirm = globalThis.prompt = function () {};
  globalThis.requestAnimationFrame = function (cb) { return setTimeout(cb, 16); };
  globalThis.cancelAnimationFrame = function (id) { clearTimeout(id); };
  globalThis.matchMedia = function (q) {
    return { matches: false, media: String(q), addEventListener() {}, removeEventListener() {} };
  };
})();
