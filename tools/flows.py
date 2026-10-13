"""Summarise IPv4 TCP/UDP flows in a pcapng capture (first/last time, packet count, sizes).

Usage: uv run --no-project --python 3.12 --with dpkt tools/flows.py captures/<file>.pcapng
"""

import socket
import sys
from collections import OrderedDict
from datetime import datetime

import dpkt


def frames(path):
    with open(path, "rb") as f:
        reader = dpkt.pcapng.Reader(f)
        for ts, buf in reader:
            yield ts, buf


flows = OrderedDict()
for ts, buf in frames(sys.argv[1]):
    eth = dpkt.ethernet.Ethernet(buf)
    ip = eth.data
    if not isinstance(ip, dpkt.ip.IP):
        continue
    l4 = ip.data
    if isinstance(l4, dpkt.udp.UDP):
        proto = "UDP"
    elif isinstance(l4, dpkt.tcp.TCP):
        proto = "TCP"
    else:
        proto = f"IP{ip.p}"
        l4 = None
    src = f"{socket.inet_ntoa(ip.src)}:{getattr(l4, 'sport', '')}"
    dst = f"{socket.inet_ntoa(ip.dst)}:{getattr(l4, 'dport', '')}"
    key = (proto, src, dst)
    size = len(l4.data) if l4 is not None else len(ip.data)
    f = flows.setdefault(key, {"first": ts, "last": ts, "n": 0, "bytes": 0, "sizes": set()})
    f["last"] = ts
    f["n"] += 1
    f["bytes"] += size
    if len(f["sizes"]) < 6:
        f["sizes"].add(size)


def t(ts):
    return datetime.fromtimestamp(ts).strftime("%H:%M:%S.%f")[:-3]


for (proto, src, dst), f in flows.items():
    print(f"{t(f['first'])}-{t(f['last'])}  {proto} {src:>22} -> {dst:<22} n={f['n']:<5} bytes={f['bytes']:<7} sizes={sorted(f['sizes'])}")
