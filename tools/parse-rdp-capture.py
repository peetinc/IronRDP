#!/usr/bin/env python3
"""Parse ws-rdp-proxy RDP_CAPTURE files and decode client->server RDPDR channel traffic.

Record format (see ws-rdp-proxy.mjs):
  [dir u8: 0=c->s, 1=s->c][ts_ms f64 LE][len u32 LE][payload only when dir==0]

Client->server payloads are a TPKT stream: each WS frame may hold one or more
TPKT PDUs (or fastpath input, first byte != 0x03). For every TPKT carrying an
MCS SendDataRequest we print channel id, and for static virtual channel data we
decode the CHANNEL_PDU_HEADER (length + flags) so multi-chunk RDPDR responses
are visible chunk by chunk.

Usage: parse-rdp-capture.py <capture-file> [--channel <id>] [--from-ts <ms>]
"""
import struct
import sys
from datetime import datetime, timezone

CHANNEL_FLAG_FIRST = 0x01
CHANNEL_FLAG_LAST = 0x02


def flags_str(flags: int) -> str:
    parts = []
    if flags & CHANNEL_FLAG_FIRST:
        parts.append("FIRST")
    if flags & CHANNEL_FLAG_LAST:
        parts.append("LAST")
    if flags & 0x10:
        parts.append("SHOW_PROTOCOL")
    rest = flags & ~0x13
    if rest:
        parts.append(hex(rest))
    return "|".join(parts) or "0"


def ts_str(ts_ms: float) -> str:
    return datetime.fromtimestamp(ts_ms / 1000, tz=timezone.utc).strftime("%H:%M:%S.%f")[:-3]


def parse_mcs_send_data(tpkt_payload: bytes):
    """Very small MCS parser: x224 data header (3 bytes: 02 f0 80) then MCS.

    MCS SendDataRequest (client->server): choice 0x64 (100 <<2 = 0x19? DomainMCSPDU per-encoded).
    PER: first byte >>2 == 25 for SendDataRequest, 26 for SendDataIndication.
    Layout: [choice][initiator u16][channel u16][dataPriority+seg u8][length PER]
    """
    if len(tpkt_payload) < 3 or tpkt_payload[0] != 0x02:
        return None
    mcs = tpkt_payload[3:]
    if len(mcs) < 7:
        return None
    choice = mcs[0] >> 2
    if choice not in (25, 26):
        return None
    initiator = struct.unpack(">H", mcs[1:3])[0] + 1001
    channel = struct.unpack(">H", mcs[3:5])[0]
    # mcs[5] = dataPriority/segmentation
    i = 6
    b0 = mcs[i]
    if b0 & 0x80:
        if b0 & 0x40:
            return None  # >16k PER length, not expected
        length = ((b0 & 0x3F) << 8) | mcs[i + 1]
        i += 2
    else:
        length = b0
        i += 1
    return channel, initiator, mcs[i : i + length]


def main() -> None:
    path = sys.argv[1]
    want_channel = None
    from_ts = None
    args = sys.argv[2:]
    while args:
        a = args.pop(0)
        if a == "--channel":
            want_channel = int(args.pop(0))
        elif a == "--from-ts":
            from_ts = float(args.pop(0))
    data = open(path, "rb").read()
    off = 0
    pending = b""  # c->s TPKT stream reassembly across WS frames
    last_sc_ts = None
    while off + 13 <= len(data):
        direction = data[off]
        ts_ms = struct.unpack("<d", data[off + 1 : off + 9])[0]
        length = struct.unpack("<I", data[off + 9 : off + 13])[0]
        off += 13
        if direction == 1:
            last_sc_ts = ts_ms
            if from_ts is None or ts_ms >= from_ts:
                print(f"{ts_str(ts_ms)}  s->c  {length} bytes")
            continue
        payload = data[off : off + length]
        off += length
        if from_ts is not None and ts_ms < from_ts:
            continue
        pending += payload
        # Walk TPKTs in the reassembled stream.
        while True:
            if len(pending) < 4:
                break
            if pending[0] != 0x03:
                # Fastpath input event: length in bytes 1..2 (7 or 15 bit)
                b1 = pending[1]
                fp_len = b1 if b1 & 0x80 == 0 else (((b1 & 0x7F) << 8) | pending[2])
                if fp_len == 0 or len(pending) < fp_len:
                    break
                print(f"{ts_str(ts_ms)}  c->s  fastpath {fp_len} bytes")
                pending = pending[fp_len:]
                continue
            tpkt_len = struct.unpack(">H", pending[2:4])[0]
            if len(pending) < tpkt_len:
                break
            tpkt = pending[:tpkt_len]
            pending = pending[tpkt_len:]
            parsed = parse_mcs_send_data(tpkt[4:])
            if parsed is None:
                print(f"{ts_str(ts_ms)}  c->s  tpkt {tpkt_len} bytes (non-MCS-data)")
                continue
            channel, initiator, vc = parsed
            if want_channel is not None and channel != want_channel:
                continue
            if len(vc) >= 8:
                total_len, flags = struct.unpack("<II", vc[:8])
                body = vc[8:]
                head = body[:16].hex()
                print(
                    f"{ts_str(ts_ms)}  c->s  ch={channel} chunk data={len(body)}B"
                    f" hdr.total={total_len} flags={flags_str(flags)} head={head}"
                )
            else:
                print(f"{ts_str(ts_ms)}  c->s  ch={channel} short vc payload {len(vc)}B")
    print("EOF", f"last s->c at {ts_str(last_sc_ts)}" if last_sc_ts else "")


if __name__ == "__main__":
    main()
