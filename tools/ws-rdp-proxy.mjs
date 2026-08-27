#!/usr/bin/env node
/**
 * WebSocket-to-TCP relay for IronRDP-web (direct mode).
 *
 * This used to terminate TLS on the browser's behalf and hand back the server's
 * public key so CredSSP had something to bind to. The wasm client now performs the
 * TLS handshake itself, so nothing here inspects, rewrites, or understands a single
 * byte of what passes through: WebSocket in, TCP out, both directions.
 *
 * That also means the two RDP orderings this file used to special-case — plain
 * (X.224 → TLS) and Hyper-V VMConnect (PCB → TLS, port 2179) — are now identical
 * from the relay's point of view, and there is no `?vmconnect=1` mode any more.
 *
 * Usage: node ws-rdp-proxy.mjs [listen-port] [rdp-target-host:port]
 * Example: node ws-rdp-proxy.mjs 8765 192.168.1.100:3389
 * Example (VMConnect): node ws-rdp-proxy.mjs 8765 hyperv-host.example.com:2179
 */

import { WebSocketServer } from 'ws';
import * as net from 'net';

const listenPort = parseInt(process.argv[2] || '8765', 10);
const targetAddr = process.argv[3] || 'localhost:3389';
const [targetHost, targetPortStr] = targetAddr.split(':');
const targetPort = parseInt(targetPortStr || '3389', 10);

const wss = new WebSocketServer({ port: listenPort });
console.log(`[proxy] Listening on ws://localhost:${listenPort}`);
console.log(`[proxy] Target: ${targetHost}:${targetPort}`);

wss.on('connection', (ws) => {
  console.log('[proxy] Browser connected');

  // The browser may start sending before the TCP connection is up (the wasm client
  // writes as soon as the WebSocket is open). Hold those frames rather than dropping
  // them; ordering is preserved because everything goes through this one queue.
  let tcpReady = false;
  const pending = [];

  const tcp = net.createConnection({ host: targetHost, port: targetPort }, () => {
    console.log(`[proxy] TCP connected to ${targetHost}:${targetPort}`);
    tcpReady = true;
    for (const chunk of pending) {
      tcp.write(chunk);
    }
    pending.length = 0;
  });

  tcp.on('data', (data) => {
    if (ws.readyState === ws.OPEN) {
      ws.send(data);
    }
  });

  tcp.on('error', (err) => {
    console.error('[proxy] TCP error:', err.message);
    ws.close(1011, 'TCP error');
  });

  tcp.on('close', () => {
    console.log('[proxy] TCP closed');
    if (ws.readyState === ws.OPEN) {
      ws.close(1000, 'Target closed');
    }
  });

  ws.on('message', (data) => {
    const chunk = Buffer.isBuffer(data) ? data : Buffer.from(data);
    if (tcpReady) {
      tcp.write(chunk);
    } else {
      pending.push(chunk);
    }
  });

  ws.on('close', () => {
    console.log('[proxy] Browser disconnected');
    tcp.destroy();
  });

  ws.on('error', (err) => {
    console.error('[proxy] WebSocket error:', err.message);
    tcp.destroy();
  });
});
