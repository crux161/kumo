#!/usr/bin/env python3
"""The NET12 isolated DNS server (UDP and TCP) and its witness transcript.

Runs unprivileged on high ports so the host's resolver and any system service on 53 are
untouched. Every name has one fixed behavior, so the campaign's phases are keyed on the
client's own queries:

  kumo.test    an A record for 192.168.0.77 with a 3 s TTL (the campaign must therefore
               issue a second query after the TTL lapses; the immediate second lookup is
               a cache hit and must NOT produce a query)
  nope.test    NXDOMAIN (the negative answer is cached; one query total)
  slow.test    no reply at all (the resolver's bounded timeout)
  big.test     a 600-byte reply (refused by the 512-byte policy)
  reject.test  three bad replies first (wrong transaction id, wrong source port, a
               truncated header, and a self-referential compression pointer), then the
               valid A record: the resolver must ignore the bad ones and accept the last
  trunc.test   a UDP reply with TC set and no answers; the full A answer is served over
               TCP (length-prefixed), which under fallback policy the resolver must fetch

Mode `tcp` expects the TCP fallback connection to arrive (policy on); mode `notcp`
expects it to be refused (policy off, MessageTooLarge).

Usage: scripts/net12-dns.py tcp|notcp [--port 6801] [--tcp-port 6802]
       [--bad-source-port 6809] [--wait 240] [--addr 192.168.0.77]
Exit: 0 PASS, 1 FAIL.
"""
import argparse
import socket
import struct
import sys
import threading
import time

A_ADDR = '192.168.0.77'
# NET15: the A record for kumo.test must name the host that actually serves the
# TCP duplex, so the address is overridable on the command line.

A_TTL = 3
FLAGS_OK = 0x8180
FLAG_TC = 0x0200
RCODE_NX = 0x0003


def parse_query(pkt):
    if len(pkt) < 12:
        return None
    txid = struct.unpack_from('!H', pkt, 0)[0]
    if struct.unpack_from('!H', pkt, 4)[0] != 1:
        return None
    i = 12
    labels = []
    while i < len(pkt):
        ln = pkt[i]
        if ln == 0:
            i += 1
            break
        if ln & 0xC0:
            return None
        labels.append(pkt[i + 1:i + 1 + ln])
        i += 1 + ln
    if i + 4 > len(pkt):
        return None
    qtype, _qclass = struct.unpack_from('!HH', pkt, i)
    return {
        'txid': txid,
        'name': b'.'.join(labels).decode('latin-1'),
        'qtype': qtype,
        'question': pkt[12:i + 4],
    }


def reply(txid, question, flags=FLAGS_OK, ancount=0, answers=b''):
    return struct.pack('!HHHHHH', txid, flags, 1, ancount, 0, 0) + question + answers


def a_answer(ttl=A_TTL):
    return (b'\xc0\x0c' + struct.pack('!HHIH', 1, 1, ttl, 4)
            + socket.inet_aton(A_ADDR))


def main():
    global A_ADDR
    ap = argparse.ArgumentParser()
    ap.add_argument('mode', choices=['tcp', 'notcp'])
    ap.add_argument('--port', type=int, default=6801)
    ap.add_argument('--tcp-port', type=int, default=6802)
    ap.add_argument('--bad-source-port', type=int, default=6809)
    ap.add_argument('--wait', type=float, default=240.0)
    ap.add_argument('--addr', default=A_ADDR)
    args = ap.parse_args()
    A_ADDR = args.addr

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(('', args.port))
    sock.settimeout(0.2)
    bad = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    bad.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    bad.bind(('', args.bad_source_port))

    # Count distinct transaction ids per name: a retransmission of the same query is the
    # same lookup, not a new one.
    query_ids = {k: set() for k in ('kumo', 'nope', 'slow', 'big', 'reject', 'trunc_udp')}
    trunc_tcp = [0]
    seen_lock = threading.Lock()

    def bump(key, txid):
        with seen_lock:
            query_ids[key].add(txid)

    def trunc_tcp_serve():
        ls = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        ls.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        ls.bind(('', args.tcp_port))
        ls.listen(2)
        ls.settimeout(0.5)
        deadline = time.time() + args.wait
        while time.time() < deadline:
            try:
                conn, src = ls.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            conn.settimeout(5.0)
            try:
                hdr = b''
                while len(hdr) < 2:
                    chunk = conn.recv(2 - len(hdr))
                    if not chunk:
                        break
                    hdr += chunk
                if len(hdr) == 2:
                    want = struct.unpack('!H', hdr)[0]
                    body = b''
                    while len(body) < want:
                        chunk = conn.recv(want - len(body))
                        if not chunk:
                            break
                        body += chunk
                    q = parse_query(body)
                    if q is not None:
                        with seen_lock:
                            trunc_tcp[0] += 1
                        print('net12-dns: query trunc.test tcp id=0x%04x from=%s'
                              % (q['txid'], src[0]), flush=True)
                        full = reply(q['txid'], q['question'], FLAGS_OK, 1,
                                     a_answer(ttl=30))
                        conn.sendall(struct.pack('!H', len(full)) + full)
            except OSError:
                pass
            finally:
                conn.close()
        ls.close()

    threading.Thread(target=trunc_tcp_serve, daemon=True).start()

    print('net12-dns: listening udp=%d tcp=%d mode=%s bad-source=%d'
          % (args.port, args.tcp_port, args.mode, args.bad_source_port), flush=True)
    deadline = time.time() + args.wait
    while time.time() < deadline:
        try:
            data, src = sock.recvfrom(700)
        except socket.timeout:
            continue
        except OSError:
            break
        q = parse_query(data)
        if q is None or q['qtype'] != 1:
            continue
        name = q['name']
        print('net12-dns: query %s udp id=0x%04x from=%s'
              % (name, q['txid'], src[0]), flush=True)
        txid, question = q['txid'], q['question']
        if name == 'kumo.test':
            bump('kumo', txid)
            sock.sendto(reply(txid, question, FLAGS_OK, 1, a_answer()), src)
        elif name == 'nope.test':
            bump('nope', txid)
            sock.sendto(reply(txid, question, FLAGS_OK | RCODE_NX, 0), src)
        elif name == 'slow.test':
            bump('slow', txid)
        elif name == 'big.test':
            bump('big', txid)
            base = reply(txid, question, FLAGS_OK, 1, a_answer())
            sock.sendto(base + b'\x00' * (600 - len(base)), src)
        elif name == 'reject.test':
            bump('reject', txid)
            bad_id = reply((txid + 1) & 0xFFFF, question, FLAGS_OK, 1, a_answer())
            sock.sendto(bad_id, src)
            print('net12-dns: sent a wrong-id reply (must be ignored)', flush=True)
            good = reply(txid, question, FLAGS_OK, 1, a_answer())
            bad.sendto(good, src)
            print('net12-dns: sent a wrong-source reply (must be ignored)', flush=True)
            sock.sendto(good[:10], src)
            print('net12-dns: sent a truncated header (must be ignored)', flush=True)
            looped = reply(txid, b'\xc0\x0c\x00\x01\x00\x01', FLAGS_OK, 0)
            sock.sendto(looped, src)
            print('net12-dns: sent a pointer-loop reply (must be ignored)', flush=True)
            sock.sendto(good, src)
            print('net12-dns: sent the valid A reply', flush=True)
        elif name == 'trunc.test':
            bump('trunc_udp', txid)
            sock.sendto(reply(txid, question, FLAGS_OK | FLAG_TC, 0), src)
        else:
            sock.sendto(reply(txid, question, FLAGS_OK | RCODE_NX, 0), src)

    # Give the TCP thread a moment to observe a late fallback connection.
    time.sleep(0.5)
    phases = {
        'a-lookup': len(query_ids['kumo']) >= 1,
        'cache-hit-no-requery': len(query_ids['kumo']) == 2,
        'negative': len(query_ids['nope']) == 1,
        'timeout-phase': len(query_ids['slow']) >= 1,
        'oversized': len(query_ids['big']) >= 1,
        'reject-sequence': len(query_ids['reject']) == 1,
        'truncation-udp': len(query_ids['trunc_udp']) == 1,
    }
    if args.mode == 'tcp':
        phases['tcp-fallback'] = trunc_tcp[0] == 1
    else:
        phases['tcp-refused'] = trunc_tcp[0] == 0
    ok = all(phases.values())
    for name, fine in phases.items():
        print('net12-dns: %s %s' % (name, 'PASS' if fine else 'FAIL'), flush=True)
    summary = {k: len(v) for k, v in query_ids.items()}
    summary['trunc_tcp'] = trunc_tcp[0]
    print('net12-dns: distinct queries %s' % summary, flush=True)
    print('net12-dns: PASS' if ok else 'net12-dns: FAIL', flush=True)
    return 0 if ok else 1


if __name__ == '__main__':
    sys.exit(main())
