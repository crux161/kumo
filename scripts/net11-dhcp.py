#!/usr/bin/env python3
"""The NET11 isolated DHCP server and echo witness.

Runs on the Mac as an ordinary user on non-privileged ports, so it never touches the
host's DHCP or the firmware's PXE exchange on 67/68. It drives the whole lease lifecycle
the NET11 bar asks for, keyed on the client's own messages (no wall-clock coupling):

  1. silent: ignore the first --silent DISCOVERs. The client must retry on its own
     bounded schedule and never wedge; netprobe sees address 0.0.0.0 throughout.
  2. mismatch: answer the next DISCOVER with a wrong-xid OFFER, a wrong-chaddr OFFER,
     and an OFFER missing the server identifier. A REQUEST before the valid OFFER is a
     named violation; the client must ignore all three.
  3. grant: the valid OFFER, then ACK the REQUEST. netprobe's GETNAME now reports the
     leased address and UDP echoes flow through it.
  4. renew: ACK exactly one renewal REQUEST, then go silent.
  5. expiry: silence long enough for the lease to lapse; the client's own re-DISCOVER
     proves it noticed, and netprobe logs the address gone.
  6. regrant: grant a fresh lease to the re-DISCOVER, proving recovery.

The echo endpoint (--echo-port) returns datagrams verbatim and records their source,
which is the leased address while the lease is live.

Usage: scripts/net11-dhcp.py [--port 6767] [--client-port 6868] [--echo-port 7702]
       [--offer-ip 192.168.0.150] [--lease 20] [--t1 10] [--t2 17]
       [--silent 2] [--wait 300]
Exit: 0 PASS, 1 FAIL.
"""
import argparse
import socket
import struct
import sys
import threading
import time

MAGIC = b'\x63\x82\x53\x63'
BOOTREPLY = 2
DISCOVER, OFFER, REQUEST, ACK = 1, 2, 3, 5


def parse(pkt):
    if len(pkt) < 240:
        return None
    if pkt[0] != 1 or pkt[1] != 1 or pkt[2] != 6:
        return None
    if pkt[236:240] != MAGIC:
        return None
    xid = struct.unpack_from('!I', pkt, 4)[0]
    chaddr = pkt[28:34]
    kind = None
    i = 240
    while i < len(pkt):
        code = pkt[i]
        if code == 255:
            break
        if code == 0:
            i += 1
            continue
        if i + 1 >= len(pkt):
            break
        ln = pkt[i + 1]
        if code == 53 and ln == 1:
            kind = pkt[i + 2]
        i += 2 + ln
    return {'xid': xid, 'chaddr': chaddr, 'type': kind}


def reply(kind, xid, chaddr, yiaddr, server, lease, t1, t2, mask, router,
          server_id=True):
    pkt = bytearray(240)
    pkt[0] = BOOTREPLY
    pkt[1] = 1
    pkt[2] = 6
    struct.pack_into('!I', pkt, 4, xid)
    struct.pack_into('!H', pkt, 8, 0)   # secs
    struct.pack_into('!H', pkt, 10, 0)  # flags: unicast replies are fine; we send broadcast
    pkt[16:20] = yiaddr
    pkt[20:24] = server  # siaddr
    pkt[28:34] = chaddr
    pkt[236:240] = MAGIC
    opts = bytearray()
    opts += bytes([53, 1, kind])
    if server_id:
        opts += bytes([54, 4]) + server
    opts += bytes([51, 4]) + struct.pack('!I', lease)
    if t1:
        opts += bytes([58, 4]) + struct.pack('!I', t1)
    if t2:
        opts += bytes([59, 4]) + struct.pack('!I', t2)
    opts += bytes([1, 4]) + mask
    opts += bytes([3, 4]) + router
    opts += bytes([255])
    return bytes(pkt) + bytes(opts)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--port', type=int, default=6767)
    ap.add_argument('--client-port', type=int, default=6868)
    ap.add_argument('--echo-port', type=int, default=7702)
    ap.add_argument('--offer-ip', default='192.168.0.150')
    ap.add_argument('--lease', type=int, default=20)
    ap.add_argument('--t1', type=int, default=10)
    ap.add_argument('--t2', type=int, default=17)
    ap.add_argument('--silent', type=int, default=2)
    ap.add_argument('--wait', type=float, default=300.0)
    ap.add_argument('--echo-only', action='store_true')
    args = ap.parse_args()

    # The server identity must be the address the client sees as the peer's. Route lookup
    # without sending anything: connect a UDP socket and read its source address.
    probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    probe.connect((args.offer_ip, 9))
    server_ip = probe.getsockname()[0]
    probe.close()
    server = socket.inet_aton(server_ip)
    offer = socket.inet_aton(args.offer_ip)
    mask = socket.inet_aton('255.255.255.0')
    router = server

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    sock.bind(('', args.port))
    sock.settimeout(0.2)

    echo_stop = threading.Event()
    echoes = [0]
    echo_first = [None]
    echo_last = [None]

    def echo_loop():
        es = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        es.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        es.bind(('', args.echo_port))
        es.settimeout(0.2)
        while not echo_stop.is_set():
            try:
                data, src = es.recvfrom(2048)
            except socket.timeout:
                continue
            except OSError:
                break
            if echo_first[0] is None:
                echo_first[0] = src
            echo_last[0] = src
            echoes[0] += 1
            es.sendto(data, src)
        es.close()

    threading.Thread(target=echo_loop, daemon=True).start()

    if args.echo_only:
        # The static-regression witness: no DHCP client will appear, so serve echoes
        # and report only that. The DHCP lifecycle is proven by the DHCP boots.
        print('net11-dhcp: echo-only mode (no DHCP phases negotiated)', flush=True)
        deadline = time.time() + args.wait
        while time.time() < deadline:
            time.sleep(1.0)
        echo_stop.set()
        time.sleep(0.3)
        print('net11-dhcp: echo-only served %d datagrams' % echoes[0], flush=True)
        print('net11-dhcp: PASS' if echoes[0] > 0 else 'net11-dhcp: FAIL', flush=True)
        return 0 if echoes[0] > 0 else 1

    state = {
        'phase': 'silent',
        'silent_seen': 0,
        'early_request': False,
        'granted': False,
        'renewed': False,
        'expired': False,
        'regranted': False,
        'quiet_at': None,
        'mismatch_deadline': None,
        'done_at': None,
    }
    pending = {}

    def broadcast(payload):
        sock.sendto(payload, ('255.255.255.255', args.client_port))

    def offer_for(msg):
        return reply(OFFER, msg['xid'], msg['chaddr'], offer, server,
                     args.lease, args.t1, args.t2, mask, router)

    def ack_for(msg):
        return reply(ACK, msg['xid'], msg['chaddr'], offer, server,
                     args.lease, args.t1, args.t2, mask, router)

    deadline = time.time() + args.wait
    print('net11-dhcp: listening server_port=%d client_port=%d echo_port=%d '
          'offer=%s server=%s lease=%ds'
          % (args.port, args.client_port, args.echo_port, args.offer_ip,
             server_ip, args.lease), flush=True)
    while time.time() < deadline:
        now = time.time()
        # The mismatch window: a REQUEST before the valid OFFER is a violation.
        if (state['phase'] == 'mismatch'
                and state['mismatch_deadline'] is not None
                and now >= state['mismatch_deadline']):
            broadcast(offer_for(pending['last']))
            broadcast(offer_for(pending['last']))
            state['mismatch_deadline'] = None
            state['phase'] = 'offered'
            print('net11-dhcp: sent the valid OFFER (mismatched replies before it)',
                  flush=True)
        try:
            data, src = sock.recvfrom(2048)
        except socket.timeout:
            continue
        except OSError:
            break
        msg = parse(data)
        if msg is None or msg['type'] is None:
            continue
        print('net11-dhcp: recv type=%d xid=0x%08x chaddr=%s phase=%s'
              % (msg['type'], msg['xid'], msg['chaddr'].hex(), state['phase']),
              flush=True)
        if msg['type'] == DISCOVER:
            if state['phase'] == 'silent':
                state['silent_seen'] += 1
                if state['silent_seen'] <= args.silent:
                    print('net11-dhcp: DISCOVER %d ignored (no-server window)'
                          % state['silent_seen'], flush=True)
                    continue
                # Mismatch batch: wrong xid, wrong chaddr, missing server id.
                bad = dict(msg)
                bad['xid'] = (msg['xid'] + 1) & 0xFFFFFFFF
                broadcast(offer_for(bad))
                print('net11-dhcp: sent an OFFER with a wrong xid (must be ignored)',
                      flush=True)
                bad2 = dict(msg)
                bad2['chaddr'] = bytes([0x02, 0, 0, 0, 0, 0x99])
                broadcast(offer_for(bad2))
                print('net11-dhcp: sent an OFFER with a wrong chaddr (must be ignored)',
                      flush=True)
                broadcast(reply(OFFER, msg['xid'], msg['chaddr'], offer, server,
                                args.lease, args.t1, args.t2, mask, router,
                                server_id=False))
                print('net11-dhcp: sent an OFFER without a server id (must be ignored)',
                      flush=True)
                pending['last'] = msg
                state['phase'] = 'mismatch'
                state['mismatch_deadline'] = now + 2.0
            elif state['phase'] == 'quiet':
                state['expired'] = True
                broadcast(offer_for(msg))
                state['phase'] = 'offered'
                print('net11-dhcp: client re-DISCOVERed after expiry; offering a '
                      'fresh lease', flush=True)
            elif state['phase'] in ('offered', 'mismatch'):
                # The client is asking again while an offer should be pending: the
                # offer or its REQUEST was lost, or another client appeared. Offer to
                # this DISCOVER (twice, so one lost broadcast cannot wedge the run).
                pending['last'] = msg
                if state['phase'] == 'mismatch':
                    state['mismatch_deadline'] = None
                broadcast(offer_for(msg))
                broadcast(offer_for(msg))
                state['phase'] = 'offered'
                print('net11-dhcp: re-offered while the phase was open (offer may '
                      'have been lost)', flush=True)
        elif msg['type'] == REQUEST:
            if state['phase'] == 'mismatch':
                state['early_request'] = True
                print('net11-dhcp: VIOLATION: REQUEST before the valid OFFER',
                      flush=True)
            elif state['phase'] == 'offered':
                broadcast(ack_for(msg))
                if state['granted']:
                    state['regranted'] = True
                    print('net11-dhcp: ACKed the fresh lease after expiry', flush=True)
                else:
                    state['granted'] = True
                    print('net11-dhcp: ACKed the lease %s for %ds'
                          % (args.offer_ip, args.lease), flush=True)
                state['phase'] = 'granted'
            elif state['phase'] == 'granted':
                if not state['renewed']:
                    broadcast(ack_for(msg))
                    state['renewed'] = True
                    state['quiet_at'] = now
                    print('net11-dhcp: ACKed one renewal; going silent', flush=True)
                else:
                    pass
            elif state['phase'] == 'silent_quiet':
                pass
        # After the renewal ACK, stop answering: the lease must expire and the client
        # must re-DISCOVER on its own.
        if state['renewed'] and state['phase'] == 'granted' \
                and state['quiet_at'] is not None and now > state['quiet_at'] + 1.0:
            state['phase'] = 'quiet'
            print('net11-dhcp: silent for the renewal/rebind window (lease expires)',
                  flush=True)
        # The whole lifecycle has been observed once the client re-acquired after an
        # expiry; a short grace lets the last echoes land, then the verdict prints.
        if state['regranted'] and echoes[0] > 0 and state['done_at'] is None:
            state['done_at'] = now
        if state['done_at'] is not None and now > state['done_at'] + 12.0:
            print('net11-dhcp: lifecycle complete; ending early', flush=True)
            break

    echo_stop.set()
    # Let the echo thread exit and the final datagram land.
    time.sleep(0.3)
    phases = {
        'silent-window': state['silent_seen'] >= max(1, args.silent),
        'mismatch-rejected': not state['early_request'],
        'grant': state['granted'],
        'renew': state['renewed'],
        'expiry': state['expired'],
        'regrant': state['regranted'],
        'echo': echoes[0] > 0,
    }
    ok = all(phases.values())
    for name, fine in phases.items():
        print('net11-dhcp: %s %s' % (name, 'PASS' if fine else 'FAIL'), flush=True)
    if echo_first[0] is not None:
        print('net11-dhcp: echo sources first=%s:%d last=%s:%d datagrams=%d'
              % (echo_first[0][0], echo_first[0][1],
                 echo_last[0][0], echo_last[0][1], echoes[0]), flush=True)
    print('net11-dhcp: PASS' if ok else 'net11-dhcp: FAIL', flush=True)
    return 0 if ok else 1


if __name__ == '__main__':
    sys.exit(main())
