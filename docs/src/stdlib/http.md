# HTTP & Networking

Perry natively implements HTTP servers, clients, and WebSocket support.

## Node.js compatibility — `node:http` / `node:https` / `node:http2`

Perry exposes a faithful subset of Node.js's stdlib HTTP server modules
on top of hyper + rustls + tokio-tungstenite. The whole shape — handler
signature, IncomingMessage / ServerResponse properties + methods,
TLS opts, ALPN-negotiated HTTP/2, WebSocket upgrade dispatch — works
unmodified, so unmodified Node servers (Express / Koa / Polka / hono via
`@hono/node-server` / etc.) compile and run natively (issue #577).

### `http.createServer(handler)`

```typescript
{{#include ../../examples/stdlib/http/snippets.ts:node-http-server}}
```

Supported on `IncomingMessage`: `.method`, `.url`, `.headers`,
`.rawHeaders`, `.httpVersion`, `.complete`, `.aborted`, `.destroyed`,
`.socket.remoteAddress`, `.socket.remotePort`, `.on('data'|'end'|'close'|
'error', cb)`, `.read()`, `.pause()`, `.resume()`, `.destroy()`.

Supported on `ServerResponse`: `.statusCode` (get/set),
`.statusMessage` (set), `.setHeader/.getHeader/.removeHeader/.hasHeader/
.getHeaders/.getHeaderNames`, `.headersSent`, `.writableEnded`,
`.writableFinished`, `.writeHead(status, msg?, headers?)`,
`.write(chunk)`, `.end(chunk?)`, `.flushHeaders()`,
`.on('finish'|'close', cb)`. Auto Content-Length on `.end()` when no
`Transfer-Encoding` was set.

### `https.createServer({ key, cert }, handler)`

```typescript
{{#include ../../examples/stdlib/http/snippets.ts:node-https-server}}
```

Both `key` and `cert` are PEM strings (PKCS#8 / RSA / EC keys + multi-cert
chains all parse). ALPN defaults to `http/1.1` only — programs that want
HTTP/2 should reach for `node:http2`'s `createSecureServer` (which always
advertises `[h2, http/1.1]`).

### `http2.createSecureServer({ key, cert }, handler)`

```typescript
{{#include ../../examples/stdlib/http/snippets.ts:node-http2-server}}
```

Driven through `hyper-util`'s `auto::Builder`, so an HTTP/1.1 client
(curl without `--http2`) and an HTTP/2 client (curl with `--http2`)
hit the same handler over the same port.

### WebSocket upgrade — `Server.on('upgrade', (req, wsId, head) => …)`

```typescript
{{#include ../../examples/stdlib/http/snippets.ts:node-http-ws-upgrade}}
```

The HTTP/1.1 server detects `Upgrade: websocket` in the request,
performs the handshake server-side (Sec-WebSocket-Accept derived via
tungstenite's `derive_accept_key`), then registers the upgraded stream
in perry-ext-ws's connection map. The TS-side `wsId` argument is
already a fully-connected client — drive it via the standard
`wsId.on('message', cb)` / `wsId.send(msg)` / `wsId.close()` surface
that standalone `WebSocketServer({ port })` clients use.

## Fastify Server

```typescript
{{#include ../../examples/stdlib/http/snippets.ts:fastify-server}}
```

Perry's Fastify implementation is API-compatible with the npm package. Routes, request/reply objects, params, query strings, and JSON body parsing all work.

## Fetch API

```typescript
{{#include ../../examples/stdlib/http/snippets.ts:fetch-api}}
```

## Axios

```typescript
{{#include ../../examples/stdlib/http/snippets.ts:axios-client}}
```

## WebSocket

```typescript
{{#include ../../examples/stdlib/http/snippets.ts:websocket-client}}
```

## Next Steps

- [Databases](database.md)
- [Overview](overview.md) — All stdlib modules
