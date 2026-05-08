//! `http2.createSecureServer({ key, cert }, handler)` — Phase 3.
//!
//! Implementation strategy: the same `IncomingMessage` /
//! `ServerResponse` types that Phase 1 introduced are reused as
//! `Http2ServerRequest` / `Http2ServerResponse`. hyper's
//! `hyper-util::server::conn::auto::Builder` performs ALPN
//! negotiation on the rustls-wrapped stream, so HTTP/1.1 and HTTP/2
//! coexist on the same port. Phase 1's request-buffering model
//! works unchanged for HTTP/2 streams (each `:path` request becomes
//! a single buffered IncomingMessage).
//!
//! Server push (`response.createPushResponse`) is **not** implemented —
//! the Node.js docs deprecate it and modern frameworks have moved
//! away from it. RFC 8441 (WebSockets over HTTP/2) is also out of
//! scope; the upgrade path stays HTTP/1.1-only.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper::{body::Incoming, Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use perry_ffi::{
    alloc_string, get_handle, get_handle_mut, register_handle, JsClosure, JsValue,
    RawClosureHeader, StringHeader,
};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::TlsAcceptor;

use crate::ensure_gc_scanner_registered;
use crate::request::{
    alloc_incoming_message, emit_no_arg_to_listeners, handle_to_pointer_f64, IncomingMessage,
};
use crate::response::{alloc_server_response, HyperResponseShape};
use crate::server::{synthesize_default_response_if_needed, HttpPendingRequest, HttpServer};
use crate::tls::{build_server_config, parse_cert_chain, parse_private_key};

/// Decode `{ key, cert }` from a NaN-boxed JsValue object. Mirrors
/// the helper in `https_server.rs` but omits the alpnProtocols flag
/// since http2 server always advertises `[h2, http/1.1]`.
unsafe fn parse_h2_opts(opts_f64: f64) -> (String, String) {
    use perry_ffi::JsValue;
    let v = JsValue::from_bits(opts_f64.to_bits());
    if !v.is_pointer() {
        return (String::new(), String::new());
    }
    let json = match perry_ffi::json_stringify(v) {
        Some(j) => j,
        None => return (String::new(), String::new()),
    };
    let parsed: serde_json::Value = match serde_json::from_str(&json) {
        Ok(p) => p,
        Err(_) => return (String::new(), String::new()),
    };
    let key_pem = parsed
        .get("key")
        .and_then(|k| k.as_str())
        .unwrap_or_default()
        .to_string();
    let cert_pem = parsed
        .get("cert")
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string();
    (key_pem, cert_pem)
}
use crate::types::{
    extract_host, extract_port, js_gc_enter_unsafe_zone, js_is_promise, js_promise_run_microtasks,
    js_promise_value, read_string_header, wait_for_promise, Promise, POINTER_TAG, PTR_MASK,
    TAG_UNDEFINED,
};

/// Backing struct for `http2.Http2SecureServer` JS-side handle.
pub struct Http2SecureServer {
    pub handler: i64,
    pub tls_config: Option<Arc<rustls::ServerConfig>>,
    pub base: HttpServer,
}

/// `http2.createSecureServer(opts, handler)` — opts carries `{ key, cert }`
/// PEM strings + the usual handler closure. ALPN advertises both
/// `h2` and `http/1.1` so non-HTTP/2 clients are still served (matches
/// Node's behavior with `allowHTTP1: true`, default in Node 14+).
#[no_mangle]
pub unsafe extern "C" fn js_node_http2_create_secure_server(
    opts_f64: f64,
    handler: i64,
) -> i64 {
    ensure_gc_scanner_registered();

    let (key_pem, cert_pem) = parse_h2_opts(opts_f64);
    let cert_chain = parse_cert_chain(cert_pem.as_bytes());
    let private_key = parse_private_key(key_pem.as_bytes());

    let tls_config = match private_key {
        Some(k) => match build_server_config(cert_chain, k, true) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("[node:http2] {}", e);
                None
            }
        },
        None => {
            eprintln!("[node:http2] no recognized PEM private key");
            None
        }
    };

    register_handle(Http2SecureServer {
        handler,
        tls_config,
        base: HttpServer {
            handler,
            listeners: HashMap::new(),
            bound_port: 0,
            bound_host: String::new(),
            listening: false,
            shutdown_tx: None,
            request_rx: None,
            upgrade_rx: None,
        },
    })
}

/// `http2SecureServer.listen({ port, host? }, cb?)`.
#[no_mangle]
pub unsafe extern "C" fn js_node_http2_server_listen(
    server_handle: i64,
    opts_f64: f64,
    callback: i64,
) {
    let port = extract_port(opts_f64, 443);
    let host = extract_host(opts_f64, "0.0.0.0");

    let (request_tx, request_rx) = mpsc::channel::<HttpPendingRequest>(1024);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

    let tls_config = if let Some(s) = get_handle_mut::<Http2SecureServer>(server_handle) {
        s.base.bound_port = port;
        s.base.bound_host = host.clone();
        s.base.listening = true;
        s.base.shutdown_tx = Some(shutdown_tx);
        s.base.request_rx = Some(request_rx);
        s.tls_config.clone()
    } else {
        return;
    };

    let tls_config = match tls_config {
        Some(c) => c,
        None => {
            eprintln!("[node:http2] tls config unavailable; refusing to listen");
            return;
        }
    };

    js_gc_enter_unsafe_zone();

    let request_tx = Arc::new(request_tx);
    let request_tx_for_spawn = request_tx.clone();
    let host_for_spawn = host.clone();
    let acceptor = TlsAcceptor::from(tls_config);

    // Issue #577 Phase 3 — `tokio::spawn` from inside
    // `spawn_blocking_with_reactor`'s closure panics with
    // "no reactor running" specifically on the http2 binary because
    // the auto::Builder dep set somehow ends up with the ambient
    // tokio runtime context unset by the time the closure runs.
    // Workaround: use `perry_ffi::spawn_blocking` (no reactor) +
    // `Handle::current().block_on` — same pattern perry-ext-fastify
    // uses. The plain spawn_blocking variant runs the closure on a
    // tokio blocking-pool thread that does NOT have a runtime
    // context, so calling `block_on(fut)` is legal there (it spins
    // up a fresh current_thread runtime to drive the future). The
    // I/O reactor IS available because the inner runtime is built
    // with `enable_all`.
    perry_ffi::spawn_blocking(move || {
        let handle = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create http2 accept-loop runtime");
        handle.block_on(async move {
            let bind_str = format!("{}:{}", host_for_spawn, port);
            let addr: SocketAddr = match bind_str.parse() {
                Ok(a) => a,
                Err(_) => SocketAddr::from(([0, 0, 0, 0], port)),
            };
            let listener = match TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!(
                        "[node:http2] bind {}:{} failed: {}",
                        host_for_spawn, port, e
                    );
                    return;
                }
            };
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, peer)) => {
                                let acceptor = acceptor.clone();
                                let request_tx = request_tx_for_spawn.clone();
                                tokio::spawn(async move {
                                    let tls_stream = match acceptor.accept(stream).await {
                                        Ok(s) => s,
                                        Err(e) => {
                                            eprintln!("[node:http2] tls handshake: {}", e);
                                            return;
                                        }
                                    };
                                    let io = TokioIo::new(tls_stream);
                                    let service = service_fn(move |req: Request<Incoming>| {
                                        let request_tx = request_tx.clone();
                                        async move {
                                            handle_h2_request(server_handle, peer, req, request_tx).await
                                        }
                                    });
                                    if let Err(e) = AutoBuilder::new(TokioExecutor::new())
                                        .serve_connection(io, service)
                                        .await
                                    {
                                        let _ = e;
                                    }
                                });
                            }
                            Err(e) => eprintln!("[node:http2] accept error: {}", e),
                        }
                    }
                    _ = &mut shutdown_rx => break,
                }
            }
        });
    });

    let listening_listeners = get_handle::<Http2SecureServer>(server_handle)
        .and_then(|s| s.base.listeners.get("listening").cloned())
        .unwrap_or_default();
    emit_no_arg_to_listeners(&listening_listeners);
    if callback != 0 {
        let raw = callback as *const RawClosureHeader;
        let closure = JsClosure::from_raw(raw);
        if !closure.is_null() {
            let _ = closure.call0();
        }
    }

    eprintln!("[node:http2] listening on https://{}:{}", host, port);

    h2_event_loop(server_handle);
}

async fn handle_h2_request(
    server_handle: i64,
    peer: SocketAddr,
    req: Request<Incoming>,
    request_tx: Arc<mpsc::Sender<HttpPendingRequest>>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let method = req.method().to_string();
    let uri = req.uri();
    let url = match uri.query() {
        Some(q) => format!("{}?{}", uri.path(), q),
        None => uri.path().to_string(),
    };
    let mut headers_lower = HashMap::new();
    let mut raw_headers = Vec::new();
    for (n, v) in req.headers() {
        if let Ok(vs) = v.to_str() {
            headers_lower.insert(n.to_string().to_lowercase(), vs.to_string());
            raw_headers.push((n.to_string(), vs.to_string()));
        }
    }
    let body = match req.collect().await {
        Ok(c) => c.to_bytes().to_vec(),
        Err(_) => Vec::new(),
    };
    let mut im = IncomingMessage::new(
        method,
        url,
        headers_lower,
        raw_headers,
        body,
        peer.ip().to_string(),
        peer.port(),
    );
    im.http_version = "2.0".to_string();
    let im_handle = alloc_incoming_message(im);
    let (response_tx, response_rx) = oneshot::channel::<HyperResponseShape>();
    let sr_handle = alloc_server_response(response_tx);
    let (request_listeners, handler) = match get_handle::<Http2SecureServer>(server_handle) {
        Some(s) => (
            s.base.listeners.get("request").cloned().unwrap_or_default(),
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

fn h2_event_loop(server_handle: i64) {
    loop {
        unsafe {
            js_promise_run_microtasks();
        }
        let pending = match try_recv_pending_h2(server_handle) {
            Some(p) => p,
            None => continue,
        };
        let req_f64 = handle_to_pointer_f64(pending.request_handle);
        let res_f64 = handle_to_pointer_f64(pending.response_handle);
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
            let jsv = JsValue::from_bits(result.to_bits());
            if jsv.is_pointer() {
                let ptr = jsv.as_pointer::<Promise>();
                if !ptr.is_null() && unsafe { js_is_promise(ptr) } != 0 {
                    wait_for_promise(ptr);
                    let _ = unsafe { js_promise_value(ptr) };
                }
            }
        }
        synthesize_default_response_if_needed(pending.response_handle);
        perry_ffi::drop_handle(pending.request_handle);
        perry_ffi::drop_handle(pending.response_handle);
    }
}

fn try_recv_pending_h2(server_handle: i64) -> Option<HttpPendingRequest> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_millis(10);
    loop {
        let result = if let Some(s) = get_handle_mut::<Http2SecureServer>(server_handle) {
            if let Some(rx) = s.base.request_rx.as_mut() {
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

/// `http2SecureServer.address()`.
#[no_mangle]
pub extern "C" fn js_node_http2_server_address_json(handle: i64) -> *mut StringHeader {
    let s = get_handle::<Http2SecureServer>(handle)
        .map(|s| {
            if !s.base.listening {
                "null".to_string()
            } else {
                let family = if s.base.bound_host.contains(':') {
                    "IPv6"
                } else {
                    "IPv4"
                };
                serde_json::json!({
                    "port": s.base.bound_port,
                    "address": s.base.bound_host,
                    "family": family,
                })
                .to_string()
            }
        })
        .unwrap_or_else(|| "null".to_string());
    alloc_string(&s).as_raw()
}

/// `http2SecureServer.close(cb?)`.
#[no_mangle]
pub unsafe extern "C" fn js_node_http2_server_close(handle: i64, callback: i64) {
    let close_listeners;
    if let Some(s) = get_handle_mut::<Http2SecureServer>(handle) {
        s.base.listening = false;
        s.base.shutdown_tx.take();
        close_listeners = s.base.listeners.get("close").cloned().unwrap_or_default();
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

/// `http2SecureServer.on(event, cb)`.
#[no_mangle]
pub unsafe extern "C" fn js_node_http2_server_on(
    handle: i64,
    event_name_ptr: *const StringHeader,
    callback: i64,
) -> f64 {
    let event = read_string_header(event_name_ptr as *mut _).unwrap_or_default();
    if let Some(s) = get_handle_mut::<Http2SecureServer>(handle) {
        s.base.listeners.entry(event).or_default().push(callback);
    }
    f64::from_bits(POINTER_TAG | (handle as u64 & PTR_MASK))
}
