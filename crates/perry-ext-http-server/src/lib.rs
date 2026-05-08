//! Native bindings for Node.js's HTTP server modules — `node:http`,
//! `node:https`, `node:http2` (issue #577).
//!
//! Closes #487 as a side effect by exposing a faithful subset of
//! Node's stdlib so that `@hono/node-server`, Express, Koa, Polka,
//! Fastify-on-node-http, h3, etc. can all run unmodified against
//! perry-compiled programs.
//!
//! # Architecture
//!
//! - `http.createServer(handler)` registers a `HttpServer` handle
//!   carrying the user's handler closure (raw `i64`).
//! - `server.listen({ port, host? }, cb?)` binds, spawns a hyper
//!   accept loop on the perry-ffi blocking pool, and enters the
//!   main-thread event loop.
//! - Each incoming request creates an `IncomingMessage` + `ServerResponse`
//!   handle pair, ships them to the main thread via mpsc, the user's
//!   handler runs synchronously (any returned Promise is awaited),
//!   then the response is flushed back through hyper.
//! - Per-request event listeners (`req.on('data', cb)` / `res.on('finish', cb)`)
//!   are stored as raw `i64` pointers on the IncomingMessage /
//!   ServerResponse handles. A GC root scanner pins them across
//!   malloc-triggered sweeps (issue #35 pattern, copied from
//!   perry-ext-fastify).
//!
//! # Modules
//!
//! - `types` — shared NaN-boxing tags, runtime extern declarations,
//!   port/host extraction helpers, body-shape helpers.
//! - `server` — `HttpServer` handle + accept loop + handler dispatch.
//! - `request` — `IncomingMessage` handle + Readable-stream surface.
//! - `response` — `ServerResponse` handle + Writable-stream surface.
//! - `tls` — Phase 2: rustls config loader + ServerConfig builder.
//! - `https_server` — Phase 2: `https.createServer(opts, handler)`
//!   wired to a TLS-wrapped accept loop.
//! - `http2_server` — Phase 3: `http2.createSecureServer` on hyper's
//!   HTTP/2 builder with ALPN negotiation.
//! - `upgrade` — Phase 4: `Server.on('upgrade', ...)` dispatch +
//!   the `tokio-tungstenite` integration that lets `ws`'s
//!   `WebSocketServer({ server })` pattern work.
//!
//! # Punted gaps
//!
//! - **Cluster module** (`node:cluster`) — out of scope per #577.
//! - **HTTP/3 / QUIC** — out of scope.
//! - **Server push (HTTP/2)** — deprioritized; modern frameworks
//!   have moved away from it.
//! - **HTTP/2 WebSocket (RFC 8441)** — separate consideration; may
//!   defer.

use std::sync::Once;

use perry_ffi::{gc_register_root_scanner, iter_handles_of};

mod types;
mod server;
mod request;
mod response;
mod tls;
mod https_server;
mod http2_server;
mod upgrade;

pub use server::*;
pub use request::*;
pub use response::*;
pub use https_server::*;
pub use http2_server::*;

const POINTER_TAG: u64 = 0x7FFD_0000_0000_0000;
const PTR_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

// ============================================================================
// GC root scanner
// ============================================================================

static GC_REGISTERED: Once = Once::new();

/// Register the http-server GC root scanner exactly once. User
/// closures (request handler, per-request event listeners on
/// IncomingMessage / ServerResponse, server-level event listeners)
/// are stored as raw `i64` pointers inside the various server
/// handles. Without this scanner, a malloc-triggered GC between
/// closure registration and callback dispatch would sweep them —
/// same root cause as issue #35 for net.Socket listeners.
pub(crate) fn ensure_gc_scanner_registered() {
    GC_REGISTERED.call_once(|| {
        gc_register_root_scanner(scan_http_server_roots);
    });
}

/// GC root scanner — walk every registered server / request /
/// response handle and mark every closure pointer they've stashed.
fn scan_http_server_roots(mark: &mut dyn FnMut(f64)) {
    let mark_cb = |cb: i64, m: &mut dyn FnMut(f64)| {
        if cb != 0 {
            let boxed = f64::from_bits(POINTER_TAG | (cb as u64 & PTR_MASK));
            m(boxed);
        }
    };

    iter_handles_of::<HttpServer, _>(|s| {
        mark_cb(s.handler, mark);
        for listeners in s.listeners.values() {
            for cb in listeners {
                mark_cb(*cb, mark);
            }
        }
    });
    iter_handles_of::<IncomingMessage, _>(|im| {
        for listeners in im.listeners.values() {
            for cb in listeners {
                mark_cb(*cb, mark);
            }
        }
    });
    iter_handles_of::<ServerResponse, _>(|sr| {
        for listeners in sr.listeners.values() {
            for cb in listeners {
                mark_cb(*cb, mark);
            }
        }
    });
}
