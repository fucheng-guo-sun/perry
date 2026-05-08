//! `HttpServer` — backing `http.createServer(handler)`. Binds via
//! hyper's HTTP/1.1 service path, pushes `(req, res)` to the main
//! thread, runs the user handler synchronously (awaiting any
//! returned Promise), and flushes the buffered response back through
//! the per-request oneshot channel.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{body::Incoming, Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use perry_ffi::{
    alloc_string, get_handle, get_handle_mut, register_handle, JsClosure, JsValue,
    RawClosureHeader, StringHeader,
};

use crate::ensure_gc_scanner_registered;
use crate::request::{
    alloc_incoming_message, emit_no_arg_to_listeners, handle_to_pointer_f64, IncomingMessage,
};
use crate::response::{alloc_server_response, HyperResponseShape, ServerResponse};
use crate::types::{
    extract_host, extract_port, js_gc_enter_unsafe_zone, js_is_promise, js_promise_run_microtasks,
    js_promise_state, js_promise_value, jsvalue_to_owned_string, read_string_header, wait_for_promise,
    Promise, POINTER_TAG, PTR_MASK, TAG_NULL, TAG_UNDEFINED,
};

/// Backing struct for an `http.Server` JS-side handle.
pub struct HttpServer {
    /// User's `(req, res) => ...` handler. Stored as raw `i64`; the
    /// GC root scanner pins it across malloc-triggered sweeps.
    pub handler: i64,
    /// Server-level event listeners (`'request'`, `'connection'`,
    /// `'close'`, `'listening'`, `'error'`, `'upgrade'`).
    pub listeners: HashMap<String, Vec<i64>>,
    /// Bound port — populated after `.listen()` resolves.
    pub bound_port: u16,
    /// Bound host (e.g. `"0.0.0.0"`).
    pub bound_host: String,
    /// True between `.listen()` and `.close()`.
    pub listening: bool,
    /// Sent by `.close()` to wake the accept loop.
    pub shutdown_tx: Option<oneshot::Sender<()>>,
    /// Channel main thread drains in the event loop. Hyper service
    /// fns push pending requests through this; main thread invokes
    /// the handler closure and flushes the response.
    pub request_rx: Option<mpsc::Receiver<HttpPendingRequest>>,
    /// Phase 4 — upgrade events queued from hyper service fns once
    /// the WebSocket handshake completes. Drained alongside
    /// `request_rx` in `event_loop`.
    pub upgrade_rx: Option<mpsc::Receiver<HttpPendingUpgrade>>,
}

/// Pending request from the hyper service fn to the main thread.
pub struct HttpPendingRequest {
    pub server_handle: i64,
    pub request_handle: i64,
    pub response_handle: i64,
    /// `'request'` listeners snapshotted at request time so the
    /// dispatch loop doesn't need to re-borrow the server handle.
    pub request_listeners: Vec<i64>,
    pub handler: i64,
}

/// Phase 4 — pending WebSocket upgrade ready to fire `'upgrade'`
/// listeners. Sent by the hyper service fn after the underlying
/// `hyper::upgrade::on` future resolves and the upgraded stream has
/// been registered with `perry_ext_ws::register_external_ws_stream`.
pub struct HttpPendingUpgrade {
    pub server_handle: i64,
    pub request_handle: i64,
    pub ws_id: i64,
}

// ============================================================================
// FFI: createServer / listen / close / address
// ============================================================================

/// `http.createServer(handler)` — register an `HttpServer` handle.
#[no_mangle]
pub extern "C" fn js_node_http_create_server(handler: i64) -> i64 {
    ensure_gc_scanner_registered();
    register_handle(HttpServer {
        handler,
        listeners: HashMap::new(),
        bound_port: 0,
        bound_host: String::new(),
        listening: false,
        shutdown_tx: None,
        request_rx: None,
        upgrade_rx: None,
    })
}

/// `server.listen(port, host?, backlog?, cb?)` — bind + start
/// accepting. Blocks the calling thread (main TS thread) for the
/// process lifetime, draining pending requests and dispatching to
/// the user handler.
///
/// `opts_f64` accepts either a bare numeric port, an object literal
/// (`{ port, host, backlog }`), or undefined for "default" (3000).
/// The TS-side wrapper normalizes Node's many `listen()` overloads
/// into this single shape.
#[no_mangle]
pub unsafe extern "C" fn js_node_http_server_listen(
    server_handle: i64,
    opts_f64: f64,
    callback: i64,
) {
    let port = extract_port(opts_f64, 3000);
    let host = extract_host(opts_f64, "0.0.0.0");

    let (request_tx, request_rx) = mpsc::channel::<HttpPendingRequest>(1024);
    let (upgrade_tx, upgrade_rx) = mpsc::channel::<HttpPendingUpgrade>(256);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

    if let Some(s) = get_handle_mut::<HttpServer>(server_handle) {
        s.bound_port = port;
        s.bound_host = host.clone();
        s.listening = true;
        s.shutdown_tx = Some(shutdown_tx);
        s.request_rx = Some(request_rx);
        s.upgrade_rx = Some(upgrade_rx);
    } else {
        return;
    }

    // Mark GC-unsafe — request closures dispatch on tokio worker
    // threads whose stacks the main-thread GC can't scan (issue #31).
    js_gc_enter_unsafe_zone();

    let request_tx = Arc::new(request_tx);
    let upgrade_tx = Arc::new(upgrade_tx);
    let request_tx_for_spawn = request_tx.clone();
    let upgrade_tx_for_spawn = upgrade_tx.clone();
    let host_for_spawn = host.clone();

    // The closure passed to `spawn_blocking_with_reactor` runs INSIDE
    // a tokio worker task (perry-stdlib's shim wraps it in
    // `runtime().spawn(async { invoke(...) })`), so calling
    // `Handle::current().block_on(fut)` would panic with
    // "Cannot start a runtime from within a runtime". Spawn the
    // accept loop as a separate async task on the existing runtime
    // and let the closure return immediately.
    perry_ffi::spawn_blocking_with_reactor(move || {
        tokio::spawn(async move {
            let bind_str = format!("{}:{}", host_for_spawn, port);
            let addr: SocketAddr = match bind_str.parse() {
                Ok(a) => a,
                Err(_) => SocketAddr::from(([0, 0, 0, 0], port)),
            };
            let listener = match TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[node:http] bind {}:{} failed: {}", host_for_spawn, port, e);
                    return;
                }
            };
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, peer)) => {
                                let io = TokioIo::new(stream);
                                let request_tx = request_tx_for_spawn.clone();
                                let upgrade_tx = upgrade_tx_for_spawn.clone();
                                let server_handle = server_handle;
                                tokio::spawn(async move {
                                    let service = service_fn(move |req: Request<Incoming>| {
                                        let request_tx = request_tx.clone();
                                        let upgrade_tx = upgrade_tx.clone();
                                        async move {
                                            handle_request(server_handle, peer, req, request_tx, upgrade_tx).await
                                        }
                                    });
                                    if let Err(e) = http1::Builder::new()
                                        .serve_connection(io, service)
                                        .with_upgrades()
                                        .await
                                    {
                                        // Common when client closes mid-request — silenced.
                                        let _ = e;
                                    }
                                });
                            }
                            Err(e) => eprintln!("[node:http] accept error: {}", e),
                        }
                    }
                    _ = &mut shutdown_rx => {
                        break;
                    }
                }
            }
        });
    });

    // Fire `'listening'` listeners + the optional `cb` argument.
    let listening_listeners = get_handle::<HttpServer>(server_handle)
        .and_then(|s| s.listeners.get("listening").cloned())
        .unwrap_or_default();
    emit_no_arg_to_listeners(&listening_listeners);
    if callback != 0 {
        let raw = callback as *const RawClosureHeader;
        let closure = JsClosure::from_raw(raw);
        if !closure.is_null() {
            let _ = closure.call0();
        }
    }

    eprintln!("[node:http] listening on http://{}:{}", host, port);

    // Main-thread event loop — drain requests + dispatch handler.
    event_loop(server_handle);
}

/// `server.close(cb?)` — drop the shutdown channel, fire `'close'`.
/// The accept loop's `tokio::select!` picks up the channel close and
/// exits.
#[no_mangle]
pub unsafe extern "C" fn js_node_http_server_close(server_handle: i64, callback: i64) {
    let close_listeners;
    if let Some(s) = get_handle_mut::<HttpServer>(server_handle) {
        s.listening = false;
        s.shutdown_tx.take();
        close_listeners = s.listeners.get("close").cloned().unwrap_or_default();
    } else {
        close_listeners = Vec::new();
    }
    emit_no_arg_to_listeners(&close_listeners);
    if callback != 0 {
        let raw = callback as *const RawClosureHeader;
        let closure = JsClosure::from_raw(raw);
        if !closure.is_null() {
            let _ = closure.call0();
        }
    }
}

/// `server.closeAllConnections()` — placeholder. Active hyper
/// connections live in their own tokio tasks; we'd need to thread an
/// abort handle through every task. For Phase 1 this is a no-op
/// (matches `closeIdleConnections` too).
#[no_mangle]
pub extern "C" fn js_node_http_server_close_all_connections(_handle: i64) {}

#[no_mangle]
pub extern "C" fn js_node_http_server_close_idle_connections(_handle: i64) {}

/// `server.address()` — returns `{ port, address, family }` as a
/// JSON-stringified object. TS-side wrapper parses with `JSON.parse`.
#[no_mangle]
pub extern "C" fn js_node_http_server_address_json(handle: i64) -> *mut StringHeader {
    let s = get_handle::<HttpServer>(handle)
        .map(|s| {
            if !s.listening {
                "null".to_string()
            } else {
                let family = if s.bound_host.contains(':') {
                    "IPv6"
                } else {
                    "IPv4"
                };
                serde_json::json!({
                    "port": s.bound_port,
                    "address": s.bound_host,
                    "family": family,
                })
                .to_string()
            }
        })
        .unwrap_or_else(|| "null".to_string());
    alloc_string(&s).as_raw()
}

/// `server.listening` getter.
#[no_mangle]
pub extern "C" fn js_node_http_server_listening(handle: i64) -> i32 {
    get_handle::<HttpServer>(handle)
        .map(|s| if s.listening { 1 } else { 0 })
        .unwrap_or(0)
}

/// `server.on(event, cb)` — register a listener. Standard event names:
/// `'request'`, `'connection'`, `'close'`, `'listening'`, `'error'`,
/// `'upgrade'`.
#[no_mangle]
pub unsafe extern "C" fn js_node_http_server_on(
    handle: i64,
    event_name_ptr: *const StringHeader,
    callback: i64,
) -> f64 {
    let event = read_string_header(event_name_ptr as *mut _).unwrap_or_default();
    if let Some(s) = get_handle_mut::<HttpServer>(handle) {
        s.listeners.entry(event).or_default().push(callback);
    }
    handle_to_pointer_f64(handle)
}

// ============================================================================
// Request dispatch — hyper service fn + main-thread event loop
// ============================================================================

async fn handle_request(
    server_handle: i64,
    peer: SocketAddr,
    req: Request<Incoming>,
    request_tx: Arc<mpsc::Sender<HttpPendingRequest>>,
    upgrade_tx: Arc<mpsc::Sender<HttpPendingUpgrade>>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let method = req.method().to_string();
    let uri = req.uri();
    let url = match uri.query() {
        Some(q) => format!("{}?{}", uri.path(), q),
        None => uri.path().to_string(),
    };

    let mut headers_lower: HashMap<String, String> = HashMap::new();
    let mut raw_headers: Vec<(String, String)> = Vec::new();
    for (name, value) in req.headers() {
        if let Ok(v) = value.to_str() {
            headers_lower.insert(name.to_string().to_lowercase(), v.to_string());
            raw_headers.push((name.to_string(), v.to_string()));
        }
    }

    // Phase 4 — WebSocket upgrade detection. If the request looks
    // like a WS upgrade, branch into the handshake path: build the
    // 101 Switching Protocols response synchronously and spawn a
    // task that awaits hyper's upgraded stream + completes the
    // tungstenite server handshake + registers the resulting
    // WebSocketStream with perry-ext-ws.
    if crate::upgrade::is_websocket_upgrade(&req) {
        return handle_websocket_upgrade(
            server_handle,
            peer,
            req,
            method,
            url,
            headers_lower,
            raw_headers,
            upgrade_tx,
        )
        .await;
    }

    let body_bytes = match req.collect().await {
        Ok(collected) => collected.to_bytes().to_vec(),
        Err(_) => Vec::new(),
    };

    let im = IncomingMessage::new(
        method,
        url,
        headers_lower,
        raw_headers,
        body_bytes,
        peer.ip().to_string(),
        peer.port(),
    );
    let im_handle = alloc_incoming_message(im);

    let (response_tx, response_rx) = oneshot::channel::<HyperResponseShape>();
    let sr_handle = alloc_server_response(response_tx);

    let (request_listeners, handler) = match get_handle::<HttpServer>(server_handle) {
        Some(s) => (
            s.listeners.get("request").cloned().unwrap_or_default(),
            s.handler,
        ),
        None => (Vec::new(), 0),
    };

    let pending = HttpPendingRequest {
        server_handle,
        request_handle: im_handle,
        response_handle: sr_handle,
        request_listeners,
        handler,
    };

    if request_tx.send(pending).await.is_err() {
        // Channel closed — return 503 directly.
        return Ok(Response::builder()
            .status(503)
            .body(Full::new(Bytes::from("Server unavailable")))
            .unwrap());
    }

    perry_ffi::notify_main_thread();

    match response_rx.await {
        Ok(shape) => Ok(shape.into_hyper()),
        Err(_) => Ok(Response::builder()
            .status(500)
            .body(Full::new(Bytes::from("Handler error")))
            .unwrap()),
    }
}

/// Phase 4 — WebSocket upgrade dispatch.
///
/// Synchronously builds the 101 response (so hyper drives the
/// protocol switch) and spawns a tokio task that awaits the
/// upgraded stream + finishes the handshake server-side via
/// `tokio_tungstenite::WebSocketStream::from_raw_socket`. The
/// resulting WS stream is registered through perry-ext-ws and an
/// `HttpPendingUpgrade` is pushed to the main-thread upgrade
/// channel; the event-loop fires the user's `'upgrade'` listeners
/// with `(req, wsId, head)`.
async fn handle_websocket_upgrade(
    server_handle: i64,
    peer: SocketAddr,
    mut req: Request<Incoming>,
    method: String,
    url: String,
    headers_lower: HashMap<String, String>,
    raw_headers: Vec<(String, String)>,
    upgrade_tx: Arc<mpsc::Sender<HttpPendingUpgrade>>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    // Compute the Sec-WebSocket-Accept value before consuming req.
    let accept_value = req
        .headers()
        .get("sec-websocket-key")
        .and_then(|v| v.to_str().ok())
        .map(|k| {
            tokio_tungstenite::tungstenite::handshake::derive_accept_key(k.as_bytes())
        })
        .unwrap_or_default();

    // Build the upgraded-protocol IncomingMessage now (no body — WS
    // upgrades carry no request body).
    let mut im = IncomingMessage::new(
        method,
        url,
        headers_lower,
        raw_headers,
        Vec::new(),
        peer.ip().to_string(),
        peer.port(),
    );
    im.complete = true;
    let im_handle = alloc_incoming_message(im);

    // Spawn a task that waits for hyper to perform the protocol
    // switch + completes the tungstenite handshake + hands the
    // resulting stream to perry-ext-ws.
    tokio::spawn(async move {
        let upgraded = match hyper::upgrade::on(&mut req).await {
            Ok(u) => u,
            Err(_) => return,
        };
        let io = TokioIo::new(upgraded);
        let ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
            io,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        let ws_id = perry_ext_ws::register_external_ws_stream(ws);
        let pending = HttpPendingUpgrade {
            server_handle,
            request_handle: im_handle,
            ws_id,
        };
        let _ = upgrade_tx.send(pending).await;
        perry_ffi::notify_main_thread();
    });

    Ok(Response::builder()
        .status(101)
        .header("upgrade", "websocket")
        .header("connection", "Upgrade")
        .header("sec-websocket-accept", accept_value)
        .body(Full::new(Bytes::new()))
        .unwrap())
}

/// Main-thread event loop. Drains pending requests + upgrade events
/// + dispatches to the user handler / `'upgrade'` listener. Never
/// returns; the process exits when the user invokes `process.exit`
/// or hits a top-level error.
fn event_loop(server_handle: i64) {
    extern "C" {
        // perry-ext-ws's main-thread pump — drains pending WS
        // events (message / close / error) and dispatches to user
        // listeners. Without this call here, WebSocket connections
        // routed in via the Phase 4 upgrade path queue events
        // forever and `ws.on('message', ...)` listeners never fire.
        fn js_ws_process_pending() -> i32;
    }
    loop {
        unsafe {
            js_promise_run_microtasks();
            js_ws_process_pending();
        }
        // Drain any ready upgrade events first so they don't get
        // starved by a busy request stream.
        while let Some(up) = try_recv_upgrade(server_handle) {
            crate::upgrade::fire_upgrade_listeners(
                up.server_handle,
                up.request_handle,
                up.ws_id,
                Vec::new(),
            );
        }
        let pending = match try_recv_pending(server_handle) {
            Some(p) => p,
            None => continue,
        };
        process_pending(pending);
    }
}

fn try_recv_upgrade(server_handle: i64) -> Option<HttpPendingUpgrade> {
    if let Some(s) = get_handle_mut::<HttpServer>(server_handle) {
        if let Some(rx) = s.upgrade_rx.as_mut() {
            match rx.try_recv() {
                Ok(p) => return Some(p),
                Err(_) => return None,
            }
        }
    }
    None
}

fn try_recv_pending(server_handle: i64) -> Option<HttpPendingRequest> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_millis(10);
    loop {
        // Borrow the rx briefly to call try_recv.
        let result = if let Some(s) = get_handle_mut::<HttpServer>(server_handle) {
            if let Some(rx) = s.request_rx.as_mut() {
                rx.try_recv()
            } else {
                return None;
            }
        } else {
            return None;
        };
        match result {
            Ok(p) => return Some(p),
            Err(mpsc::error::TryRecvError::Disconnected) => return None,
            Err(mpsc::error::TryRecvError::Empty) => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_micros(200));
            }
        }
    }
}

/// Dispatch one pending request — fire `'request'` listeners, then
/// the main handler, then await any returned Promise. The handler is
/// expected to call `res.end(...)` itself; the response oneshot
/// fires from inside `js_node_http_res_end`.
fn process_pending(pending: HttpPendingRequest) {
    let req_f64 = handle_to_pointer_f64(pending.request_handle);
    let res_f64 = handle_to_pointer_f64(pending.response_handle);

    // Fire `'request'` listeners (Node's `server.on('request', ...)`).
    for cb in &pending.request_listeners {
        if *cb == 0 {
            continue;
        }
        unsafe {
            let raw = *cb as *const RawClosureHeader;
            let closure = JsClosure::from_raw(raw);
            if !closure.is_null() {
                let _ = closure.call2(req_f64, res_f64);
            }
            js_promise_run_microtasks();
        }
    }

    // Main handler.
    if pending.handler != 0 {
        let result = unsafe {
            let raw = pending.handler as *const RawClosureHeader;
            let closure = JsClosure::from_raw(raw);
            if closure.is_null() {
                f64::from_bits(TAG_UNDEFINED)
            } else {
                closure.call2(req_f64, res_f64)
            }
        };
        unsafe {
            js_promise_run_microtasks();
        }
        // Await any returned Promise so `async (req, res) => …`
        // handlers settle before the next iteration.
        let jsv = JsValue::from_bits(result.to_bits());
        if jsv.is_pointer() {
            let ptr = jsv.as_pointer::<Promise>();
            if !ptr.is_null() && unsafe { js_is_promise(ptr) } != 0 {
                wait_for_promise(ptr);
                let _ = unsafe { js_promise_value(ptr) };
            }
        }
    }

    // If the handler didn't call `res.end()` (still has the channel),
    // synthesize a default 200 with empty body so hyper's service fn
    // doesn't hang.
    synthesize_default_response_if_needed(pending.response_handle);

    // Free the per-request handles.
    perry_ffi::drop_handle(pending.request_handle);
    perry_ffi::drop_handle(pending.response_handle);
}

/// If the handler didn't call `res.end()`, finish the response
/// transparently with whatever buffer / status was set so hyper's
/// service fn doesn't hang awaiting the oneshot.
pub(crate) fn synthesize_default_response_if_needed(response_handle: i64) {
    if let Some(sr) = get_handle_mut::<ServerResponse>(response_handle) {
        if !sr.writable_ended {
            sr.writable_ended = true;
            sr.headers_sent = true;
            sr.writable_finished = true;
            let body = std::mem::take(&mut sr.buffered_body);
            let mut headers = Vec::with_capacity(sr.headers.len());
            for (lower_k, v) in &sr.headers {
                let orig = sr
                    .raw_header_names
                    .get(lower_k)
                    .cloned()
                    .unwrap_or_else(|| lower_k.clone());
                headers.push((orig, v.clone()));
            }
            if !sr.headers.contains_key("content-length")
                && !sr.headers.contains_key("transfer-encoding")
            {
                headers.push(("Content-Length".to_string(), body.len().to_string()));
            }
            let shape = HyperResponseShape {
                status: sr.status_code,
                status_message: sr.status_message.clone(),
                headers,
                body,
            };
            if let Some(tx) = sr.response_tx.take() {
                let _ = tx.send(shape);
            }
        }
    }
}

#[allow(dead_code)]
fn _force_link_helpers(v: f64) -> Option<String> {
    jsvalue_to_owned_string(v)
}

#[allow(dead_code)]
fn _force_promise_link(p: *mut Promise) -> i32 {
    unsafe { js_promise_state(p) }
}

#[allow(dead_code)]
fn _force_tag_link() -> u64 {
    TAG_NULL | (POINTER_TAG & PTR_MASK)
}
