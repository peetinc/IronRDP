#!/usr/bin/env node
/**
 * Simple WebSocket-to-RDP TLS-terminating proxy for IronRDP-web (direct mode).
 *
 * Protocol:
 * 1. Browser connects via WebSocket
 * 2. Browser sends X.224 Connection Request (raw RDP PDU)
 * 3. Proxy relays it to the RDP target over TCP
 * 4. RDP target responds with X.224 Connection Confirm
 * 5. Proxy relays the confirm back to the browser
 * 6. Proxy upgrades the TCP connection to TLS
 * 7. Proxy sends the server's SubjectPublicKeyInfo (DER) to the browser:
 *    [0x00][len:4 BE][spki_der_bytes]
 * 8. All subsequent bytes are relayed transparently
 *
 * Usage: node ws-rdp-proxy.mjs [listen-port] [rdp-target-host:port]
 * Example: node ws-rdp-proxy.mjs 8765 192.168.1.100:3389
 */

import { WebSocketServer } from 'ws';
import * as net from 'net';
import * as tls from 'tls';
import * as crypto from 'crypto';

const listenPort = parseInt(process.argv[2] || '8765', 10);
const targetAddr = process.argv[3] || 'localhost:3389';
const [targetHost, targetPortStr] = targetAddr.split(':');
const targetPort = parseInt(targetPortStr || '3389', 10);

const wss = new WebSocketServer({ port: listenPort });
console.log(`[proxy] Listening on ws://localhost:${listenPort}`);
console.log(`[proxy] Target: ${targetHost}:${targetPort}`);

wss.on('connection', (ws) => {
  console.log('[proxy] Browser connected');

  let tcpSocket = null;
  let tlsSocket = null;
  let activeSocket = null;
  let tlsUpgraded = false;
  let x224Phase = true; // true until we see X.224 confirm and need TLS upgrade
  let tcpReady = false;
  let pendingMessages = []; // buffer messages until TCP is connected

  // Track how many X.224 PDUs we've relayed
  let x224RequestSent = false;

  // Connect to RDP target
  tcpSocket = net.createConnection({ host: targetHost, port: targetPort }, () => {
    console.log(`[proxy] Connected to RDP target ${targetHost}:${targetPort}`);
    activeSocket = tcpSocket;
    tcpReady = true;
    // Flush any messages that arrived before TCP was ready
    for (const msg of pendingMessages) {
      activeSocket.write(msg);
    }
    pendingMessages = [];
  });

  tcpSocket.on('error', (err) => {
    console.error('[proxy] TCP error:', err.message);
    ws.close();
  });

  tcpSocket.on('close', () => {
    console.log('[proxy] TCP connection closed');
    ws.close();
  });

  // Buffer for TCP data during X.224 phase
  let tcpBuffer = Buffer.alloc(0);

  tcpSocket.on('data', (data) => {
    if (tlsUpgraded) {
      // Should not get data on raw tcp after TLS upgrade
      return;
    }

    tcpBuffer = Buffer.concat([tcpBuffer, data]);

    // During X.224 phase, wait for complete TPKT packet
    // TPKT header: [version:1][reserved:1][length:2 BE]
    while (tcpBuffer.length >= 4) {
      const tpktLen = tcpBuffer.readUInt16BE(2);
      if (tcpBuffer.length < tpktLen) break;

      const pdu = tcpBuffer.subarray(0, tpktLen);
      tcpBuffer = tcpBuffer.subarray(tpktLen);

      if (x224Phase && x224RequestSent) {
        // This should be the X.224 Connection Confirm
        console.log(`[proxy] Got X.224 response (${pdu.length} bytes), relaying to browser`);
        ws.send(pdu);

        // Now upgrade to TLS
        x224Phase = false;
        performTlsUpgrade();
      } else {
        // Relay any other PDU
        ws.send(pdu);
      }
    }
  });

  function performTlsUpgrade() {
    console.log('[proxy] Upgrading TCP to TLS...');
    tlsUpgraded = true;

    tlsSocket = tls.connect({
      socket: tcpSocket,
      rejectUnauthorized: false, // RDP servers typically use self-signed certs
      // Don't set servername for IP addresses (Node.js rejects it)
      ...(net.isIP(targetHost) ? {} : { servername: targetHost }),
    }, () => {
      console.log('[proxy] TLS upgrade complete');
      activeSocket = tlsSocket;

      // Extract server's SubjectPublicKeyInfo (DER)
      const cert = tlsSocket.getPeerCertificate(true);
      if (!cert || !cert.raw) {
        console.error('[proxy] No server certificate available');
        ws.close();
        return;
      }

      // Parse the DER certificate to extract SubjectPublicKeyInfo
      const spkiDer = extractSPKI(cert.raw);
      if (!spkiDer) {
        console.error('[proxy] Failed to extract SubjectPublicKeyInfo from cert');
        ws.close();
        return;
      }

      console.log(`[proxy] Sending server public key (${spkiDer.length} bytes) to browser`);

      // Send tagged message: [0x00][len:4 BE][spki_der]
      const keyMsg = Buffer.alloc(5 + spkiDer.length);
      keyMsg[0] = 0x00;
      keyMsg.writeUInt32BE(spkiDer.length, 1);
      spkiDer.copy(keyMsg, 5);
      ws.send(keyMsg);

      // Now relay all TLS data to browser
      tlsSocket.on('data', (data) => {
        if (ws.readyState === ws.OPEN) {
          ws.send(data);
        }
      });
    });

    tlsSocket.on('error', (err) => {
      console.error('[proxy] TLS error:', err.message);
      ws.close();
    });

    tlsSocket.on('close', () => {
      console.log('[proxy] TLS connection closed');
      ws.close();
    });
  }

  // Browser -> RDP target
  ws.on('message', (data) => {
    const buf = Buffer.from(data);

    if (x224Phase && !x224RequestSent) {
      // First message from browser should be X.224 Connection Request
      console.log(`[proxy] Got X.224 request from browser (${buf.length} bytes)`);
      x224RequestSent = true;
    }

    if (tcpReady && activeSocket && !activeSocket.destroyed) {
      activeSocket.write(buf);
    } else {
      // Buffer until TCP is connected
      pendingMessages.push(buf);
    }
  });

  ws.on('close', () => {
    console.log('[proxy] Browser disconnected');
    if (tlsSocket) tlsSocket.destroy();
    else if (tcpSocket) tcpSocket.destroy();
  });

  ws.on('error', (err) => {
    console.error('[proxy] WebSocket error:', err.message);
  });
});

/**
 * Extract SubjectPublicKeyInfo (DER) from a DER-encoded X.509 certificate.
 * This is a minimal ASN.1 parser — just enough to find the SPKI.
 */
function extractSPKI(certDer) {
  try {
    const x509 = new crypto.X509Certificate(certDer);
    // Export as DER SPKI, then extract just the raw public key BIT STRING value.
    // SPKI structure: SEQUENCE { SEQUENCE { algorithm }, BIT STRING { raw key } }
    // IronRDP expects the raw public key bytes (BIT STRING contents), not the full SPKI.
    const spkiDer = x509.publicKey.export({ type: 'spki', format: 'der' });
    return extractBitStringFromSPKI(spkiDer);
  } catch (err) {
    console.error('[proxy] SPKI extraction error:', err.message);
    return null;
  }
}

/**
 * Extract the raw BIT STRING value from a DER-encoded SubjectPublicKeyInfo.
 * SPKI = SEQUENCE { AlgorithmIdentifier, BIT STRING }
 * We need the BIT STRING contents (skip the unused-bits byte).
 */
function extractBitStringFromSPKI(spki) {
  let offset = 0;

  // Outer SEQUENCE
  if (spki[offset] !== 0x30) throw new Error('Expected SEQUENCE');
  offset++;
  const [seqLen, seqLenBytes] = readDERLength(spki, offset);
  offset += seqLenBytes;

  // AlgorithmIdentifier SEQUENCE — skip it
  if (spki[offset] !== 0x30) throw new Error('Expected AlgorithmIdentifier SEQUENCE');
  offset++;
  const [algoLen, algoLenBytes] = readDERLength(spki, offset);
  offset += algoLenBytes + algoLen;

  // BIT STRING
  if (spki[offset] !== 0x03) throw new Error('Expected BIT STRING');
  offset++;
  const [bitLen, bitLenBytes] = readDERLength(spki, offset);
  offset += bitLenBytes;

  // Skip the unused-bits byte (should be 0x00)
  const unusedBits = spki[offset];
  offset++;

  // Raw public key bytes
  return spki.subarray(offset, offset + bitLen - 1);
}

function readDERLength(buf, offset) {
  const first = buf[offset];
  if (first < 0x80) {
    return [first, 1];
  }
  const numBytes = first & 0x7f;
  let len = 0;
  for (let i = 0; i < numBytes; i++) {
    len = (len << 8) | buf[offset + 1 + i];
  }
  return [len, 1 + numBytes];
}
