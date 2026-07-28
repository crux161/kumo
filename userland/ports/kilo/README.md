<!-- j517 -->
# kilo — the first third-party C program ported to KUMO

Salvatore Sanfilippo's ~1000-line terminal text editor, running native on KUMO.

- **Upstream:** <https://github.com/antirez/kilo>
- **Revision:** `323d93b29bd89a2cb446de90c4ed4fea1764176e` ("Fix function
  declaration missing void.")
- **Licence:** BSD-2-Clause, © 2016 Salvatore Sanfilippo. `LICENSE` is the
  upstream file, unmodified, and must travel with the source.
- **`kilo.c` sha256:** `4a44dd0e41670a9e49ecccb338ee199334f0dd472fc7f86467569cf99c391abe`

## It is unmodified

Not "lightly patched", not "with a KUMO ifdef" — **byte-identical to upstream**.
The hash above is the upstream file's. That is the whole point of the port: KUMO
had to meet the program, not the other way round. If a future slice ever needs to
change it, add a `.patch` beside this file and say why here, rather than editing
`kilo.c` in place.

## What KUMO had to grow first

Five slices, each closing one thing kilo needs, none of them written for kilo
specifically:

| Need | Landed |
|---|---|
| `#include <stdio.h>` and friends | J511 — vendored cbindgen, generated sysroot |
| `read(STDIN_FILENO, &c, 1)` | J509 — stdin as a granted capability |
| `tcgetattr`/`tcsetattr` raw mode | J512 — routed through `ttyd` |
| `ioctl(TIOCGWINSZ)` | J513 — window size through the ttyd capability |
| `open`/`write`/`ftruncate` to save | J514 — writable vfs across the FAT engine |

## Building

`xtask` builds it into the initrd as `bin/kilo` using `scripts/kumo-cc.sh`,
which is the one place that knows `--sysroot` and `-D__kumo__`. Do not compile a
port without that define: platform constants are emitted guarded, and without it
a header offers the Linux and Redox values and selects neither.

## Running

```text
[MUREX> kilo /NOTES.TXT
```

Sora grants it stdin, stdout, argv, a filesystem channel, and a `ttyd` control
endpoint — nothing else. It gets no initrd handle, no resource, no device.
