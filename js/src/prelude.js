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
  globalThis.scrollY = 0;
  globalThis.scrollX = 0;
  globalThis.pageYOffset = 0;
  globalThis.pageXOffset = 0;
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
    try { t.cb.apply(null, t.args); } catch (e) { console.error(String(e)); }
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
      const text = await this.text();
      const buf = new ArrayBuffer(text.length);
      new Uint8Array(buf).set(text.split('').map(c => c.charCodeAt(0)));
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
  class Element {
    constructor(handle) { this._h = handle; }
    getAttribute(n) { const v = __native_dom_getAttr(this._h, String(n)); return (v === null || v === undefined) ? null : v; }
    setAttribute(n, v) { __native_dom_setAttr(this._h, String(n), String(v)); }
    removeAttribute(n) { __native_dom_removeAttr(this._h, String(n)); }
    hasAttribute(n) { return this.getAttribute(n) !== null; }
    appendChild(child) { if (child && child._h) __native_dom_appendChild(this._h, child._h); return child; }
    removeChild(child) { if (child && child._h) __native_dom_removeChild(this._h, child._h); return child; }
    get parentNode() { return null; }
    get textContent() { return __native_dom_textContent(this._h); }
    set textContent(t) { __native_dom_setTextContent(this._h, String(t)); }
    get style() {
      const self = this;
      return new Proxy({}, {
        get(_, prop) { return __native_dom_style_get(self._h, String(prop)); },
        set(_, prop, v) { __native_dom_style_set(self._h, String(prop), String(v)); return true; },
      });
    }
    addEventListener(type, cb) {
      __native_dom_addEventListener(this._h, String(type));
      (this._ls = this._ls || {})[type] = cb;
    }
    removeEventListener(type) { if (this._ls) delete this._ls[type]; }
    click() { __native_dom_click(this._h); }
    focus() {}
    blur() {}
    get id() { return this.getAttribute('id') || ''; }
    set id(v) { this.setAttribute('id', v); }
    get className() { return this.getAttribute('class') || ''; }
    set className(v) { this.setAttribute('class', v); }
    get innerHTML() { return ''; }
    get children() { return []; }
    get firstChild() { return null; }
  }

  class Document {
    getElementById(id) { const h = __native_dom_getElementById(String(id)); return (h === null || h === undefined || h === 0) ? null : new Element(h); }
    querySelector(sel) { const h = __native_dom_querySelector(String(sel)); return (h === null || h === undefined) ? null : new Element(h); }
    querySelectorAll(sel) {
      const handles = JSON.parse(__native_dom_querySelectorAll(String(sel)) || '[]');
      return handles.map(function (h) { return new Element(h); });
    }
    getElementsByTagName(tag) { return this.querySelectorAll(String(tag)); }
    getElementsByClassName(cls) { return this.querySelectorAll('.' + String(cls)); }
    getElementsByName() { return []; }
    createElement(tag) { return new Element(__native_dom_createElement(String(tag))); }
    createTextNode(text) { return new Element(__native_dom_createTextNode(String(text))); }
    createDocumentFragment() { return new Element(__native_dom_createElement('fragment')); }
    get body() { const h = __native_dom_body(); return h === null ? null : new Element(h); }
    get head() { const h = __native_dom_querySelector('head'); return h === null ? null : new Element(h); }
    get documentElement() { const h = __native_dom_html(); return h === null ? null : new Element(h); }
    get title() { return __native_document_title(); }
    set title(t) { /* engine-managed; JS title sets are a UI nicety for later */ }
    get readyState() { return 'complete'; }
    get visibilityState() { return 'visible'; }
    get hidden() { return false; }
    get currentScript() { return null; }
    get cookie() { return ''; }
    set cookie(value) { /* engine cookie-jar integration is a documented gap */ }
    get timeline() { return { currentTime: 0, play() {} }; }
    getAnimations() { return []; }
    addEventListener(type, cb) { __native_document_addEventListener(String(type)); (this._ls = this._ls || {})[type] = cb; }
    removeEventListener(type) { if (this._ls) delete this._ls[type]; }
  }
  globalThis.document = new Document();
  globalThis.Element = Element;
  globalThis.Node = Element;
  globalThis.Text = Element;

  // ------------------------------------------------------------------ DOM classes
  // `class X extends HTMLElement` is ubiquitous; without the HTML* element
  // classes every modern framework's class registration throws.
  const HTMLElementBase = class HTMLElement extends Element {};
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
      get content() { return this; }
    },
    HTMLVideoElement: class HTMLVideoElement extends HTMLElementBase {},
    HTMLAudioElement: class HTMLAudioElement extends HTMLElementBase {},
    HTMLCanvasElement: class HTMLCanvasElement extends HTMLElementBase {},
    HTMLIFrameElement: class HTMLIFrameElement extends HTMLElementBase {},
    HTMLUnknownElement: class HTMLUnknownElement extends HTMLElementBase {},
    SVGSVGElement: class SVGSVGElement extends Element {},
    SVGElement: class SVGElement extends Element {},
  };
  for (const name of Object.keys(htmlClasses)) globalThis[name] = htmlClasses[name];

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

  // WebComponents: registry + observers. These are *stubs* — custom elements
  // register, scripts continue, upgrade callbacks are not synthesized.
  const registry = new Map();
  globalThis.customElements = {
    define(name, ctor) { registry.set(String(name).toLowerCase(), ctor); },
    get(name) { return registry.get(String(name).toLowerCase()); },
    whenDefined(name) { return Promise.resolve(); },
    upgrade() {},
  };
  globalThis.MutationObserver = class MutationObserver {
    constructor(cb) { this._cb = cb; }
    observe() {}
    disconnect() {}
    takeRecords() { return []; }
  };
  globalThis.IntersectionObserver = class IntersectionObserver {
    constructor(cb) { this._cb = cb; }
    observe() {}
    unobserve() {}
    disconnect() {}
    takeRecords() { return []; }
  };
  globalThis.ResizeObserver = class ResizeObserver {
    constructor(cb) { this._cb = cb; }
    observe() {}
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
    return { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0 };
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
  globalThis.TreeWalker = class TreeWalker {
    constructor(root) { this.root = root; this.currentNode = root; }
    nextNode() { return null; }
    previousNode() { return null; }
    firstChild() { return null; }
    parentNode() { return null; }
  };
  globalThis.NodeIterator = class NodeIterator {
    constructor(root) { this.root = root; }
    nextNode() { return null; }
    previousNode() { return null; }
  };
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
    static createObjectURL() { return ''; }
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
  globalThis.__elementFromHandle = function (h) { return new Element(h); };

  // Window-level listeners + event dispatch (from the engine/UI).
  const winListeners = {};
  globalThis.addEventListener = function (type, cb) {
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
  globalThis.__onDomEvent = function (handle, type) {
    const el = new Element(handle);
    (winListeners[type] || []).forEach(function (cb) { try { cb({ type: type, target: el }); } catch (e) {} });
    const list = el._ls && el._ls[type];
    if (list) { try { list({ type: type, target: el }); } catch (e) {} }
    const docLs = document._ls && document._ls[type];
    if (docLs) { try { docLs({ type: type, target: el }); } catch (e) {} }
  };

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
