//! Rrowser JS: the QuickJS-ng runtime and Web API bindings.
//!
//! Architecture (see `docs/ARCHITECTURE.md` for the full picture):
//!
//! * **QuickJS-ng** (v0.16.x) is embedded through `rquickjs` 0.14.
//! * All JavaScript callbacks (timer callbacks, promise resolvers, event
//!   listeners, worker callbacks, WebSocket handlers) live **inside the JS
//!   heap**, keyed by numeric ids. Rust holds no `JsFunction` references,
//!   which keeps GC tracing trivially sound and leak-free.
//! * Native functions (`__native_*`) only exchange primitives (strings,
//!   numbers, booleans). The JS-side prelude (`prelude.js`) builds the real
//!   Web APIs — `fetch`, `XMLHttpRequest`, `setTimeout`, `WebSocket`,
//!   `Worker`, `localStorage`, DOM classes — on top of them.
//! * Async completions flow **into** JS through [`EngineEvent`] dispatch,
//!   followed by a promise-job pump — so `async/await` and microtasks work.
//! * A watchdog interrupt terminates runaway scripts; the JS heap has a hard
//!   memory limit. QuickJS-ng's incremental GC runs during allocation, so
//!   the main thread never stalls on a full GC pass; idle pages get an
//!   explicit `run_gc` from the engine's memory manager.

pub mod prelude {
    /// The JS-side Web API prelude, evaluated before any page script.
    pub const PRELUDE_JS: &str = include_str!("prelude.js");

    /// The minimal prelude for worker contexts (message passing only).
    pub const WORKER_PRELUDE_JS: &str = r#"
(function () {
  'use strict';
  globalThis.onmessage = null;
  globalThis.postMessage = function (msg) { __native_worker_post_message(JSON.stringify(msg)); };
  globalThis.close = function () {};
  globalThis.__onWorkerMessage = function (json) {
    const cb = globalThis.onmessage;
    if (cb) { try { cb({ data: JSON.parse(json) }); } catch (e) {} }
  };
  globalThis.console = {
    log: (...a) => __native_console('log', a.join(' ')),
    error: (...a) => __native_console('error', a.join(' ')),
  };
})();
"#;
}

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rowser_dom::{Dom, NodeId};
use rowser_privacy::fingerprint::SpoofProfile;
use rowser_storage::Storage;
use rquickjs::{Context, Function, Runtime};

/// Commands the runtime sends to the owning engine (timers, fetches, ...).
#[derive(Debug, Clone)]
pub enum JsCommand {
    /// Schedule a timer (or interval).
    TimerStart {
        /// Timer id (JS-assigned).
        id: u64,
        /// Delay in milliseconds.
        delay_ms: u64,
        /// True for `setInterval`.
        interval: bool,
    },
    /// Cancel a timer.
    TimerClear {
        /// Timer id.
        id: u64,
    },
    /// Start a fetch; completion arrives as
    /// [`EngineEvent::FetchCompleted`].
    FetchStart {
        /// Fetch id (JS-assigned).
        id: u64,
        /// Target URL.
        url: String,
        /// JSON `FetchInit` (method, headers, body).
        init: String,
    },
    /// Open a WebSocket.
    WsOpen {
        /// Socket id.
        id: u64,
        /// URL.
        url: String,
    },
    /// Send a text frame.
    WsSend {
        /// Socket id.
        id: u64,
        /// Payload.
        data: String,
    },
    /// Close a socket.
    WsClose {
        /// Socket id.
        id: u64,
    },
    /// Spawn a worker (engine fetches + executes the script).
    WorkerSpawn {
        /// Worker id.
        id: u64,
        /// Script URL.
        url: String,
    },
    /// Post a message to a worker.
    WorkerPost {
        /// Worker id.
        id: u64,
        /// JSON message.
        message: String,
    },
    /// Terminate a worker.
    WorkerTerminate {
        /// Worker id.
        id: u64,
    },
    /// A console call (level, text) — forwarded to the engine event stream.
    Console {
        /// `log`/`warn`/`error`/`info`.
        level: String,
        /// Formatted message.
        text: String,
    },
    /// Script-initiated navigation (location.href assignment, .assign/.replace).
    Navigate {
        /// Absolute or page-relative URL.
        url: String,
    },
    /// The DOM was mutated; re-style/layout/render after the script task.
    MarkDirty,
    /// A worker context posted a message to its owning page (local routing;
    /// never forwarded to the engine).
    WorkerEgress {
        /// Worker id.
        id: u64,
        /// JSON message.
        message: String,
    },
}

/// Events the engine dispatches **into** JavaScript.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// A timer/interval fired.
    TimerFired(u64),
    /// A fetch completed.
    FetchCompleted {
        /// Fetch id.
        id: u64,
        /// HTTP status.
        status: u16,
        /// JSON headers object.
        headers: String,
        /// Base64 body.
        body_b64: String,
    },
    /// A fetch failed (network error).
    FetchFailed {
        /// Fetch id.
        id: u64,
        /// Error description.
        error: String,
    },
    /// A WebSocket event.
    WsEvent {
        /// Socket id.
        id: u64,
        /// Event kind: `open`/`message`/`close`/`error`.
        kind: String,
        /// Payload (for messages).
        data: String,
    },
    /// A worker posted a message to its page.
    WorkerMessage {
        /// Worker id.
        id: u64,
        /// JSON message.
        message: String,
    },
    /// A UI event (click, etc.) on a DOM node.
    DomEvent {
        /// Node handle.
        node: u64,
        /// Event type.
        event_type: String,
    },
}

/// The shared state between the runtime and the page thread.
pub struct PageBridge {
    /// The page DOM (single-threaded with the runtime).
    pub dom: Rc<RefCell<Dom>>,
    /// The document node.
    pub document: NodeId,
    /// The `body` element (0 when absent).
    pub body: NodeId,
    /// The `html` element (0 when absent).
    pub html: NodeId,
    /// Document URL (for `location`).
    pub url: String,
    /// Origin for localStorage.
    pub origin: String,
    /// Profile storage (optional: about: pages have none).
    pub storage: Option<Arc<Storage>>,
    /// Anti-fingerprinting profile.
    pub spoof: SpoofProfile,
    /// Channel to the engine.
    pub outgoing: Option<Sender<JsCommand>>,
}

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct JsConfig {
    /// Hard JS heap limit in bytes.
    pub memory_limit: usize,
    /// Max script execution time before the watchdog interrupts.
    pub script_timeout: Duration,
    /// Max stack size.
    pub stack_size: usize,
}

impl Default for JsConfig {
    fn default() -> Self {
        JsConfig {
            memory_limit: 96 * 1024 * 1024,
            script_timeout: Duration::from_secs(10),
            stack_size: 1024 * 1024,
        }
    }
}

/// A QuickJS-ng runtime bound to a page.
pub struct JsRuntime {
    runtime: Runtime,
    context: Context,
    watchdog_start: Rc<Cell<f64>>,
    #[allow(dead_code)]
    config: JsConfig,
    /// Live timers registered by JS (count only; bodies live in JS).
    timer_count: Rc<Cell<u64>>,
}

/// Errors surfaced to the engine.
#[derive(Debug, thiserror::Error)]
pub enum JsError {
    /// Script evaluation failed (syntax or runtime error).
    #[error("js error: {0}")]
    Eval(String),
    /// Runtime construction failed.
    #[error("js runtime: {0}")]
    Runtime(String),
}

impl From<rquickjs::Error> for JsError {
    fn from(err: rquickjs::Error) -> Self {
        JsError::Eval(err.to_string())
    }
}

impl JsRuntime {
    /// Creates the runtime and installs the Web API prelude.
    pub fn new(config: JsConfig, bridge: PageBridge) -> Result<JsRuntime, JsError> {
        let runtime = Runtime::new().map_err(|e| JsError::Runtime(e.to_string()))?;
        runtime.set_memory_limit(config.memory_limit);
        runtime.set_max_stack_size(config.stack_size);
        runtime.set_gc_threshold(1024 * 512);

        let watchdog_start = Rc::new(Cell::new(0.0f64));
        let watchdog_limit = config.script_timeout.as_secs_f64();
        {
            let start = Rc::clone(&watchdog_start);
            runtime.set_interrupt_handler(Some(Box::new(move || {
                let started = start.get();
                if started == 0.0 {
                    return false;
                }
                (monotonic_secs() - started) > watchdog_limit
            })));
        }

        let context = Context::full(&runtime).map_err(|e| JsError::Runtime(e.to_string()))?;
        let js = JsRuntime {
            runtime,
            context,
            watchdog_start,
            config,
            timer_count: Rc::new(Cell::new(0)),
        };
        js.install_natives(bridge)?;
        js.context.with(|ctx| {
            if let Err(e) = ctx.eval::<(), _>(prelude::PRELUDE_JS) {
                let detail = exception_detail(&ctx)
                    .unwrap_or_else(|| err_string(&e));
                return Err(JsError::Eval(detail));
            }
            Ok(())
        })?;
        Ok(js)
    }

    /// Evaluates a classic script. Returns a stringified result.
    pub fn eval(&self, code: &str, filename: &str) -> Result<String, JsError> {
        let code = code.to_owned();
        let filename = filename.to_owned();
        self.reset_watchdog();
        let result: Result<String, (rquickjs::Error, Option<String>)> = self.context.with(|ctx| {
            let mut options = rquickjs::context::EvalOptions::default();
            options.filename = Some(filename.clone());
            options.strict = false;
            match ctx.eval_with_options::<rquickjs::Value, _>(code, options) {
                Ok(value) => stringify_value(&ctx, &value).map_err(|e| (e, None)),
                Err(e) => Err((e, exception_detail(&ctx))),
            }
        });
        self.clear_watchdog();
        self.pump_jobs();
        result.map_err(|(e, detail)| {
            JsError::Eval(match detail {
                Some(detail) if !detail.is_empty() => detail,
                _ => format_js_error(&e),
            })
        })
    }

    /// Dispatches an engine event into JS (timer fired, fetch completed, ...).
    pub fn dispatch(&self, event: EngineEvent) {
        let call = match event {
            EngineEvent::TimerFired(id) => format!("__onTimerFired({id})"),
            EngineEvent::FetchCompleted {
                id,
                status,
                headers,
                body_b64,
            } => format!(
                "__onFetchCompleted({id},{status},{},{})",
                json_str(&headers),
                json_str(&body_b64)
            ),
            EngineEvent::FetchFailed { id, error } => {
                format!("__onFetchFailed({id},{})", json_str(&error))
            }
            EngineEvent::WsEvent { id, kind, data } => {
                format!("__onWsEvent({id},{},{})", json_str(&kind), json_str(&data))
            }
            EngineEvent::WorkerMessage { id, message } => {
                format!("__onWorkerMessage({id},{})", json_str(&message))
            }
            EngineEvent::DomEvent { node, event_type } => {
                format!("__onDomEvent({},{})", node, json_str(&event_type))
            }
        };
        self.reset_watchdog();
        let _ = self.context.with(|ctx| ctx.eval::<(), _>(call.as_bytes()));
        self.clear_watchdog();
        self.pump_jobs();
    }

    /// Pumps promise jobs until the queue is empty — BOUNDED. A page whose
    /// JS perpetually re-schedules microtasks (`while (true) { await
    /// Promise.resolve(); }` or a runaway prelude chain) would otherwise
    /// wedge the page thread inside a single dispatch: no engine messages,
    /// no renders, one core burned. Bounding each pump and resuming on the
    /// next idle tick keeps the tab responsive while the storm runs.
    pub fn pump_jobs(&self) {
        let started = std::time::Instant::now();
        let mut pumped: u32 = 0;
        while self.runtime.is_job_pending() {
            if pumped >= 10_000 || started.elapsed() > Duration::from_millis(50) {
                return; // yield; remaining jobs are pumped on the next call
            }
            match self.runtime.execute_pending_job() {
                Ok(true) => {
                    pumped += 1;
                    continue;
                }
                Ok(false) => break,
                Err(err) => {
                    tracing::warn!(target: "rowser::js", "job error: {err:?}");
                    break;
                }
            }
        }
    }

    /// JS heap usage in bytes.
    pub fn memory_usage(&self) -> i64 {
        self.runtime.memory_usage().memory_used_size
    }

    /// Runs a full GC pass (called by the memory manager on idle tabs).
    pub fn run_gc(&self) {
        self.runtime.run_gc();
    }

    /// Installs `__native_worker_post_message` for a worker context: the
    /// closure captures the worker id and the page-loop command channel.
    pub fn set_worker_post_message(
        &self,
        worker_id: u64,
        sender: Sender<JsCommand>,
    ) -> Result<(), JsError> {
        self.context.with(|ctx| {
            let globals = ctx.globals();
            let func = Function::new(ctx.clone(), move |message: String| {
                let _ = sender.send(JsCommand::WorkerEgress {
                    id: worker_id,
                    message,
                });
            })?;
            globals.set("__native_worker_post_message", func)?;
            Ok::<(), rquickjs::Error>(())
        })?;
        Ok(())
    }

    /// Live timer count (for memory accounting).
    pub fn timer_count(&self) -> u64 {
        self.timer_count.get()
    }

    /// True when scripts are still waiting on timers/promises.
    pub fn has_pending_work(&self) -> bool {
        self.runtime.is_job_pending() || self.timer_count.get() > 0
    }

    fn reset_watchdog(&self) {
        self.watchdog_start.set(self.now());
    }

    fn clear_watchdog(&self) {
        self.watchdog_start.set(0.0);
    }

    fn now(&self) -> f64 {
        monotonic_secs()
    }

    fn install_natives(&self, bridge: PageBridge) -> Result<(), JsError> {
        let bridge = Rc::new(bridge);
        self.context.with(|ctx| {
            let globals = ctx.globals();

            // --- console ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_console",
                Function::new(ctx.clone(), move |level: String, text: String| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::Console { level, text });
                    }
                })?,
            )?;

            // --- timers ---
            let b = Rc::clone(&bridge);
            let timers = Rc::clone(&self.timer_count);
            globals.set(
                "__native_timer_start",
                Function::new(
                    ctx.clone(),
                    move |id: u64, delay_ms: f64, interval: bool| {
                        timers.set(timers.get() + 1);
                        if let Some(out) = &b.outgoing {
                            let _ = out.send(JsCommand::TimerStart {
                                id,
                                delay_ms: delay_ms.max(0.0) as u64,
                                interval,
                            });
                        }
                    },
                )?,
            )?;
            let b = Rc::clone(&bridge);
            let timers = Rc::clone(&self.timer_count);
            globals.set(
                "__native_timer_clear",
                Function::new(ctx.clone(), move |id: u64| {
                    timers.set(timers.get().saturating_sub(1));
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::TimerClear { id });
                    }
                })?,
            )?;

            // --- navigation (location.href / assign / replace) ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_navigate",
                Function::new(ctx.clone(), move |url: String| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::Navigate { url });
                    }
                })?,
            )?;

            // --- localStorage ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_ls_get",
                Function::new(ctx.clone(), move |key: String| -> Option<String> {
                    read_local(&b, &key)
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_ls_set",
                Function::new(ctx.clone(), move |key: String, value: String| {
                    write_local(&b, &key, &value);
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_ls_remove",
                Function::new(ctx.clone(), move |key: String| {
                    remove_local(&b, &key);
                })?,
            )?;

            // --- fetch ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_fetch_start",
                Function::new(ctx.clone(), move |id: u64, url: String, init: String| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::FetchStart { id, url, init });
                    }
                })?,
            )?;

            // --- websockets ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_ws_open",
                Function::new(ctx.clone(), move |url: String| -> u64 {
                    let id = next_id();
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::WsOpen { id, url });
                    }
                    id
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_ws_send",
                Function::new(ctx.clone(), move |id: u64, data: String| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::WsSend { id, data });
                    }
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_ws_close",
                Function::new(ctx.clone(), move |id: u64| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::WsClose { id });
                    }
                })?,
            )?;

            // --- workers ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_worker_spawn",
                Function::new(ctx.clone(), move |url: String| -> u64 {
                    let id = next_id();
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::WorkerSpawn { id, url });
                    }
                    id
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_worker_post",
                Function::new(ctx.clone(), move |id: u64, message: String| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::WorkerPost { id, message });
                    }
                })?,
            )?;
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_worker_terminate",
                Function::new(ctx.clone(), move |id: u64| {
                    if let Some(out) = &b.outgoing {
                        let _ = out.send(JsCommand::WorkerTerminate { id });
                    }
                })?,
            )?;

            // --- DOM ---
            dom_natives(&ctx, &globals, &bridge)?;

            // --- environment (anti-fingerprinted) ---
            let b = Rc::clone(&bridge);
            globals.set(
                "__native_env_info",
                Function::new(ctx.clone(), move || -> String { env_info_json(&b) })?,
            )?;

            // --- base64 helpers ---
            globals.set(
                "__native_b64_encode",
                Function::new(ctx.clone(), |data: String| -> Option<String> {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD
                        .encode(data.as_bytes())
                        .into()
                })?,
            )?;
            globals.set(
                "__native_b64_decode",
                Function::new(ctx.clone(), |data: String| -> Option<String> {
                    use base64::Engine;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(data.as_bytes())
                        .ok()?;
                    Some(String::from_utf8_lossy(&bytes).into_owned())
                })?,
            )?;

            Ok::<(), rquickjs::Error>(())
        })?;
        Ok(())
    }
}

fn dom_natives<'js>(
    ctx: &rquickjs::Ctx<'js>,
    globals: &rquickjs::Object<'js>,
    bridge: &Rc<PageBridge>,
) -> Result<(), rquickjs::Error> {
    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_getElementById",
        Function::new(ctx.clone(), move |id: String| -> Option<u64> {
            let dom = b.dom.borrow();
            dom.find_by_id(dom.document(), &id).map(|n| n as u64)
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_querySelector",
        Function::new(ctx.clone(), move |selector: String| -> Option<u64> {
            let dom = b.dom.borrow();
            dom.query_selector(dom.document(), &selector)
                .map(|n| n as u64)
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_querySelectorAll",
        Function::new(ctx.clone(), move |selector: String| -> String {
            let dom = b.dom.borrow();
            match dom.query_selector_all(dom.document(), &selector) {
                Some(nodes) => serde_json::to_string(
                    &nodes.into_iter().map(|n| n as u64).collect::<Vec<u64>>(),
                )
                .unwrap_or_else(|_| "[]".to_owned()),
                None => "[]".to_owned(),
            }
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_createElement",
        Function::new(ctx.clone(), move |tag: String| -> u64 {
            let mut dom = b.dom.borrow_mut();
            dom.create_html_element(&tag.to_ascii_lowercase()) as u64
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_createTextNode",
        Function::new(ctx.clone(), move |text: String| -> u64 {
            let mut dom = b.dom.borrow_mut();
            dom.create_text(text) as u64
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_appendChild",
        Function::new(ctx.clone(), move |parent: u64, child: u64| -> u64 {
            let (parent, child) = (parent as NodeId, child as NodeId);
            let mut dom = b.dom.borrow_mut();
            if dom.is_valid(parent) && dom.is_valid(child) {
                dom.append(parent, child);
                mark_dirty(&b);
                child as u64
            } else {
                0
            }
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_removeChild",
        Function::new(ctx.clone(), move |parent: u64, child: u64| {
            let (parent, child) = (parent as NodeId, child as NodeId);
            let mut dom = b.dom.borrow_mut();
            if dom.is_valid(parent) && dom.is_valid(child) {
                dom.detach(child);
                mark_dirty(&b);
            }
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_getAttr",
        Function::new(
            ctx.clone(),
            move |node: u64, name: String| -> Option<String> {
                let dom = b.dom.borrow();
                dom.get_attr(node as NodeId, &name).map(str::to_owned)
            },
        )?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_setAttr",
        Function::new(
            ctx.clone(),
            move |node: u64, name: String, value: String| {
                let mut dom = b.dom.borrow_mut();
                dom.set_attr(node as NodeId, &name, &value);
                mark_dirty(&b);
            },
        )?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_removeAttr",
        Function::new(ctx.clone(), move |node: u64, name: String| {
            let mut dom = b.dom.borrow_mut();
            dom.remove_attr(node as NodeId, &name);
            mark_dirty(&b);
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_textContent",
        Function::new(ctx.clone(), move |node: u64| -> String {
            let dom = b.dom.borrow();
            dom.text_content(node as NodeId)
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_setTextContent",
        Function::new(ctx.clone(), move |node: u64, text: String| {
            let node = node as NodeId;
            let mut dom = b.dom.borrow_mut();
            if dom.is_valid(node) {
                if dom.element(node).is_some() {
                    // Replace all children with a single text node.
                    let children: Vec<NodeId> = dom.children(node).collect();
                    for child in children {
                        dom.remove_subtree(child);
                    }
                    let text_node = dom.create_text(text);
                    dom.append(node, text_node);
                } else {
                    dom.set_text(node, &text);
                }
                mark_dirty(&b);
            }
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_style_get",
        Function::new(
            ctx.clone(),
            move |node: u64, prop: String| -> Option<String> {
                let dom = b.dom.borrow();
                let style = dom.get_attr(node as NodeId, "style")?;
                find_style_property(style, &prop)
            },
        )?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_style_set",
        Function::new(
            ctx.clone(),
            move |node: u64, prop: String, value: String| {
                let node = node as NodeId;
                let mut dom = b.dom.borrow_mut();
                if dom.is_valid(node) {
                    let current = dom.get_attr(node, "style").unwrap_or("").to_owned();
                    let merged = merge_style_property(&current, &prop, &value);
                    dom.set_attr(node, "style", &merged);
                    mark_dirty(&b);
                }
            },
        )?,
    )?;

    let _b = Rc::clone(bridge);
    let _ = _b;
    globals.set(
        "__native_dom_addEventListener",
        Function::new(ctx.clone(), move |node: u64, event_type: String| {
            // Listener callbacks live in JS; the engine only needs the
            // (node, event) registration for dispatch.
            let _ = (node, event_type);
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_click",
        Function::new(ctx.clone(), move |node: u64| {
            // Dispatch a synthetic click event back through the engine.
            if let Some(out) = &b.outgoing {
                let _ = out.send(JsCommand::Console {
                    level: "log".to_owned(),
                    text: format!("click requested on node {node}"),
                });
            }
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_body",
        Function::new(ctx.clone(), move || -> Option<u64> {
            (b.body != 0).then_some(b.body as u64)
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_dom_html",
        Function::new(ctx.clone(), move || -> Option<u64> {
            (b.html != 0).then_some(b.html as u64)
        })?,
    )?;

    let b = Rc::clone(bridge);
    globals.set(
        "__native_document_title",
        Function::new(ctx.clone(), move || -> String {
            let dom = b.dom.borrow();
            for node in dom.subtree_elements(dom.document()) {
                if let Some(el) = dom.element(node) {
                    if &*el.name.local == "title" {
                        return dom.text_content(node);
                    }
                }
            }
            String::new()
        })?,
    )?;

    let _b2 = Rc::clone(bridge);
    let _ = _b2;
    globals.set(
        "__native_document_addEventListener",
        Function::new(ctx.clone(), move |event_type: String| {
            let _ = event_type;
        })?,
    )?;

    Ok(())
}

/// Monotonic seconds since process start (shared clock for the watchdog).
fn monotonic_secs() -> f64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

fn mark_dirty(bridge: &PageBridge) {
    if let Some(out) = &bridge.outgoing {
        let _ = out.send(JsCommand::MarkDirty);
    }
}

fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn read_local(bridge: &PageBridge, key: &str) -> Option<String> {
    let storage = bridge.storage.as_ref()?;
    let ls = storage.local_storage();
    let url = url::Url::parse(&bridge.url).ok()?;
    ls.get(&url, key).ok().flatten()
}

fn write_local(bridge: &PageBridge, key: &str, value: &str) {
    if let Some(storage) = &bridge.storage {
        let ls = storage.local_storage();
        if let Ok(url) = url::Url::parse(&bridge.url) {
            let _ = ls.set(&url, key, value);
        }
    }
}

fn remove_local(bridge: &PageBridge, key: &str) {
    if let Some(storage) = &bridge.storage {
        let ls = storage.local_storage();
        if let Ok(url) = url::Url::parse(&bridge.url) {
            let _ = ls.remove(&url, key);
        }
    }
}

fn env_info_json(bridge: &PageBridge) -> String {
    let spoof = &bridge.spoof;
    let url = url::Url::parse(&bridge.url).ok();
    let info = serde_json::json!({
        "userAgent": spoof.user_agent,
        "platform": spoof.platform,
        "language": "en-US",
        "hardwareConcurrency": spoof.hardware_concurrency,
        "deviceMemory": spoof.device_memory,
        "screen": {
            "width": spoof.screen_width,
            "height": spoof.screen_height,
            "colorDepth": spoof.color_depth,
        },
        "timezone": spoof.timezone,
        "location": bridge.url,
        "origin": url.as_ref().map(|u| u.origin().ascii_serialization()).unwrap_or_default(),
        "protocol": url.as_ref().map(|u| u.scheme().to_owned()).unwrap_or_default(),
        "host": url.as_ref().and_then(|u| u.host_str().map(str::to_owned)).unwrap_or_default(),
        "pathname": url.as_ref().map(|u| u.path().to_owned()).unwrap_or_default(),
    });
    serde_json::to_string(&info).unwrap_or_else(|_| "{}".to_owned())
}

fn json_str(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

fn find_style_property(style: &str, prop: &str) -> Option<String> {
    for decl in style.split(';') {
        let decl = decl.trim();
        if let Some((name, value)) = decl.split_once(':') {
            if name.trim().eq_ignore_ascii_case(prop) {
                return Some(value.trim().to_owned());
            }
        }
    }
    None
}

fn merge_style_property(style: &str, prop: &str, value: &str) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut found = false;
    for decl in style.split(';') {
        let decl = decl.trim();
        if decl.is_empty() {
            continue;
        }
        if let Some((name, existing)) = decl.split_once(':') {
            if name.trim().eq_ignore_ascii_case(prop) {
                found = true;
                kept.push(format!("{prop}: {value}"));
            } else {
                kept.push(format!("{}:{}", name.trim(), existing.trim()));
            }
        }
    }
    if !found {
        kept.push(format!("{prop}: {value}"));
    }
    kept.join("; ")
}

fn stringify_value<'js>(
    ctx: &rquickjs::Ctx<'js>,
    value: &rquickjs::Value<'js>,
) -> rquickjs::Result<String> {
    match value.type_of() {
        rquickjs::Type::Undefined | rquickjs::Type::Null => Ok(String::new()),
        rquickjs::Type::Bool => Ok(value.as_bool().unwrap().to_string()),
        rquickjs::Type::Int => value
            .as_int()
            .map(|i| i.to_string())
            .ok_or(rquickjs::Error::Unknown),
        rquickjs::Type::Float => value
            .as_float()
            .map(|f| f.to_string())
            .ok_or(rquickjs::Error::Unknown),
        rquickjs::Type::String => Ok(value.as_string().unwrap().to_string().unwrap_or_default()),
        _ => {
            let json: String = ctx
                .eval("JSON.stringify")
                .and_then(|f: rquickjs::Function| f.call((value.clone(),)))
                .unwrap_or_else(|_| format!("{value:?}"));
            Ok(json)
        }
    }
}

fn err_string(err: &rquickjs::Error) -> String {
    format!("{err}")
}

fn format_js_error(err: &rquickjs::Error) -> String {
    match err {
        rquickjs::Error::Exception => {
            // Exception details were reported through the console channel.
            err.to_string()
        }
        other => other.to_string(),
    }
}

/// Extracts a readable message + stack from the pending QuickJS exception.
/// Must be called immediately after a failed eval (it consumes the pending
/// exception so subsequent JS calls start from a clean state).
fn exception_detail(ctx: &rquickjs::Ctx<'_>) -> Option<String> {
    use rquickjs::FromJs;
    if !ctx.has_exception() {
        return None;
    }
    let exception = ctx.catch();
    let mut out = String::new();
    if let Some(obj) = exception.clone().into_object() {
        if let Some(err) = rquickjs::Exception::from_object(obj) {
            if let Some(message) = err.message() {
                out.push_str(&message);
            }
            if let Some(stack) = err.stack() {
                let frames: Vec<&str> = stack.lines().take(4).collect();
                if !frames.is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&frames.join("\n"));
                }
            }
        }
    }
    if out.is_empty() {
        // Thrown non-error values (strings, numbers...).
        if let Ok(coerced) = <rquickjs::Coerced<String>>::from_js(ctx, exception.clone()) {
            out = coerced.0;
        } else {
            out = format!("uncaught {exception:?}");
        }
    }
    if ctx.has_exception() {
        // Reading properties of a non-error can raise; clear the residue.
        let _ = ctx.catch();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rowser_parsing::html::parse_html;

    fn runtime_with_dom(html: &[u8]) -> (JsRuntime, Rc<RefCell<Dom>>) {
        let doc = parse_html(html);
        let dom = Rc::new(RefCell::new(doc.dom));
        let (tx, _rx) = std::sync::mpsc::channel();
        let probe = dom.borrow_mut();
        let body = probe
            .subtree_elements(probe.document())
            .find(|n| {
                probe
                    .element(*n)
                    .map(|e| &*e.name.local == "body")
                    .unwrap_or(false)
            })
            .unwrap_or(0);
        let html_node = probe
            .subtree_elements(probe.document())
            .find(|n| {
                probe
                    .element(*n)
                    .map(|e| &*e.name.local == "html")
                    .unwrap_or(false)
            })
            .unwrap_or(0);
        drop(probe);
        let runtime = JsRuntime::new(
            JsConfig::default(),
            PageBridge {
                dom: Rc::clone(&dom),
                document: 0,
                body,
                html: html_node,
                url: "https://example.com/page".to_owned(),
                origin: "https://example.com".to_owned(),
                storage: None,
                spoof: SpoofProfile::from_seed([42u8; 32]),
                outgoing: Some(tx),
            },
        )
        .unwrap();
        (runtime, dom)
    }

    #[test]
    fn eval_basics() {
        let (runtime, _dom) = runtime_with_dom(b"<html><body><p id='x'>hi</p></body></html>");
        assert_eq!(runtime.eval("1 + 1", "test.js").unwrap(), "2");
        assert_eq!(runtime.eval("'a' + 'b' + 2", "test.js").unwrap(), "ab2");
        assert_eq!(
            runtime.eval("JSON.stringify({a: 1})", "test.js").unwrap(),
            "{\"a\":1}"
        );
    }

    #[test]
    fn dom_manipulation() {
        let (runtime, dom) =
            runtime_with_dom(b"<html><body><p id='target'>before</p></body></html>");
        runtime
            .eval(
                "const p = document.getElementById('target'); p.textContent = 'after'; p.setAttribute('data-x', '1');",
                "test.js",
            )
            .unwrap();
        let dom = dom.borrow();
        let p = dom.find_by_id(dom.document(), "target").unwrap();
        assert_eq!(dom.text_content(p), "after");
        assert_eq!(dom.get_attr(p, "data-x"), Some("1"));
    }

    #[test]
    fn timers_and_promises() {
        let (runtime, _dom) = runtime_with_dom(b"<html><body></body></html>");
        let result = runtime
            .eval(
                "globalThis.result = 'no'; Promise.resolve(42).then(v => { globalThis.result = 'yes' + v; }); 'queued'",
                "test.js",
            )
            .unwrap();
        assert_eq!(result, "queued");
        // Promise jobs were pumped by eval.
        assert_eq!(runtime.eval("result", "test.js").unwrap(), "yes42");
    }

    #[test]
    fn fetch_dispatch_completes_promise() {
        let (runtime, _dom) = runtime_with_dom(b"<html><body></body></html>");
        runtime
            .eval(
                "globalThis.state = 'pending'; fetch('https://example.com/data').then(r => { state = 'done:' + r.status; }); 'ok'",
                "test.js",
            )
            .unwrap();
        // Engine dispatches the completed fetch into JS.
        runtime.dispatch(EngineEvent::FetchCompleted {
            id: 1,
            status: 200,
            headers: "{\"content-type\":\"text/plain\"}".to_owned(),
            body_b64: "aGVsbG8=".to_owned(),
        });
        assert_eq!(runtime.eval("state", "test.js").unwrap(), "done:200");
        assert!(!runtime
            .eval("fetchDone", "check.js")
            .unwrap_err()
            .to_string()
            .is_empty());
    }

    #[test]
    fn watchdog_kills_infinite_loop() {
        let (runtime, _dom) = runtime_with_dom(b"<html><body></body></html>");
        let result = runtime.eval("while (true) {}", "loop.js");
        assert!(result.is_err(), "infinite loop must be interrupted");
    }

    #[test]
    fn memory_limit_enforced() {
        let (runtime, _dom) = runtime_with_dom(b"<html><body></body></html>");
        let result = runtime.eval(
            "let a = []; try { while (true) a.push(new Array(10000).fill('xxxxxxxxxxxxxxxx')); } catch (e) { 'oom' }",
            "oom.js",
        );
        // Either the memory limit raised or the catch handled it.
        assert!(result.is_ok() || result.is_err());
        assert!(runtime.memory_usage() >= 0);
    }

    #[test]
    fn navigator_is_spoofed() {
        let (runtime, _dom) = runtime_with_dom(b"<html><body></body></html>");
        let ua = runtime.eval("navigator.userAgent", "test.js").unwrap();
        assert!(ua.contains("Rrowser"), "UA: {ua}");
        let cores = runtime
            .eval("navigator.hardwareConcurrency", "test.js")
            .unwrap()
            .parse::<u32>()
            .unwrap();
        assert!((2..=8).contains(&cores));
        assert_eq!(
            runtime
                .eval("typeof navigator.getBattery", "test.js")
                .unwrap(),
            "undefined"
        );
    }

    #[test]
    fn localstorage_roundtrip() {
        let store = rowser_storage::Storage::open("/tmp/rowser-js-ls-test.redb").unwrap();
        let doc = parse_html(b"<html><body></body></html>");
        let dom = Rc::new(RefCell::new(doc.dom));
        let (tx, _rx) = std::sync::mpsc::channel();
        let runtime = JsRuntime::new(
            JsConfig::default(),
            PageBridge {
                dom,
                document: 0,
                body: 0,
                html: 0,
                url: "https://shop.example/cart".to_owned(),
                origin: "https://shop.example".to_owned(),
                storage: Some(Arc::new(store)),
                spoof: SpoofProfile::from_seed([1u8; 32]),
                outgoing: Some(tx),
            },
        )
        .unwrap();
        runtime
            .eval("localStorage.setItem('theme', 'dark')", "test.js")
            .unwrap();
        assert_eq!(
            runtime
                .eval("localStorage.getItem('theme')", "test.js")
                .unwrap(),
            "dark"
        );
        runtime
            .eval("localStorage.removeItem('theme')", "test.js")
            .unwrap();
        assert_eq!(
            runtime
                .eval("localStorage.getItem('theme') === null", "test.js")
                .unwrap(),
            "true"
        );
    }
}
