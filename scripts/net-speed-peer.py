#!/usr/bin/env python3
"""Mac side of KUMO's AX88 speed test (`net netprobe speed <mac-ip> <port> [seconds]`).

Binds UDP <port> and serves any number of runs, printing a live line every second:
  HELLO <secs>      -> OK                    (kumo found the peer)
  PING <i>          -> PONG <i>              (latency, timed on kumo)
  DOWN <secs>       -> stream 'D'+seq datagrams of 1472 B for <secs>, then DOWNEND <sent>
  STAT <received>      kumo's running receive count, once a second during DOWN; the
                       sender paces on it (x1.25 under 1% loss, x0.75 over 5%)
  'U'+seq datagrams    counted during UP
  UPEND <sent>      -> UPRESULT <received> <bytes> <usec first->last>
  BYE                  kumo is done; print the summary

Usage: scripts/net-speed-peer.py [--port 7710] [--start-mbit 20] [--max-mbit 400]
"""
import argparse
import socket
import struct
import threading
import time

DGRAM = 1472


def mbit(nbytes, seconds):
    return nbytes * 8 / seconds / 1e6 if seconds > 0 else 0.0


class Peer:
    def __init__(self, sock, args):
        self.s = sock
        self.args = args
        self.kumo = None
        self.lock = threading.Lock()
        self.down_sent = 0
        self.down_stat = (0, 0)  # (kumo received, our sent count at that moment)
        self.down_thread = None
        self.up = None
        self.summary = {}

    def send(self, text):
        self.s.sendto(text.encode(), self.kumo)

    # -- download: paced sender ------------------------------------------------------
    def download(self, secs):
        rate = self.args.start_mbit
        pad = b'\0' * (DGRAM - 5)
        start = time.monotonic()
        end = start + secs
        tick = start
        tick_sent = 0
        last_stat = (0, 0)
        sent = 0
        budget = 0.0
        prev = start
        while True:
            now = time.monotonic()
            if now >= end:
                break
            budget += (now - prev) * rate * 1e6 / 8 / DGRAM
            prev = now
            burst = int(budget)
            for _ in range(min(burst, 2000)):
                self.s.sendto(b'D' + struct.pack('<I', sent & 0xFFFFFFFF) + pad, self.kumo)
                sent += 1
            budget -= burst
            with self.lock:
                self.down_sent = sent
                stat = self.down_stat
            if now - tick >= 1.0:
                got_delta = stat[0] - last_stat[0]
                sent_delta = stat[1] - last_stat[1]
                loss = 1 - got_delta / sent_delta if sent_delta > 0 else 0.0
                kumo_rate = got_delta * DGRAM * 8 / (now - tick) / 1e6
                print(f'mac:  down t={int(now - start + 0.5)}s  sending {mbit((sent - tick_sent) * DGRAM, now - tick):7.2f} Mbit/s'
                      f'  kumo got {kumo_rate:7.2f} Mbit/s  loss {max(loss, 0) * 100:5.1f}%  pace {rate:.1f}', flush=True)
                if sent_delta > 0:
                    if loss < 0.01:
                        rate = min(rate * 1.25, self.args.max_mbit)
                    elif loss > 0.05:
                        rate = max(rate * 0.75, 1.0)
                last_stat = stat
                tick = now
                tick_sent = sent
            time.sleep(0.002)
        elapsed = time.monotonic() - start
        self.summary['down_sent'] = sent
        print(f'mac:  down sent {sent} datagrams ({sent * DGRAM / 1e6:.2f} MB) in {elapsed:.2f} s'
              f' = {mbit(sent * DGRAM, elapsed):.2f} Mbit/s offered', flush=True)
        for _ in range(3):
            self.send(f'SPD1 DOWNEND {sent}')
            time.sleep(0.05)

    # -- main loop --------------------------------------------------------------------
    def serve(self):
        self.s.settimeout(0.2)
        up_tick = None
        while True:
            try:
                data, addr = self.s.recvfrom(65535)
            except socket.timeout:
                data, addr = None, None
            now = time.monotonic()
            if data is not None and data[:1] == b'U' and len(data) >= 5 and self.up is not None:
                u = self.up
                if u['first'] is None:
                    u['first'] = now
                    up_tick = (now, 0, 0)
                u['last'] = now
                u['got'] += 1
                u['bytes'] += len(data)
                seq = struct.unpack_from('<I', data, 1)[0]
                u['max_seq'] = max(u['max_seq'], seq + 1)
            elif data is not None and data.startswith(b'SPD1 '):
                self.control(data.decode(errors='replace').split(), addr)
            if self.up is not None and self.up['first'] is not None and not self.up.get('done') and up_tick is not None and now - up_tick[0] >= 1.0:
                u = self.up
                print(f'mac:  up   t={int(now - u["first"] + 0.5)}s  received {mbit(u["bytes"] - up_tick[2], now - up_tick[0]):7.2f} Mbit/s'
                      f'  {int((u["got"] - up_tick[1]) / (now - up_tick[0]))} dgrams/s  total {u["bytes"] / 1e6:.2f} MB'
                      f'  missing {u["max_seq"] - u["got"]}', flush=True)
                up_tick = (now, u['got'], u['bytes'])

    def control(self, words, addr):
        cmd = words[1] if len(words) > 1 else ''
        arg = [int(w) for w in words[2:] if w.isdigit()]
        if cmd == 'HELLO':
            if addr != self.kumo or self.summary.get('done', True):
                print(f'\nmac:  kumo at {addr[0]}:{addr[1]} — speed test, {arg[0] if arg else "?"} s per direction', flush=True)
            self.kumo = addr
            self.summary = {'done': False}
            self.up = None
            self.send('SPD1 OK')
        elif cmd == 'PING' and self.kumo:
            self.send(f'SPD1 PONG {arg[0] if arg else 0}')
            self.summary['pings'] = self.summary.get('pings', 0) + 1
        elif cmd == 'DOWN' and self.kumo:
            if self.summary.get('pings'):
                print(f'mac:  answered {self.summary["pings"]} pings (latency is timed on kumo)', flush=True)
            secs = arg[0] if arg else 5
            print(f'mac:  download phase: streaming to kumo for {secs} s, pacing from {self.args.start_mbit} Mbit/s', flush=True)
            with self.lock:
                self.down_sent = 0
                self.down_stat = (0, 0)
            self.down_thread = threading.Thread(target=self.download, args=(secs,), daemon=True)
            self.down_thread.start()
        elif cmd == 'STAT':
            with self.lock:
                self.down_stat = (arg[0] if arg else 0, self.down_sent)
        elif cmd == 'UP':
            print(f'mac:  upload phase: kumo streams to us for {arg[0] if arg else "?"} s', flush=True)
            self.up = {'first': None, 'last': None, 'got': 0, 'bytes': 0, 'max_seq': 0}
        elif cmd == 'UPEND' and self.up is not None:
            u = self.up
            u['done'] = True
            us = int(((u['last'] or 0) - (u['first'] or 0)) * 1e6)
            self.send(f'SPD1 UPRESULT {u["got"]} {u["bytes"]} {us}')
            if 'up_reported' not in self.summary:
                sent = arg[0] if arg else 0
                loss = (1 - u['got'] / sent) * 100 if sent else 0.0
                print(f'mac:  up   received {u["got"]}/{sent} datagrams ({u["bytes"] / 1e6:.2f} MB)'
                      f' = {mbit(u["bytes"], us / 1e6):.2f} Mbit/s  loss {loss:.2f}%', flush=True)
                self.summary['up_reported'] = True
        elif cmd == 'BYE':
            self.summary['done'] = True
            print('mac:  kumo finished; its RESULT line is on the opi5 console. Waiting for the next run.\n', flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--port', type=int, default=7710)
    ap.add_argument('--start-mbit', type=float, default=20.0)
    ap.add_argument('--max-mbit', type=float, default=400.0)
    args = ap.parse_args()
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 << 20)
    s.bind(('0.0.0.0', args.port))
    print(f'net-speed-peer: listening on udp {args.port}; on KUMO run: net netprobe speed <this-mac-ip> {args.port} [seconds]',
          flush=True)
    try:
        Peer(s, args).serve()
    except KeyboardInterrupt:
        pass


if __name__ == '__main__':
    main()
