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
    createElement(tag) { return new Element(__native_dom_createElement(String(tag))); }
    createTextNode(text) { return new Element(__native_dom_createTextNode(String(text))); }
    get body() { const h = __native_dom_body(); return h === null ? null : new Element(h); }
    get documentElement() { const h = __native_dom_html(); return h === null ? null : new Element(h); }
    get title() { return __native_document_title(); }
    get readyState() { return 'complete'; }
    addEventListener(type, cb) { __native_document_addEventListener(String(type)); (this._ls = this._ls || {})[type] = cb; }
    removeEventListener(type) { if (this._ls) delete this._ls[type]; }
  }
  globalThis.document = new Document();
  globalThis.Element = Element;
  globalThis.Node = Element;
  globalThis.Text = Element;
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
  globalThis.location = {
    href: env.location,
    origin: env.origin,
    protocol: env.protocol,
    host: env.host,
    pathname: env.pathname,
    toString() { return env.location; },
  };
  globalThis.history = { length: 1, back() {}, forward() {}, go() {} };

  // ------------------------------------------------------------------ misc
  globalThis.btoa = function (s) { return __native_b64_encode(String(s)); };
  globalThis.atob = function (s) { return __native_b64_decode(String(s)); };
  globalThis.performance = {
    now() { return Date.now(); },
    timeOrigin: Date.now(),
  };
  globalThis.alert = globalThis.confirm = globalThis.prompt = function () {};
  globalThis.requestAnimationFrame = function (cb) { return setTimeout(cb, 16); };
  globalThis.cancelAnimationFrame = function (id) { clearTimeout(id); };
  globalThis.matchMedia = function (q) {
    return { matches: false, media: String(q), addEventListener() {}, removeEventListener() {} };
  };
})();
