# Native Mesa boundary — G7b/G7c/G7d/G7e/G7f/G7g/G7h/G7i/G7j
<!-- j769 -->
<!-- j770 -->
<!-- j772 -->
<!-- j773 -->
<!-- j774 -->

## GL3-b: the triangle copied into view 1 and held on HDMI (J774, metal-proven)

GL3-b (PLAN/015 §8.32, D1/D2 as ratified) shows GL3-a's exact image on the glass. Built with
`KUMO_GL_PRESENT_MAIN`, [gl-triangle.c](gl-triangle.c) runs the GL3-a body unchanged. It then
copies the verified public `glReadPixels` result into view 1 (`GPU_VIEW`) and presents it:

- **Lane (D1).** The stage adds the initrd marker `etc/gpu-lane` = `view` beside
  `bin/gpu-client`. Sora reads it at boot and logs `sora: gpu-lane marker selects the view
  lane`. It keeps `GPU_VIEW` from drv-gpu and sends `V` (view channel), `S` (surface), then
  `N` over the lane pair. This replaces G8's hand-flipped `GPU_CHURN_VIEW`: a `churn` marker
  selects the G8 lane, and an image with no marker behaves exactly as before. A `view` lane is
  one launch. When the client exits, Sora marks the view grantable again and does not regrant.
- **Decode first.** `kumo_metal_view_take` ([device-metal.rs](device-metal.rs)) decodes the
  lane before Mesa starts, through `kumo_graphics::lane::decode_lane`. A native-only lane, meaning
  the marker is missing, fails before any GPU work. The native producer is closed unused.
- **Copy (D2).** `kumo_graphics::lane::copy_readback` converts each RGBA8 word to XRGB8888
  (`0xff6633cc` becomes `0x00cc3366`; the clear `0xff996633` becomes `0x00336699`). It flips
  the bottom-up readback rows upright and writes the 256x128 surface in 512 VmoWrite chunks of
  256 bytes.
- **Hold.** `kumo_graphics::lane::present_hold` sends 15 fresh FULL-damage presents, paced one
  second apart, with one outstanding at a time and a 10 s reply bound. It counts `Retired` and
  `Copied` separately. Any other status, a sequence mismatch or a link failure ends the hold,
  named. The client then closes the view channel and surface and exits, so the view's lease
  returns.
- **Gate (CRUX, 2026-11-03).** drv-fb arms its `Retired` gate only after the J0c diagnostic
  completes (J713). A GL proof image replaces the Rust gpu-client that runs J0c, so every
  present is honestly `Copied`, and `--check` prints drv-fb's `display: present gate ...`
  line. The metal gate is therefore: every present accepted, nothing refused, and CRUX sees
  the triangle on HDMI and then the restored console. `Retired` presentation stays GL4's gate.

QEMU mode 25 is unchanged: the body gained only a NULL `keep` argument. Host tests cover the
marker parse (`cargo test -p sora --lib gpu_lane`) and the lane decode, copy and hold
(`cargo test -p kumo-graphics lane`). The runtime now links `kumo-graphics`, which
`mesa-device-smoke.py` builds and records in the proof manifest.

**Metal (J774, 2026-11-04).** Two PXE boots on the Orange Pi each ran the body exactly
(0 of 32,768 pixels, 0 of 32 guards), wrote the copy in 512 chunks and had all 15 presents
accepted: `retired=0 copied=15`, with drv-fb's `present gate skipped: diagnostic did not
complete`. CRUX photographed the second run on HDMI: pink (`0x00cc3366`) lower-left, blue
(`0x00336699`) upper-right, so the copy is upright. F2c's `VIEW 1` title strip is drawn
inside the view's own rectangle and hides its top `TITLE_HEIGHT` (16) rows. That is drv-fb's
existing chrome, not the copy. The checker passes both captures.

```sh
python3 scripts/mesa-device-smoke.py --gl-screen --run
python3 scripts/mesa-device-metal.py --proof gl3-b --stage <new-directory>
python3 scripts/mesa-device-metal.py --proof gl3-b --check <tio-log>
```

## GL3-a: the first GLES triangle through the tiler, offscreen

[gl-triangle.c](gl-triangle.c) compiles a GLSL ES 300 vertex and fragment shader at
runtime, then clears the frontend's owned 256x128 RGBA8 framebuffer to `0xff996633` and
draws one triangle, (-1,-1), (1,-1), (-1,1), with one vertex buffer and `glDrawArrays`.
Its varying color is equal at all three vertices, so interpolation is exact and packs to
`0xff6633cc`. No pixel center lies on the hypotenuse X + 2Y = 256: pixel (x, y), rows
bottom-up as `glReadPixels` returns them, is inside exactly when x + 2y <= 254, which is
16,384 pixels. A public readback checks all 32,768 pixels and GL2-b's sixteen guard
words on each side. A baseline readback first requires an image different from the
expected one. The color resource's DRM modifier is logged before the draw and after the
readback: `0x0800000000000351`, AFBC both times. AFBC readback goes through a staging
blit, so the readback is itself a second tiled draw. That is also what GL2-b's third
Submit was.

QEMU mode 25 runs the same body over the modeled producer, which executes no GPU and so
retains the initial image. All 26 modes pass. Mode 25 has three Submits (init, the
clear and draw batch, the readback blit) and three wakes, one group/heap, no SyncRead,
matching explicit bind/unbind counts and zero buffers/bindings/heaps/bytes at release.

The standalone client passed on the Orange Pi at 09:39:12 (J773), on Gallium's default
heap (no tiler OOM):

```text
GL3-a readback: 0 of 32768 pixels differ; 0 of 32 readback guards changed (Mali triangle 0xff6633cc over clear 0xff996633)
GL3-a coverage: 16384 pixels inside x+2y<=254; samples (0,0)=0xff6633cc (254,0)=0xff6633cc (255,0)=0xff996633 (0,127)=0xff6633cc (255,127)=0xff996633
GL3-a destroy: shaders, program, vertex buffer, context, drawable and display released
drv-gpu: containment reset=ok buffers=0 bindings=0 waits=0 retained=no
```

The static client is 19,772,408 bytes, SHA256
`050f98266f019e8a0f9320f3e875e224fc776283c241f80725c6e2f47fe3056d`; the loader read
the staged 26,558,871-byte initrd, SHA256
`060b1287e3722f8dfef76493ead34f3d3884d9d42f6c53cf2ca1b5c95af4e308`. Image builds
replace the sysroot's `libc.a`, so the archive the client linked
(`23421283...`) is kept as `build/verification/j773/linked-libc.a`. The served tree and
all 88 manifest files were restored, and pxehost/tio stopped. Evidence:
`build/verification/j773/metal-boot-to-login.log` (checked boot prefix) and
`metal-tio-2.log` (full capture). **GL3-b (the copy into `GPU_VIEW`, shown on HDMI)
is next.** Nothing is presented by this offscreen proof. — TIMBERDOODLE, 2026-11-03

```sh
python3 scripts/mesa-device-smoke.py --gl-screen --run
python3 scripts/mesa-device-metal.py --proof gl3-a --stage <new-directory>
python3 scripts/mesa-device-metal.py --proof gl3-a --check <tio-log>
```

## GL2-b: public GLES clear and pixel readback — GL2 complete

[gl-clear.c](gl-clear.c) uses the frontend's owned 64x48 RGBA8 framebuffer. Public
`glReadPixels` first records its initial pixels and requires a different value from
the requested clear color somewhere in the image. `glClearColor(0.2, 0.4, 0.6, 1.0)`,
`glClear` and `glFinish` then precede a second public readback. On Mali, every one of
3,072 pixels must equal packed RGBA8 `0xff996633`; sixteen words before and after the
CPU readback region must keep `0xa5a5a5a5`. These guards check readback bounds. They
are not a GPU memory-isolation proof. Context, drawable and display are destroyed.

QEMU mode 24 runs the same GL body, retaining the observed initial framebuffer
because the modeled producer executes no GPU instructions. The two CPU read buffers
start with different sentinels, so a no-op readback cannot satisfy the comparison.
All 25 modes pass. The new mode observes exactly three native submissions/wakes,
one group/heap, no SyncRead, matching explicit bind/unbind counts, and zero retained
buffers/bindings/heaps/bytes; the completed wait row is pruned on close.

The standalone client passed on Mali at 20:55:09 (J772):

```text
GL2-b clear: public glClear and native glFinish complete, expected RGBA8 0xff996633
GL2-b readback: 0 of 3072 pixels differ; 0 of 32 readback guards changed (Mali clear color 0xff996633)
GL2-b samples: first=0xff996633 last=0xff996633
GL2-b destroy: context, owned framebuffer drawable and display released
drv-gpu: containment reset=ok buffers=0 bindings=0 waits=0 retained=no
```

The static client is 19,766,552 bytes, SHA256
`ab8e24882c32b1f5fcccd65957f06764bb27ac71eb34bba0c34d8270c0173364`.
The loaded 26,553,047-byte initrd matched the staged candidate. The original served
tree and all 88 manifest files were restored, and pxehost/tio stopped. Evidence:
`build/proofs/gl2-b-metal/metal-boot-to-login.log` (checked boot prefix) and
`metal-tio.log` (full original capture). **GL2 is complete; GL3 owns triangle/display
output.** No image presentation is claimed by this offscreen proof.

An initial texture-FBO test exposed a separate prerequisite: panfrost's texture-size
preflight asks `os_get_total_physical_memory`, whose `sysconf(_SC_PHYS_PAGES)` call
returns -1 in the current relibc backend. Mesa therefore reports GL_OUT_OF_MEMORY
even for 64x48 RGBA8. The clear proof uses the already-owned frontend framebuffer;
it neither overrides that check nor supplies fictitious physical RAM. Public texture
allocation remains an explicit follow-up before a texture-dependent workload.
— KESTREL, 2026-11-02

```sh
python3 scripts/mesa-device-smoke.py --gl-screen --run
python3 scripts/mesa-device-metal.py --proof gl2-b --stage <new-directory>
python3 scripts/mesa-device-metal.py --proof gl2-b --check <tio-log>
```

## GL2-a: runtime GLES compute output

[gl-compute.c](gl-compute.c) compiles and links a GLSL ES 310 compute shader through the
public GL API on KUMO, dispatches four groups of sixteen invocations, and reads the SSBO
back after `glMemoryBarrier` and `glFinish`. The shader writes 64 words with
`0x6b750000u ^ (i * 0x9e3779b9u)`. The CPU initializes all 80 words to `0xdeadbeef`; the
last sixteen are guards, and the readback must match every output while preserving them.
Shader, program, buffer and frontend objects are explicitly destroyed.

The first QEMU run exhausted the old 16 KiB process stack in Mesa's GLSL parser. GL links
now reuse G7n's existing 2 MiB `PT_GNU_STACK` request and header writer. Sora already
honors and bounds that request; no loader/kernel policy changed. The request is RW and
non-executable. The generated link script and shared stack helper are included in the
input manifest, and imported build-module bytecode stays under the build output.

QEMU mode 23 runs the actual compiler and submission path, with exactly two Submits
(initialization and compute), one group/heap, two wakes, and matching explicit bind/unbind
counts. Its producer runs no GPU: the readback must retain the CPU poison and guards.
All 24 modes pass, and teardown releases every buffer/binding/heap/byte; the completed
wait row is pruned on close.

The standalone `gl-compute` client passed on Mali (J770):

```text
GL2-a compiler: GLSL ES 310 compute source compiled and linked at runtime
GL2-a dispatch: four groups of sixteen invocations, memory barrier and native glFinish complete
GL2-a readback: 0 of 64 output words differ; 0 of 16 guards changed (Mali shader results)
GL2-a destroy: shader, program, SSBO, context, drawable and display released
drv-gpu: containment reset=ok buffers=0 bindings=0 waits=0 retained=no
```

The loaded 26,554,119-byte initrd and client matched the staged hashes. The original
served tree and all 88 manifest entries were restored, and pxehost/tio stopped. Evidence:
`build/proofs/gl2-a-metal/metal-boot-to-login.log` (checked boot prefix) and
`build/proofs/gl2-a-metal/metal-tio.log` (full session, including later interactive commands).
**GL2-a is closed. GL2-b's clear/readback proof above completes GL2.**
Displaying an image remains a later gate.
— KESTREL, 2026-11-02

```sh
python3 scripts/mesa-device-smoke.py --gl-screen --run
python3 scripts/mesa-device-metal.py --proof gl2-a --stage <new-directory>
python3 scripts/mesa-device-metal.py --proof gl2-a --check <tio-log>
```

## GL1-f: the static GLES2 client — GL1 complete

[gl-client.c](gl-client.c) is a standalone static KUMO application with its own `main`.
It opens Gallium's native screen, uses the KUMO frontend to create/bind a context and
drawable, calls public GLES2 `glGetString`, `glGetError` and `glFinish`, then destroys the
frontend resources. The C application uses `<GLES2/gl2.h>` and no private Mesa GL context
or dispatch calls. It reuses the existing Rust GPU-slot bootstrap/session owner, keeping
its native authority in the one granted session.

`mesa-device-metal.py --proof gl1-f` builds `build/proofs/gl1-f-metal/gl-client`, an AArch64
ET_EXEC with TLS and no interpreter or dynamic segment. The frontend, Mesa state tracker,
glapi, panfrost and native bridge are statically linked. `--stage` builds the base Orange Pi
image at console level 5, then packages the ELF in a separate candidate's `bin/gpu-client`
slot. Serving or installing that candidate is a separate step. QEMU mode 22 runs the same
application body against the modeled completion producer;
all 23 device modes pass. No rendering job is submitted after Gallium's initialization.

The standalone ELF passed on the Orange Pi (J769):

```text
GL1-f renderer: Mali-G610 MC4 (Panfrost)
GL1-f version: OpenGL ES 3.1 Mesa 26.2.5
GL1-f finish: public glFinish returned with GL_NO_ERROR and the native device live
GL1-f destroy: context unbound, drawable and display destroyed
drv-gpu: containment reset=ok buffers=0 bindings=0 waits=0 retained=no
```

This is an entry-point/lifecycle proof; the reported version is not a conformance result.
The loaded 26,552,495-byte initrd and its client matched the staged hashes. The original
served tree and all 88 manifest entries were restored, and pxehost/tio stopped. Evidence:
`build/proofs/gl1-f-metal/metal-tio.log`. **GL1 is closed. Next is GL2: runtime compute
output into an SSBO, then GL clear/readback, each checked on Mali.** — KESTREL, 2026-11-02

```sh
python3 scripts/mesa-device-metal.py --proof gl1-f
python3 scripts/mesa-device-metal.py --proof gl1-f --stage <new-directory>
python3 scripts/mesa-device-metal.py --proof gl1-f --check <tio-log>
python3 scripts/mesa-device-smoke.py --gl-screen --run
```

## GL1-e: the KUMO GL frontend

[kumo_gl.h](kumo_gl.h) exposes a small C frontend over Gallium's native screen (J768).
It creates a Mesa GLES2 state-tracker context with `st_api_create_context`, makes it current
with `st_api_make_current`, and supplies a fixed-size drawable with owned RGBA8 color and
Z24/S8 depth/stencil resources. The structure follows Mesa's Haiku `hgl` frontend. Glthread
and the threaded pipe stay off; calls belong to one thread. GL1 admits one display, one
context attempt per display/session, and one drawable at a time.

`kumo_gl_display_create` takes ownership of the screen on success. Display destruction
refuses while a context or drawable remains, and drawable destruction refuses while it is
current. Context destruction unbinds it; a new context requires a fresh display/session.
The drawable's validate callback returns resource references to Mesa. Its flush_front waits
for native fence completion before invoking an optional caller presentation callback with
the borrowed color resource. Callback refusal propagates to the caller. Offscreen operation
needs no callback, and the frontend acquires no framebuffer or display authority.

QEMU mode 21 checks Mesa's actual current GLES2 context and framebuffer attachment pointers,
both thread modes off, invalid dimensions, duplicate-object and busy-destruction refusals,
unbind/rebind, callback acceptance/refusal and teardown. All 22 device modes pass. The model
sees exactly one Submit (Gallium's initialization), one group/heap, matching bind/unbind
counts, and zero buffers/bindings/heaps/bytes at close. Its completed wait row is pruned by
session close. State-tracker C++ queries pull the already inventoried optional weak TLS
initializer; the link check permits only measured weak symbols and still rejects every
strong unresolved symbol.

Metal proof 9 (`--proof gl1-e`) ran the same cases on Mali. Mesa's context became current,
the front flush completed, and the heap/context/display were destroyed with
`containment reset=ok buffers=0 bindings=0 waits=0 retained=no`. The loader's 26,552,655-byte
initrd matched the staged client. The served PXE tree and all 88 original manifest entries
were restored; pxehost and tio are stopped. Evidence: `build/proofs/gl1-e-metal/metal-tio.log`.
**Next: GL1-f, the static GLES2 application target and its public GL entry-point proof.**
GL2 owns the first output through GL calls. — KESTREL, 2026-11-02

```sh
python3 scripts/mesa-device-smoke.py --gl-screen --run
python3 scripts/mesa-device-metal.py --proof gl1-e --stage <new-directory>
python3 scripts/mesa-device-metal.py --proof gl1-e --check <tio-log>
```

## GL1-d: batches, activity tracking and fences

Gallium's batches now submit through the native device (J765). Each submission takes the VM
timeline's next point, which is also the group's next point, because a session has one group
(D2). One PREPARE signals that point and the context binary, which is what
`drmSyncobjTransfer` did. No wait goes into the PREPARE: every fence in a session is on that
one group's queue, which runs in order. Only an accepted Submit publishes the point. Batch
end attaches it to every BO the batch touched, as Panthor's per-BO sync points do. The kumo
backend's `bo_wait` is real now: a reader waits for the last write, a writer for every
access, by SNAPSHOT, ARM, bounded WAITs and ARM again (`pan_kmod_kumo_snapshot_wait`). A
fence is a native snapshot of the context binary. `fence_finish` waits on it the same way,
the sync-file paths stay refused, and `native_fence_fd` is now false. `csf_sync_ctx_state`
reads native GROUP_STATE, and a reset context is not re-created. The init job takes point 1
through the same path.

Freed BOs used to leak their GPU addresses. `panfrost_bo_free` unmaps with
`DEFER_TO_NEXT_IDLE_POINT`, which the bridge refused until now. Gallium ignored the error, so
every freed BO kept its auto-VA range, and only the server's buffer destroy dropped the
binding. A TRACK_ACTIVITY VM now waits for its last point and then unmaps.

`mesa-device-smoke.py --gl-screen --run` adds mode 20 (21 modes in all). It clears a linear
64x64 RGBA8 target, flushes with a fence, polls the fence while the modeled producer holds
completion (the poll times out and gives its wake back), waits, reads back and checks group
state. Exactly two Submits cross, at points 1 and 2, with three wakes. In modes 17, 19 and 20,
every binding Gallium makes is explicitly unbound before its buffer is destroyed. Mode 16
adds a deferred unmap whose range the next map reuses. Metal proof 8 (`--proof gl1-d`) runs
the same cases on Mali, where every pixel must read back as the clear color. — TIMBERDOODLE,
2026-11-02

```sh
python3 scripts/mesa-device-smoke.py --gl-screen --run
python3 scripts/mesa-device-metal.py --proof gl1-d --stage <dir>
python3 scripts/mesa-device-metal.py --proof gl1-d --check <tio log>
```

## GL1-c4: context initialization and cleanup on Mali

GL1-c is closed on the Orange Pi (J763). Proof 7 (`--proof gl1-c`) booted the signed J762
candidate, created Gallium's native CSF context, and completed its initialization stream on
Mali. The heap had five 2 MiB chunks; its six mapped pieces were destroyed after native
completion. A second context was refused both while the first was live and after its
destruction, preserving the session-owned group rule. The screen/device then closed with:

```text
GL1-c metal context green: Gallium's native initialization completed, a second context was refused, and the heap, context and screen were destroyed
drv-gpu: containment reset=ok buffers=0 bindings=0 waits=0 retained=no
```

The loader's 21,622,607-byte initrd matched the staged candidate. The serial checker passed
with every proof line once and in order, and no TOWER/FAILED/ABEND in the boot. The served PXE
tree was restored byte-for-byte and its original manifest verified; the board is at login,
and pxehost/tio were stopped. Evidence: `build/verification/j763/metal-tio.log` and the copy
at `build/proofs/gl1-c-metal/metal-tio.log`. **Next: GL1-d, batches, activity tracking and
fences, with a clear/flush/fence-wait/readback proof.** — KESTREL, 2026-11-02

## GL1-c3: native context initialization and cleanup (QEMU)

Gallium's CSF initializer now creates the one native group, creates its tiler heap through
ABI 15, and submits its own HEAP_SET / tiler-OOM-handler initialization stream through native
PREPARE and STEP. The context keeps its full-width group ID. It waits through ARM, a bounded
WAIT when pending, and another ARM: a wake alone is not completion. Cleanup snapshots the
context's output binary, waits, and destroys the heap. The group remains session-owned; a
second context, including after destruction of the first, is refused until a fresh session.
Uncertain submission or completion closes the device before caller cleanup.

`mesa-device-smoke.py --gl-screen --run` adds mode 19 (20 modes total). It inspects Gallium's
actual five-word init stream and its live heap/handler addresses. The modeled producer
completes only after a real ARM wake endpoint exists. Exactly one group, heap and Submit
cross, the wake is consumed, and teardown leaves zero buffers/bindings/heaps/bytes. One
completed wait-table row remains until close; it has no live endpoint. The raw VmoCreatePaged
output vector is now explicitly `u64`: its former inferred `i32` elements let the kernel's
64-bit address writes overwrite the proof process's heap.

The actual bridge headers are staged beside the patched driver and refreshed by the
incremental closure build. There is no duplicated native ABI definition in Mesa.

The same cases build as proof 7 (`--proof gl1-c`); staging creates a separate ESP candidate.
J763 above closes their metal acceptance. — KESTREL, 2026-11-02

```sh
python3 scripts/mesa-device-smoke.py --gl-screen --run
python3 scripts/mesa-device-metal.py --proof gl1-c --stage build/proofs/gl1-c-metal/image
python3 scripts/mesa-device-metal.py --proof gl1-c --check <tio log>
```

## GL1-c2: Gallium context binary sync ownership

The KUMO context constructor creates a signaled output binary and an empty input binary
through `pan_kmod_kumo_sync_create`. Their IDs stay in dedicated 64-bit fields; the DRM
handle fields are not aliases. Both binaries are created before other context resources,
so either create can fail and release only the initialized IDs. The context destructor
uses native DROP; a failed void sync destructor loses the device and closes the session.
No wire or native ABI change is needed.

`mesa-device-smoke.py --gl-screen --run` adds mode 18, driving the real Gallium constructor.
A test-only observer adds a high word to real native IDs and translates them back at the
callback boundary, checks each binary's initial payload and that DROP removes its metadata,
and injects first/second-create refusal. Two subsequent attempts inject native
GROUP_CREATE refusal and release both binaries; any DRM ioctl now fails the proof. After screen destruction the modeled service
has no buffer or binding left. Only this QEMU proof wraps `drmIoctl`, and it still delegates
to the real refusal implementation; the metal screen client does not link the observer.

GL1-c3 above adds the native group/heap/init-stream and completion-wait paths on QEMU.
This earlier gate proves sync ownership and failure cleanup; metal context acceptance remains open.
— KESTREL, 2026-11-02

## GL1-c1: native command-stream register counts

Gallium's context initializer, tiler-OOM handler and batch builder now take their register
configuration through one helper in `kumo-build.patch`. On KUMO it uses
`pan_kmod_kumo_cs_reg_count(dev)`: `CS_FEATURES[7:0] + 1`, cached from the native device's
attested query, with the last four registers reserved for drv-gpu's Submit wrapper. The
Panthor private-layout accessor remains in the non-KUMO branch. Firmware slot counts are
not needed by these builders and are not invented.

`mesa-device-smoke.py --gl-screen --run` also checks the modeled attested 96-register count,
the 256-register decoding edge, repeated reads without another query, and null/foreign device
refusal. This is GL1-c's first prerequisite: context creation, its init job and cleanup still
need the native group/heap/sync/submission seam and a metal proof. — KESTREL, 2026-11-02

## GL1-b: Gallium's screen over the native device

`panfrost_create_screen_kumo(kmod)` (kumo-build.patch) opens Gallium's panfrost screen over a
kmod device the client made with `pan_kmod_kumo_dev_create(api)`, with no DRM fd and no
pipe-loader. It builds panfrost's driconf options itself and owns the device from then on.
The kumo VM serves Gallium's `AUTO_VA | TRACK_ACTIVITY`: one lowest-fit address heap over the
window, freed ranges reused, caller-chosen addresses refused, and a full window refused with
nothing sent. Activity tracking waits for GL1-d's submissions, and Gallium skips the BO wait for
anything the GPU never touched. Gallium's BO unmap now goes back through the bridge the map
came from (`pan_kmod_kumo_bo_munmap`). relibc's `clock_gettime` gained the monotonic clocks:
it had been an ENOSYS stub, and Gallium's BO cache aged its BOs by uninitialized stack.

Proofs:
- QEMU: `mesa-device-smoke.py --gl-screen --run`, mode 16 (auto-VA) and mode 17 (the screen),
  linked with Gallium.
- Metal: proof 6 (`--proof gl1-b`). It reported "Mali-G610 MC4 (Panfrost)", GLSL 140, a 16384
  maximum 2D texture and compute, created and destroyed a 64x64 RGBA8 render target, and closed
  with `containment reset=ok buffers=0 bindings=0`: every BO Gallium made was released.

`build(gl=True)` links the inventory's GL archives on the host, rebuilding thin archives from
their members and copying regular ones, plus drm-refusal, Gallium's generated format header and
the C++ runtime. The metal client is 14.8 MB. — TIMBERDOODLE, 2026-11-02

```sh
python3 scripts/mesa-device-smoke.py --gl-screen --run
python3 scripts/mesa-device-metal.py --proof gl1-b --stage build/proofs/gl1-b-metal/image
python3 scripts/mesa-device-metal.py --proof gl1-b --check <tio log>
```

## GL1-a: command streams in writable no-execute memory

CRUX ruled on 2026-11-02 that Submit accepts a stream from a writable no-execute binding.
Gallium and panvk build their command streams in such pools. The stream must still fit
wholly inside one live binding of the group's VM. W^X still holds, because a valid binding is
read-only or no-execute, never writable and executable. As DESIGN/021 notes, a read-only GPU
mapping never made the client's CPU memory immutable; the cooperative rule (stop writing
before Submit, reuse after completion) is unchanged.

Proof 5 (`--proof gl1-a`) runs three jobs through the native bridge:
- one stream from a GPU-cached no-execute BO, Gallium's CS pool's flags;
- the same 64-byte line rewritten and run again, as a recycled CS chunk would be;
- one stream from a GPU-uncached no-execute BO.

All three stores landed, and the rewritten line ran its new words with the first store intact.
No stale copy of a stream line survived between jobs under the driver's job wrapper and its
cache flushes. A stream crossing its binding's end is still refused, before any I/O. The run
was green on the first boot, with no fault.

— TIMBERDOODLE, 2026-11-02

```sh
python3 scripts/mesa-device-metal.py --proof gl1-a --stage build/proofs/gl1-a/image
python3 scripts/mesa-device-metal.py --proof gl1-a --check <tio log>
```

## G7q-c: tiler heaps through the C bridge (ABI 15)

ABI 15 appends `heap_create` and `heap_destroy` to the API table, which grows from 120 to 136
bytes, and the factory refuses a device that lacks either. The published device exposes them
as `pan_kmod_kumo_heap_create(dev, request, info)` and `pan_kmod_kumo_heap_destroy(dev, heap,
bytes)`, the shape of TILER_HEAP_CREATE and _DESTROY, for GL1's `pan_csf.c` seam and VK2's
queue seam. `struct kumo_gpu_heap_request` names the logical VM, the chunk size, the initial
and maximum chunk counts, `target_in_flight`, and an MBZ flags word. `struct kumo_gpu_heap_info`
returns the heap ID, the context VA and the first chunk's VA.

Heaps go through the same BO/session owner as everything else. The owner checks Panthor's
rules, an unknown logical VM, and the eight-heap cap locally, so a refusal sends nothing.
Every heap crosses on the server VM. A heap belongs to its logical VM: destroying that VM
destroys its heaps first, as Panthor's VM teardown takes its heap pool, and the next logical
VM starts with none. An unknown heap is `KUMO_GPU_UNKNOWN_OBJECT`, and known loss poisons the
device here too. Failures leave a zeroed `info`.

Proofs:
- QEMU mode 15 of `mesa-device-smoke.py --heaps --run`: the factory's refusals, Panthor's
  shapes refused with no I/O, exact bytes on destroy, and VM teardown destroying heaps.
- Metal proof 4 (`--proof g7q-c`): G7q-a's GPU chain walk on a heap made and destroyed
  through the bridge. It was green on the first boot.

— TIMBERDOODLE, 2026-11-02

```sh
python3 scripts/mesa-device-smoke.py --heaps --run
python3 scripts/mesa-device-metal.py --proof g7q-c --stage build/proofs/g7q-c-metal/image
python3 scripts/mesa-device-metal.py --proof g7q-c --check <tio log>
```

## G7q-b: tiler OOM served by growing the heap

drv-gpu now answers `CS_TILER_OOM`, Panthor's `group_process_tiler_oom` and
`panthor_heap_grow`, inside a job's wait. It reads the heap context the firmware named and the
render-pass counters. It grows that heap by one chunk unless more passes are in flight than
`target_in_flight` or the heap already has `max_chunks`. The chunk is charged to the shared
session budget first, and the charge is refunded if allocation fails. drv-gpu maps the chunk
live and invalidates only its packed region. It hands the firmware the chunk's encoded link as
both heap_start and heap_end (0 for a refusal), acknowledges bit 26 alone, and wakes the stream.
A refusal is not an error: the firmware then waits for fragment work or runs the stream's
tiler-OOM handler. Only a partial map or a failed live flush loses the session.

This is host-proven only. No KUMO workload tiles yet, so nothing raises the event. A metal
boot with the new wait loop (the G7q-a proof, `build/proofs/g7q-b/`) completed every job and
logged no OOM line. Growth on hardware is G7q-d's, under the first tiling job.
— TIMBERDOODLE, 2026-11-01

## G7q-a: tiler heap memory on metal

The channel has two new opcodes. HeapCreate (19) takes Panthor's TILER_HEAP_CREATE fields:
a VM, a page-aligned chunk size in [128 KiB, 8 MiB], at least one initial chunk, a maximum no
smaller than that, and `target_in_flight`. drv-gpu places a context page and the initial
chunks in one run in its heap window, 0x3000_0000-0x3800_0000. That window is part of the
client's address space but sits outside the client's manual-VA window, so no VmBind or
Submit can name it. Each chunk header links the previous chunk, head-first as Panthor does.
Everything is mapped no-execute and GPU-cached. The reply carries the context VA and the
head chunk's VA, and no handle, since the client never maps heap memory. HeapDestroy (20)
unmaps, flushes and releases the heap. Heaps share the 128 MiB session budget with buffers,
and a session may hold 8.

Proof 3 (`--proof g7q-a`) makes the GPU read the heap, because the client cannot. Each job
LOAD_MULTIPLEs one chunk header and stores it in the data BO, and the next job follows the
link the GPU reported. On metal the GPU walked 0x30041000 → 0x30021000 → 0x30001000 → 0. Each
link carried the chunk size, and the context read 32 zero bytes. After HeapDestroy, a load
from the old head took `TRANSLATION_FAULT Read` at exactly 0x30041000, and drv-gpu contained
it with a reset.

Tiler-OOM growth (G7q-b), the native C entry points (G7q-c) and growth under a real tiling
job (G7q-d) are still to come. — TIMBERDOODLE, 2026-11-01

```sh
python3 scripts/mesa-device-metal.py --proof g7q-a --stage build/proofs/g7q-a/image
python3 scripts/mesa-device-metal.py --proof g7q-a --check <tio log>
```

## G7p-e: native sync and group state on metal

G7o-f's vehicle (`device-metal.c`/`.rs`, staged as `bin/gpu-client`) now builds one of three
proofs, picked by `KUMO_METAL_PROOF`: 0 is G7o-f, 1 is G7p-e ending in a fatal fault, 2 is
G7p-e ending in a timeout. A failed group ends its session, and Sora regrants only on the churn
image, so each failure state needs its own boot. Every G7p-e boot first runs the same cases
through `pan_kmod_kumo_native` against real Mali completions:
- binary and timeline syncs, a group whose READ-only status page reads USABLE, and a CPU
  binary signal and reset;
- job A (point 1) signals a binary and timeline 42, which is then wait-pending;
- job B (point 2) waits on both of A's signals before anything has observed A, so its STEP
  asks drv-gpu about point 1, finds it done on the GPU, and submits. A CPU signal of
  timeline 200 is refused Busy behind B's unobserved point;
- ARM completes both points, both `{value, status 0}` stores land, and timeline 200 is
  accepted afterwards.

Job C (point 3) then fails on purpose. drv-gpu accepts it, fails it at its 1 s deadline,
publishes the group status, contains with `reset=ok` and closes the session. ARM sees the
close, the VM is faulty, and GROUP_STATE reports the right fact before and after the loss:
- **fatal:** a store into the executable binding faults (`PERMISSION_FAULT Write`), so the
  MMU evidence makes the stall FATAL;
- **timeout:** a `SYNC_WAIT32` on a word nobody writes stalls with no fault evidence, so it is
  TIMED_OUT.

drv-gpu runs each job to completion or deadline before reading the next request. So no fence
a request asks about is still running, and WAIT's Port/timer wake path stays proven in QEMU
only. The proof lines are NOTICE, and J748's default console shows WARNING and worse, so
`--stage` rebuilds the image with `KUMO_CONSOLE_LOGLEVEL=5` before copying it.

Facts for later slices:
- **A known loss unmaps every CPU alias of the session's BOs** (J751's shared-owner close).
  The first fatal boot read a BO after the loss and took an EL0 translation fault. A Mesa
  robustness path (GL5, VK6) must not touch a mapping after loss, or the bridge must keep
  aliases until unmap.
- drv-gpu's stall report printed CS status words of zero in both failures, because it never
  requested CSG_REQ STATUS_UPDATE (RESEARCH/009 §9.5). J756 requests it and tags the line
  `(refreshed)` or `(stale)`. On metal the timeout's stall then read `cmd_ptr=0x20200090
  blocked=3 wait=0x81000000 sync=0x7fffffff@0x20000040`: the SYNC_WAIT32 itself, waiting
  GREATER on the gate word. With AS1 holding a permission fault, the firmware did not
  acknowledge within 10 ms, so the fatal stall's words stay stale.

— TIMBERDOODLE, 2026-11-01

```sh
python3 scripts/mesa-device-metal.py --proof g7p-e-fatal --stage build/proofs/g7p-e-fatal/image
python3 scripts/mesa-device-metal.py --proof g7p-e-fatal --check <tio log>
python3 scripts/mesa-device-metal.py --proof g7p-e-timeout --stage build/proofs/g7p-e-timeout/image
python3 scripts/mesa-device-metal.py --proof g7p-e-timeout --check <tio log>
```

## G7p-d: group facts survive command-channel loss

ABI **14** retains the 120-byte API, 104-byte Info and 240-byte property report. Native
operation `KUMO_NATIVE_GROUP_STATE` (13) takes the opaque group ID and returns a native enum
in `reply.value` plus the observed `gpu_status`. It remains available after known loss and
owner close, before the C device is destroyed. Other operations still refuse after loss.

| Native group value | Meaning |
|---|---|
| USABLE (0) | Last producer report is usable, and this client/VM has no known loss |
| FATAL (1) | GPU sync status, CS fault/fatal word or the group's MMU fault was observed |
| TIMED_OUT (2) | Job deadline left seqno short, with no observed GPU/CS/MMU fault |
| UNKNOWN (3) | Closed/unverified group, missing IRQ, late/mismatched seqno, transport or unreadable status |

These values are mutually exclusive native enums, not Panthor bit flags or guilt attribution.
A fault outranks a deadline: a permission fault which also stalls the queue reports FATAL,
including when the raw sync status is zero. No group result is a fence completion or permission
to reuse/free DMA. USABLE is a last-known snapshot, not an independent peer-liveness heartbeat;
an observed transport/owner loss makes an old usable sample UNKNOWN.

Wire opcode 18, `GroupWatch { group }`, requires the session's live group and returns its
initial report plus one READ/DUPLICATE/TRANSFER status VMO grant. This is ordinary CPU metadata:
no GPU mapping, device pin or MAP/WRITE grant. The driver initializes a fresh 4 KiB VMO per
session and writes the 16-byte `KGS1, group, state, gpu_status` record with VmoWrite; readers
use VmoRead. Strict decoding rejects unknown enums, wrong groups, truncated records, and
nonzero GPU status outside FATAL. No new syscall or asynchronous reply format is introduced.

The real driver's completion failure path samples CS/MMU fault evidence and publishes the
terminal report before reset/containment and channel close. Other close paths publish UNKNOWN
without erasing a positive fault/timeout. The CPU-only status owner closes on both reset
outcomes; it never participates in DMA reclamation. A reader grant independently retains the
last record after the writer and command endpoint disappear.

The shared BO context acquires and validates this grant at native group creation, owns it
until close, and caches positive reports before releasing it. GROUP_STATE performs no command
RPC. It refuses foreign/stale/wrong-kind IDs, survives C-device poisoning, and never converts
a known lost/unreadable group into USABLE. Failed subscription/initial validation closes the
grant and contains the already-created session. Normal scope close retains UNKNOWN, not a
fabricated fatal/timeout. The existing single-group and prepared-submit rules are unchanged.

```sh
python3 scripts/mesa-device-smoke.py --group-state --run
```

QEMU exercises real read-only VMO permissions and transfer, retains reports after closing a
real command peer and the only writer, and tests fatal/timeout/unknown through the real C/Mesa
device after a modeled command-transport loss. The model uses the production classification
function. Status/wake/BO grants close exactly once; cached reads perform no replay or command
I/O. Existing sync/device/W^X cases still run. The actual Mali failure sampling remains the
next metal gate; no new hardware observation is claimed here.
— MILVUS, 2026-11-01

## G7p-c: the native C boundary shares the BO/session owner

J751 introduced callback ABI **13**, **120 bytes**; J752 advances it to 14 without a layout
change. Info stays 104 bytes and the property report stays 240 bytes. A single appended
`native` callback exposes the operations below through
`pan_kmod_kumo_native(dev, request, reply)`. The current factory rejects ABI 13 and older, truncated API tables
and a missing native callback. Query-only and BO-only APIs leave this callback null.

The device's existing Rust `Buffers` context retains sync state, snapshots, prepared jobs and
owned wake channels alongside its BOs and logical VM. Each callback briefly lends that sync
state the same Client; it restores metadata after success or recoverable refusal. There is
one endpoint and one monotonically increasing request sequence. No second Client or DRM FD
is created. The adapter publishes/observes Normal-NC shared memory with a system barrier;
the GPU wrapper remains responsible for cache cleaning before completion.

All native IDs are opaque Rust metadata keys from one never-reused ID space, with kind and
context lookup before use. C never receives a kernel wake handle or an unchecked GPU fence.
There are at most 256 native entries (including one group), with the existing 64-sync and
256-point limits underneath. Capacity and return-ID reservations precede any I/O that could
accept work or transfer a wake. A snapshot, job or wake ID has an explicit lifetime; dropping
a job discards only its intent and cannot cancel GPU work.

| Operation | Request fields | Result on success |
|---|---|---|
| CREATE_BINARY / CREATE_TIMELINE | value = initial signal (0/1) or u64 timeline value | new sync ID |
| DROP | id | release sync/snapshot/job metadata or close a wake; group drop refuses |
| RESET / SIGNAL | id, signal value for SIGNAL | binary reset or host signal |
| PENDING | id, wait value | value and state report signal availability, separately from completion |
| SNAPSHOT | id, wait value | new snapshot ID, or UNAVAILABLE with ID 0 |
| ARM | snapshot id | COMPLETE, or AVAILABLE with an owned wake ID |
| WAIT | wake id, relative timeout in ns | WOKEN, TIMED_OUT or CLOSED; consumes/closes the wake |
| GROUP_CREATE | value = current logical VM ID | one group ID for the device lifetime |
| PREPARE | group id, job, wait/signal arrays and counts | new prepared-job ID |
| STEP | prepared-job id | SUBMITTED with snapshot ID, or AVAILABLE with wake ID |

Flags and unused fields must be zero. Arrays are live/aligned for their bounded count and are
copied before the callback returns; no C array pointer is retained. A prepared job records the
logical VM generation and stream, rechecked against live owned bindings before each step.
A stale VM or destroyed/unbound stream cannot reach Submit. Input snapshots survive object
reset/destruction. Output metadata still changes only after acknowledgement, and an accepted
intent cannot replay. A recoverable refusal preserves it; uncertain transport or failed GPU
completion latches loss across BO, VM and sync operations.

WAIT accepts 1 ns through 5 s and consumes its wake on timeout, peer close, platform failure or
unsupported waiting too. A wake byte alone never implies completion: ARM/STEP must obtain an
authoritative FenceWait Done/status 0. Failure clears result IDs; Unsubmitted (status 11)
returns the input index in value, and failed GPU completion returns DeviceLost with raw
gpu_status. The native C wrapper closes the owned session on known loss. Device/scope close
also closes every remaining Rust-held wake; it does not claim DMA quiescence.

```sh
python3 scripts/mesa-device-smoke.py --native-sync --run
```

The extended proof uses real Mesa device/BO/VM lifecycle and C callbacks, a modeled GPU
producer through the actual codec/Session, and real kernel VMOs, channels, Ports and timers.
It checks timeout, a wake byte without completion, peer close without completion, rejected
submit/retry, once-only acceptance, failed dependency, uncertain publication, foreign/stale
IDs, stale VM generations and exact grant/wake cleanup. Rust verifies one contiguous request
sequence per session and zero SyncRead requests. The existing device and W^X cases still run.

Group fatal/timed-out reporting and GPU metal acceptance remain G7p gates. Group destruction
is device/session close in this one-group lane; creating several groups, automatic VA/activity
tracking, wait-many and future-value wake policy are not added. Gallium's fence wrapper and
panvk's vk_sync type remain GL1/VK2; sync-file import/export stay unsupported.
— MILVUS, 2026-11-01

## G7p-b: captured dependencies before Submit

`SyncObjects::prepare(submission, waits, signals)` validates the full output set, reserves
metadata and captures each input's current payload. Duplicate payloads share one dependency.
An unsubmitted input refuses locally with its index; preparation sends no request and publishes
no output. There are at most 64 input points per intent. Resetting, replacing or destroying an
input object after preparation cannot change the captured fence.

`submit_prepared` steps the intent through those captured dependencies. Each one must answer
FenceWait Done/status 0 before the next is considered. A pending dependency returns `Blocked`
with its snapshot and one owned wake-channel read end; no Submit is sent. The caller bounds its
Port wait with a timer, closes that read end on every path, and steps the same intent again.
Neither wake, timeout nor peer close advances the dependency cursor. Output objects are
revalidated after interleaved serial calls, before any wait I/O and before Submit.

Once every input is complete, the existing submit path atomically publishes outputs after
acceptance. A recoverable server refusal leaves outputs unchanged and permits an explicit
retry without waiting again for inputs already proven complete. Acceptance consumes the
non-cloneable intent; further steps refuse locally. Transport/protocol uncertainty or failed
dependency completion latches loss and prevents dependent work and replay. Dropping an intent
discards only that local intent, with GPU work and wake ownership unchanged.

```sh
python3 scripts/mesa-sync-smoke.py --dependencies --run
```

This executes both native sync proofs in one disposable UEFI image. G7p-b uses the actual
codec/Session with a modeled GPU, real kernel channels, Port and timer: a timed-out input sends
no dependent Submit; a later wake still requires completion confirmation; a refused Submit
preserves both outputs; acceptance happens once; a failed input closes its wake without a byte
and prevents the dependent job. The request ledger contains zero SyncRead requests.

This is client-side ordering for the existing single-group lane. C callbacks and their shared
BO/sync session owner, wait-many and future-value wake policy, group fatal/timeout reporting,
and GPU metal acceptance remain G7p work. The C ABI stays 12 and the wire is unchanged.
— MILVUS, 2026-11-01

## G7p-a: native sync payloads on G8c fences

`kumo_gpu::sync::SyncObjects` borrows one native Client and owns session-local binary and
timeline sync metadata. It publishes every output of a Submit only after its acknowledgement;
validation and fallible allocation happen before publication. A refused submit preserves the
old payloads. Transport/protocol uncertainty and GPU failure latch the existing Client's loss,
so neither work nor signals are replayed.

- Binary objects can start signaled, be CPU-signaled, reset and replaced. Their wait value is 0.
- Timeline values are u64, monotonic and sparse; a wait for 0 is satisfied. Pending logical
  points map to accepted G8c queue fences. All GPU signals in a store use the same group, so a
  later completed queue point covers the earlier ones. Observed completion prunes old rows.
- `wait_pending` answers availability of a submitted signal, separately from completion.
  An unsubmitted value has no snapshot; no fake wake is manufactured for a future producer.
- A captured `Snapshot` survives binary reset, replacement and object destruction. Gallium can
  use it instead of an exported sync file. Opaque object IDs and snapshots include a never-reused
  store generation; a foreign or stale object cannot alias a new one.
- `arm` uses G8c FenceWait, returning Done or one owned wake-channel read end. The caller must
  bound its Port wait with a timer and close the wake on every path. After any wake, re-arm and
  require Done/status 0; a wake, timeout or peer close alone cannot complete a payload. There is
  no SyncRead loop.
- Bounds are 64 live objects and 256 unobserved timeline points per object. CPU timeline signal
  conservatively returns Busy while GPU points remain unobserved; it cannot bypass GPU work.

```sh
cargo test -p kumo-gpu
python3 scripts/mesa-sync-smoke.py --run
```

The QEMU proof runs the real codec, Session and native client over a modeled GPU producer,
with real kernel VMO, wake-channel, Port and timer objects. It checks accepted versus completed
signals, snapshots, rejected-submit rollback, bounded timeout/abandonment and terminal loss.
It does not execute GPU work. Blocking/wait-many policy, native C callback
exposure, group fatal/timeout reporting and real-driver integration remain G7p/GL1/VK2. The
C ABI stays 12; no DRM FD, sync-file operation or new wire opcode is introduced.
— MILVUS, 2026-11-01

## G7o-f: the device, BOs and VM on metal

`device-metal.c`/`.rs` run as the Orange Pi's GPU client, staged in place of `bin/gpu-client`,
over the live drv-gpu session. Sora passes that session in x0, which relibc would consume, so
`device-metal-crt0.S` keeps x0/x1 and starts relibc on an empty, closed startup channel. Lines
go to the kernel log, because a Sora-launched client has no stdio.

What one boot proves on hardware:
- the published props equal J742's attestation;
- five Mesa BOs exist, the 64 MiB one among them, and every page is filled, remapped and
  verified through real DMA pages;
- five binds go through `pan_kmod_vm_bind`;
- GPU `SYNC_SET32` stores through Mesa bindings land in the data BO and on 17 pages across
  the 64 MiB;
- a live unbind and rebind work;
- a store aimed at an executable binding faults (`PERMISSION_FAULT Write`). drv-gpu then
  contains the close with `reset=ok`, and the executable bytes are unchanged.

`Buffers::native_client` reaches the session's group, submit and sync operations, which the
bridge does not model until G7p.

Two facts for later slices:
- **`SYNC_SET32` writes a 32-bit sync object, `{value, status}`**: the value at the target
  and status 0 in the next word. It is not a bare 32-bit store.
- **KUMO's Submit reads its stream from a read-only binding, and through the bridge only
  EXECUTABLE BOs bind read-only.** Gallium and panvk keep command streams in writable,
  non-executable pools, so GL1 must settle that rule. Settled by J758: writable no-execute
  stream bindings are accepted (see GL1-a).

— TIMBERDOODLE, 2026-11-01

```sh
python3 scripts/mesa-device-metal.py --stage build/proofs/g7o-f/opi  # never serves it
python3 scripts/mesa-device-metal.py --check <tio log>                 # judges the log's last boot
```

Since J753, `--stage` builds the image with `KUMO_CONSOLE_LOGLEVEL=5` (see G7p-e).

## G7o-e: W^X against both drivers

`WX-INVENTORY.md` is the measurement. Gallium has two executable pools and panvk one. All three
hold CPU-copied shader binaries only and are mapped RW/shared/null, never PROT_EXEC. Panthor
binds them read-only by the same rule as KUMO, so neither driver needs a writable executable
GPU mapping. On KUMO's props panvk's `EXECUTABLE | WB_MMAP` request becomes `EXECUTABLE`.

The one new constraint is placement. Valhall's blend descriptor keeps 32 bits of the blend
shader's PC, and Gallium on v10 truncates it unchecked. All executable BOs must therefore share
one 4 GiB region. The service window is now asserted to be one region, and the device factory
refuses a reported window that crosses a 4 GiB boundary. An auto-VA backend must keep this.
Two non-executable panvk map shapes, placed maps and single-protection host-copy maps, are
left for VK2. — TIMBERDOODLE, 2026-11-01

```sh
python3 scripts/mesa-wx-audit.py           # pinned tarball, no container or network
python3 scripts/mesa-device-smoke.py --run # mode 8: the drivers' executable BO shapes
```

## G7o-d: native device and immediate VM operations

The callback ABI is **12**, with a **112-byte API**; Info remains 104 bytes and the
property report remains 240 bytes. Seven new callbacks open/close the device,
create/destroy a logical VM, bind/unbind, and report known device loss. The
query-only and BO-only APIs leave those callbacks null. `with_device_api` owns one
session for its scope: all C objects/pointers must die inside it, calls are serial
and non-reentrant, and normal scope return always closes the endpoint once.

`pan_kmod_kumo_dev_create` explicitly selects `kumo` using this capability API,
without a DRM name, FD, ioctl or fabricated DRM version. It publishes the shared
complete property set unchanged, after validating every required/optional group
and opening the one service VM. The actual pinned Mesa initialization, allocators,
refcounts, handle table and cleanup helpers manage the device, BOs and VM. The
FD-based factory refuses. Generic `pan_kmod_bo_mmap` routes fd=-1 native BOs to
the callback, and unsupported sparse/async operations refuse before upstream
sparse emulation can allocate a dummy BO. — KESTREL, 2026-10-31

One live logical VM may lease an aligned subrange of the service window
`0x20000000..0x30000000`. This slice accepts manual VA allocation (VM flags zero)
and immediate map/unmap only. Automatic VA and activity tracking, requested by
Gallium, require later G7p/GL1 work; this device is not yet a Gallium/panvk render
context. Logical VM IDs never repeat during the session. Exclusive BOs retain
that generation after VM destruction, so pointer reuse cannot reassign them.
Map rows enforce owner, range, offset and BO extent; Rust derives executable
READ_ONLY versus data NO_EXECUTE policy from immutable BO flags. Unmap requires
exact coverage by whole live bindings, with no holes. — KESTREL, 2026-10-31

Successful operations commit metadata only after the service acknowledges them.
A batch that fails after a successful prefix closes the session and exposes a
faulty VM. BO/VM void destructors do the same on failure. Device destruction
invalidates all children, drops local aliases/grants, and closes the owned endpoint;
drv-gpu retains DMA backing until reset establishes quiescence. No destroy replay,
fake wait completion, sync object, group, submission or hardware execution is added.

```sh
python3 scripts/mesa-device-smoke.py --run
```

The proof uses the real generic Mesa lifecycle and a modeled GPU service through
the actual codec/Session, backed by real kernel VMOs and NC mappings. Eight cases
cover complete/incomplete/failed properties, C allocation failure, refcounts,
map/unmap/remap, offset binds, VM subranges/recreation, exclusive generations,
unsupported modes, partial bind failure, BO/VM destructor failure and destruction
with live children. It also checks real native-channel send/receive/idempotent
close. Both complete GL/VK link graphs include the backend; GPU metal acceptance
remains G7o-f. — KESTREL, 2026-10-31

## G7o-c: native BO allocation, destruction and CPU mapping

The borrowed callback ABI is version **11** (56-byte API); its existing 104-byte
property result and 240-byte report retain their layouts. Four optional callbacks
create, destroy, map and unmap BOs. `with_api` stays query-only;
`with_buffer_api` supplies all four over a Rust `bo::Buffers` store. Kernel VMO
handles remain in Rust. C sees a session-local ID, flags, rounded size and a scoped
CPU pointer. Query and BO callbacks use the same client and request sequence.

`kumo_kmod.h` embeds the pinned Mesa `pan_kmod_bo` in caller-owned native storage.
Its allocation/free/mmap/munmap helpers use the callback table directly. The
owner token rejects a foreign context before I/O. This BO is unpublished:
`base.dev` and `exclusive_vm` remain null, and generic Mesa device helpers require
G7o-d. A service VM must already exist before BO allocation. — KESTREL, 2026-10-31

- Only EXECUTABLE, NO_MMAP and GPU_UNCACHED are accepted, in any combination.
  Zero, overflowing and over-limit sizes refuse before I/O; other sizes round up
  to the 4 KiB page granule.
- CPU mapping is lazy and covers the complete BO with RW Normal-NC data pages.
  Repeated mmap returns the existing pointer; munmap is idempotent and remapping
  preserves the VMO contents. NO_MMAP refuses before mapping. The native helper
  accepts the drivers' `PROT_READ | PROT_WRITE`, `MAP_SHARED`, null-address call;
  other protections, private/fixed requests and CPU execution refuse explicitly.
- GPU bind policy is READ_ONLY for EXECUTABLE, NO_EXECUTE otherwise, plus UNCACHED
  for GPU_UNCACHED. Actual VM binding and both drivers' W^X acceptance remain d/e.
- Free first confirms BufferDestroy, which orders GPU work and unbinds in the
  service, then unmaps the CPU alias and closes the local VMO grant. Recoverable
  failures keep the BO retryable. If unmap fails after the destroy acknowledgement,
  retry finishes local cleanup without issuing a second destroy.
- Scope abandonment marks the client lost, releases local mappings/grants, and
  sends no destroy replay. The caller must close the endpoint and reconnect;
  drv-gpu retains its backing until contained reset proves quiescence. A failed
  CPU unmap retains the kernel mapping's pin. — KESTREL, 2026-10-31

```sh
python3 scripts/mesa-bo-smoke.py --run
```

The runner verifies the pinned header/source and existing property-link closure,
then links the real C BO helpers into a native Rust test executable. The service
is modeled through the real wire codec and Session; allocation and mapping use
real kernel paged VMOs and `native.rs`'s kernel adapter. It checks all eight flag
combinations, every page's first/last words across unmap/remap, a 4 MiB + 1-byte
request, stale/foreign rejection, exact request/handle/map counts and zero live
bytes at close. It publishes no device and claims no GPU hardware execution.
— KESTREL, 2026-10-31

## G7o-b: service memory for real driver allocations

The native channel now allows 64 MiB per BO, 128 MiB live per session, 256 bindings,
and a 256 MiB GPU VA window (`0x20000000..0x30000000`). These are bounded bring-up
limits, not a promise that every arbitrary GL/Vulkan workload fits. Mesa 26.2.5's
v10 drivers use 64 KiB command rings and geometry storage, 16/64 KiB descriptor and
shader slabs, and 2 MiB tiler chunks; images and buffers can exceed the former
2 MiB cap. The 128 MiB legacy tiler BO in both sources is gated to PAN_ARCH <= 9,
and is not part of the G610/v10 requirement. — KESTREL, 2026-10-31

BufferCreate now uses `VmoCreatePaged` (syscall 55): independent zeroed/cleaned DMA
pages, one shareable VMO, and one contiguous Normal-NC CPU mapping. The physical
page list is returned to the allocating driver only; the GPU channel still returns
an ID, size and VMO grant. VmBind resolves offsets in that list and writes individual
GPU leaves. The driver reserves enough table pages to visit the entire VA window;
binding metadata and temporary unbind batches live on the heap. Reset-before-release,
checked unbind/flush, live-byte accounting and monotonic IDs keep their existing contract.
A failed allocation returns its entire prefix to the DMA free list. Duplicates, CPU
mappings, queued transfers and retained kernel references prevent reclamation.

```sh
python3 scripts/gpu-memory-smoke.py --run
```

This builds a small native Rust probe and copies the QEMU image into a disposable
ESP. It forces physical holes with live guard VMOs, allocates 64 MiB, checks both
ends of every page through two shared NC aliases, closes one owner while retaining
the other, and requires the same pages back zeroed after final release. Host tests
cover the GPU table walk, offsets, table capacity, the budget and binding quotas,
and failure/close accounting. GPU hardware acceptance remains G7o-f; Mesa's BO and
VM backend operations remain G7o-c/d. — KESTREL, 2026-10-31

## G7o-a: the property set is complete

GetMemoryProperties (opcode 17) attests `L2_FEATURES` (+0x004), `MEM_FEATURES` (+0x010),
`COHERENCY_FEATURES` (+0x300) and the `GPU_COHERENCY_PROTOCOL` value drv-gpu programs before L2
power-up (31, "none"). All four words are required and cross verbatim. drv-gpu already read
and logged the three registers at bring-up (J653); it now serves them.

`kumo_kmod.c` builds one complete `pan_kmod_dev_props`, which G7o-d's device will publish
unchanged:
- `l2_features` and `mem_features` are attested; upstream's `pan_query_l2_slices` and
  `pan_query_bus_width` derive 4 slices and a 128-bit bus from the board's words;
- `is_io_coherent` is upstream's `protocol != none`, so false on the Orange Pi;
- `pgsize_bitmap` is the protocol's 4 KiB page granule, carried from `kumo_gpu::PAGE_BYTES`
  and the only leaf size drv-gpu writes;
- KUMO's stated policy, which the later BO and VM slices must honor: MEDIUM priority only
  (GroupCreate carries none); Panthor's base BO flags EXECUTABLE, NO_MMAP and GPU_UNCACHED,
  with no WB_MMAP because CPU mappings are non-cacheable; no VM op flags (no sparse); no
  timestamps.

The missing mask therefore reaches zero for a device whose optional groups are present.
Only bits 1, 3 and 4 can remain, for an absent or unusable tiler, texture or thread group.
`kumo_mesa_probe` still returns INCOMPLETE and publishes no device: Device, VM and group operations
are not published yet. The live gpu-client logs the memory group on every boot, decoded as upstream
does. — TIMBERDOODLE, 2026-10-31

## G7n: the compiler runs on KUMO

After G7m, keep the offline driver container running:

```sh
python3 scripts/mesa-compiler-smoke.py --run
```

`build-compiler.py` builds one harness twice from the same Mesa build: for KUMO
on relibc, and natively for Linux as the reference. It runs three compiles, all
for v10 (GPU_ID `0xa8670005`):

- **SPIR-V → NIR → Valhall.** Mesa's own `panfrost_compile`
  (`src/panfrost/clc/pan_compile.c`, compiled unchanged) on the shader recipe's
  `kumo_paint` SPIR-V. On Linux its two outputs must equal the recipe's own
  `panfrost_compile` byte for byte.
- **GLSL ES 3.10 → NIR → Valhall**, twice. Mesa's standalone GLSL front end and
  linker run first. The state tracker's compute lowering follows, then Gallium
  panfrost's compute compile in `panfrost_shader_compile`'s order.
  `g7n/paint.comp` is the kumo_paint pattern in GLSL. `g7n/math.comp` adds
  shared memory, barriers, a reduction, and a constant expression folded with
  the build's libm (openlibm on KUMO, glibc on Linux).

KUMO must print every Linux line exactly: size, BLAKE3 of the output, work
registers, TLS, WLS and sysvals.

`pan_compile.c` reads and writes files. KUMO's root volume is read-only and
relibc's `mmap` is anonymous-only. So its file calls are renamed in the object
(`llvm-objcopy --redefine-sym`) to an in-memory layer in `compiler-smoke.c`.
glibc's headers spell them `open64`/`fopen64`/`lseek64`/`mmap64`, so both
spellings are renamed.

The GLSL compiler outran the 16 KiB main stack every KUMO program had. The
harness asks for 2 MiB through `PT_GNU_STACK`, and Sora now honours its
`p_memsz` (16 KiB floor, 8 MiB ceiling, refused above). With an explicit
`PHDRS` block lld emits the header but leaves the size zero, so the build
writes the field after linking and checks it with llvm-readelf.

CPU only: no GPU device is opened and no compiled binary runs. QEMU and Orange
Pi 5 Plus metal both printed the Linux reference byte for byte (J741). Outputs,
the Linux transcript and the manifest live in
`build/mesa-drivers/result/compiler/`. — TIMBERDOODLE, 2026-10-31

## G7m: OS closure and Mesa utilities

After G7k preparation and G7l, keep the offline driver container running:

```sh
scripts/relibc-headers.sh
python3 scripts/mesa-util-smoke.py --run
```

The closure recipe synchronizes the actual SDK, rebuilds affected Mesa archives
and the measured C++ runtime, then links both complete probe graphs without
ignoring undefined symbols. It retains only the previously inventoried optional
weak dispatch/TLS references. Both links have zero required gaps.

`drm-refusal.c` exposes the measured libdrm signatures only under `__kumo__`.
Every device, ioctl, BO/FD, syncobj and sync-file operation refuses with ENOTSUP;
outputs remain untouched. No integer FD becomes a capability. Null cleanup is
harmless; attempting to free a live foreign DRM object aborts. These entry points
keep retained DRM code explicit until native seams replace their callers.

`static-build-id.c` reads the linker's actual GNU note from mapped program
headers in a static ET_EXEC image. Its utility target preserves that note in a
read-only PT_LOAD and PT_NOTE and uses LLD's SHA1 build ID. The runtime ID must
match llvm-readelf byte for byte; foreign addresses have no ID. The relibc
overlay also corrects Elf64_Word/Sword to their 32-bit ELF widths, with compile-time
size and offset checks at the consumer.

The QEMU proof executes Mesa's ralloc ownership/destructors, collision-heavy
hash insertion/growth/removal, stable sparse storage across distant indices,
dynarray growth, simple mutex/once/once-data, and C++ TLS sort callback. It also
checks every refusal, the actual Mesa DRM factory, and relibc's existing dlopen
refusal/error reset. It creates no thread and makes no concurrency claim.
Build manifests, both probe links/maps/extraction traces, note bytes and UEFI
proof logs live in `build/mesa-drivers/result/closure/`.

G7m is closed by the passing OS/util proof (J740). Runtime GLSL/SPIR-V compiler
acceptance (G7n), native device/memory/sync/heaps (G7o–G7q), and Gallium rendering
remain later gates. Closure decisions — SAKER, 2026-10-31.

## G7k: both complete driver graphs cross-build

`scripts/mesa-drivers.py` builds Gallium panfrost, the Mesa state tracker/GLSL
compiler/GLES2 dispatch, and panvk together for `aarch64-unknown-none`, with
`host_machine.system = 'kumo'`. This is a separate compile/link inventory; the
native property probe below keeps its original scope. No target probe ELF is
executed, and no kernel, runtime, image or served payload changes in G7k.

```sh
scripts/relibc-headers.sh
# Network is allowed only while preparing the pinned tool cache and original tarball:
python3 scripts/mesa-drivers.py --prepare
# Subsequent clean builds use the cache, with Docker networking disabled:
python3 scripts/mesa-drivers.py
```

Preparation extends the existing shader builder with clang/lld/libc++ headers
22.1.8-3 from the same signed Debian snapshot (20261024T000000Z), builds the
native `vtn_bindgen2`, then snapshots that tool cache. The source tarball's full
SHA256 is checked before every extraction. The actual build runs in a separate
`--network=none` container with a read-only repository mount and Meson
`--wrap-mode=nodownload`. Python/Mako, native compiler tools and every package
version are recorded. `--keep` leaves this offline container running for the
following slices; remove `kumo-mesa-drivers-g7k` before starting another clean run.
No Linux library or Linux host-header directory is passed to the target compiler.
The Debian clang driver uses `aarch64-none-elf` only for configure-test links so
that it selects LLD directly; every target translation unit uses
`aarch64-unknown-none`. C++ uses `-fno-exceptions -fno-rtti` and libc++ headers
configured for relibc's pthread declarations, without localization or parallel
algorithms. The Linux-built libc++ binary is never linked.

The checked [driver inventory](DRIVER-INVENTORY.md) records the measured result:
53 required GL gaps, 44 required Vulkan gaps, and the requesting archive/object.
`build/mesa-drivers/result/` contains all static archives, diagnostic ELFs,
linker errors/maps/extraction traces, per-object sizes, the exhaustive
`inventory.json` and `optional-undefined.tsv`, and input/tool/archive hashes.
There is one optional GL TLS wrapper and 6,292 optional Vulkan dispatch symbols;
each is verified against a weak reference in an extracted object. They are
accounted separately from required failures. Every symbol must be classified or
the recipe fails. The plan's glapi root has the pinned source's actual name,
`_mesa_glapi_get_dispatch`.

`kumo-build.patch` is applied to the pristine source, independently of the older
`native-header.patch`. It admits generic POSIX utilities on KUMO, uses relibc's
existing C11 threads instead of Mesa's duplicate fallback, exposes build-ID
declarations without providing bodies, retains the DRM modifier metadata that
panvk requires, supplies libdrm declarations only, and creates static GLES/panvk
targets. All upstream GPU architecture variants are compiled and linked where
their driver tables reference them, including v10. The legacy JM Linux kernel
backend `panfrost_kmod.c` alone is excluded for KUMO because its performance
session needs eventfd/timerfd; the Panthor backend, both real drivers, their
compilers, and their CSF operations remain in the inventory. Optional standalone
compiler scaffolds, test libraries, video implementations and alternative lite
Vulkan runtimes are outside the driver link graphs.

The recipe repairs only its private header copies: classification declarations
are protected from macro expansion and libc++ overload conflicts, missing
`HUGE_VAL`/`NAN` definitions use compiler constants, openlibm fenv headers are
installed from the pinned relibc submodule, and a compile-only header declares
`open_memstream` while importing the existing `alloca`/strings declarations.
No missing function is stubbed successfully. `open_memstream`, math exports,
the measured C++ runtime, DRM/sync seams and static build IDs remain explicit
follow-up work. Link success against an existing libc export is not proof that
the operation succeeds at runtime. G7l/G7m must establish that separately.

G7k build and inventory decisions — SAKER, 2026-10-31.

The original scaffold documentation follows.

This is a compile/link scaffold, not a usable Mesa device or a rendering driver.
The probe compiles the actual pinned `pan_kmod.h` and `pan_props.c` for
**aarch64-unknown-none** against
KUMO's real relibc headers, links C → Rust → C callbacks into a static KUMO ELF,
and can run it as a separate process in a disposable full UEFI QEMU image.
Its transport uses the real GPU `Session`/wire implementation with **modeled hardware**.
It never connects to a GPU, allocates device memory, submits work or changes a served image.

The deliberately unregistered `pan_kmod_ops` scaffold has a type-checked `dev_create`
that always refuses. No half-initialized `pan_kmod_dev`, zero-filled feature report,
FD-to-capability cast, fake mutex or successful unsupported operation is returned.
The native identity/topology query bridge and Mesa's two shader-topology helpers are
implemented. The full G7 gate remains open.

## Reproduce

The source is the same Mesa 26.2.5 original tarball used by the offline shader recipe
(Debian package 26.2.5-1). SHA256:
`794b3d1729075dae7ebd47c7283b7ca0cdb5dec668f7f11cc0a5d82f6138a0a5`.
The pristine `src/panfrost/lib/kmod/pan_kmod.h` SHA256 is
`55fa7f89e5728a209fb615a26ee4099733bba29cee0695c754878f0c5dc6c4c4`.
No Mesa source checkout or Linux driver implementation is vendored. The tarball's
original copyright/license files remain in the private extraction. `NOTICE` accompanies
the small header patch; KUMO owns the new C/Rust bridge and probe.

If the shader compiler cache is absent, build it once using the existing pinned recipe:
`KUMO_MALI_SHADERS_KEEP=1 scripts/build-mali-shaders.sh`. With the existing cache:

```sh
mkdir -p build/mesa-native
# Start OrbStack first if needed. Preserve the existing task container/cache.
docker start kumo-mali-shaders
docker cp kumo-mali-shaders:/work/mesa_26.2.5.orig.tar.xz build/mesa-native/mesa.tar.xz
docker stop kumo-mali-shaders
scripts/relibc-headers.sh
python3 scripts/mesa-native-probe.py
# Requires the normal full QEMU image, without modifying it:
cargo xtask image --arch aarch64 --hardware qemu-virt-aarch64
python3 scripts/mesa-native-probe.py --run
```

Alternatively supply that exact original archive with `--source-tar /path/to/archive`.
The script makes no network requests and rejects the wrong tarball before extraction.
The local stable Rust toolchain, clang, ld.lld and llvm-nm are required. Their versions,
the libc archive and all included header hashes are recorded in
`build/mesa-native/probe/manifest.json`; compiler bytes are not newly pinned by this slice.
`--run` uses the same local Homebrew QEMU/EDK2 setup as `scripts/thread-tls-smoke.py`.
The probe replaces only `bin/relibc-smoke` and autoexec in its **disposable ESP copy**.

Acceptance checks real cross-language ABI layout, wrong version/size/null rejection
before I/O, recoverable Busy followed by two queries with increasing IDs, continued
Rust use of the same client, rejected-handle closure, and latched loss without replay.
The output explicitly says `modeled service`. A positive incomplete result is acceptance
of this scaffold's refusal, not successful Mesa device creation. G7c additionally executes
the unmodified Mesa helpers on contiguous, sparse, high-bit and full64-bit masks, and
checks invalid zero topology, cleared failure output and unchanged loss behavior. G7d
additionally executes the unmodified `pan_kmod_dev_query_user_va_range` on attested
MMU_FEATURES words, and checks that a width which cannot hold the service window, or
which would make Mesa's own `1ull <<` shift undefined, is refused with nothing published.
The separate Orange Pi demo queries the real server mask; modeled QEMU coverage is recorded
separately from hardware evidence.

## Native seam and runtime closure

`lib/kumo-gpu/src/c.rs` and `kumo_gpu.h` share a versioned callback descriptor and a
104-byte result (private C ABI version11; older versions are refused). The C-owned report the
bridge fills is 240 bytes; `has_afbc` carries upstream's AFBC constant rather than a hardware
attestation, and G7o-a's block follows it. `with_api` lends an **existing** typed client for
synchronous serial queries. Each callback performs GetInfo, GetTopology, GetMmuFeatures,
GetTilerFeatures, GetTextureFeatures, GetCoreFeatures, GetThreadProperties then
GetMemoryProperties, preserving the same client and consuming eight request IDs on success.
The result is published only after all eight succeed; a failed later query leaves no partial
identity. NotSupported from an optional query is the one status that does not abort it;
core features and memory properties are required, so any failure of either aborts the whole
report, and any thread-query failure other than NotSupported aborts it too. The thread group is optional:
NotSupported leaves its three words zero and missing-property bit 4 set. The opaque context may not be retained, raced, reentered or unwound. It carries
no FD, OS handle, Rust layout or new authority. The Rust owner keeps endpoint and grant
cleanup responsibility. G7a's request ID, correlation and loss rules remain authoritative.
Numeric query errors distinguish invalid arguments, invalid request, transport, protocol,
loss and server status; the result is cleared on operational failure. No retry occurs.

`native-header.patch` excludes the generic DRM factory/ioctl, FD import/export and
FD-backed mmap helpers under `__kumo__`, plus their unneeded DRM/mman includes. Missing
native implementations remain compile/link errors if called. The actual Mesa structures
and vtable remain unchanged. Generic kmod sources and the Panthor DRM backend are not linked.
No Linux preprocessor identity or DRM emulation is used.

Compilation uses real relibc pthread declarations because Mesa's embedded lock structures
require them (`HAVE_PTHREAD`). This does **not** initialize a Mesa lock or prove pthread
execution. `HAVE_ENDIAN_H` and `HAVE_STRUCT_TIMESPEC` describe actual installed headers.
The scaffold C object refers to exactly nine upstream functions: `pan_query_core_count`,
`pan_query_core_id_range`, `pan_query_tiler_features`, `pan_query_compressed_formats`,
`pan_query_afrc`, `pan_query_afbc`, since G7g `pan_get_model`, and since G7o-a
`pan_query_l2_slices` and `pan_query_bus_width` (both in the already-linked `pan_props.c`).
All nine must appear defined in the final
native ELF, which has no unresolved symbols. G7h deliberately adds none: the thread
derivations are upstream's expressions applied to the attested words, and the optional
`pan_query_thread_tls_alloc` helper is not called, because the report already records its
value (`max_tls_instance_per_core`, the documented fallback to `max_threads_per_core`) as a
field rather than a returned device property. G7j's `pan_query_afbc` also lives in that same
`pan_props.c` and consumes upstream's constant `.afbc_features = 0`, so it likewise adds no
translation unit. G7d's
`pan_kmod_dev_query_user_va_range` is `static inline` in the pinned header, so it adds no
entry to that inventory; G7e's tiler helper is a real function in the already-compiled
`pan_props.c`, which is why the asserted set grew from two to three and no new translation
unit was added. G7j's `pan_query_afbc` also lives in that same `pan_props.c` and consumes
upstream's constant `.afbc_features = 0`, so it likewise adds no translation unit. G7g's
`pan_get_model` lives in `model/pan_model.c`, so that upstream
translation unit is now compiled and linked too. `pan_props.c` and `pan_model.c` are
compiled unchanged from the verified
archive; `common_pack.h` is generated by that archive's own Python/XML source. Only
upstream's unused-parameter warning is suppressed for that upstream translation unit;
KUMO's C scaffold retains Wall/Wextra/Werror. No pthread, DRM or allocator operation
is added by these helpers. Recheck these inventories before adding runtime calls. The executable
also links the real relibc startup/stdio/TLS path, Rust core and compiler builtins.

A probe-private `math.h` copy parenthesizes four existing function declarators
(`isfinite`, `isinf`, `isnan`, `isnormal`) to avoid expansion by their own function-like
macros. Types, macros and function bodies are unchanged; the installed sysroot and
upstream checkout are immutable. The script checks each exact old declaration before
applying this narrow workaround. A general header-generator repair is separate work.

## Properties and operations still missing

GetInfo supplies GPU_ID and CS_FEATURES. Additive GetTopology (opcode8) supplies the
nonzero64-bit shader-present mask attested by driver bring-up and used in its CSG masks.
Additive GetMmuFeatures (opcode9) supplies the nonzero raw MMU_FEATURES word that bring-up
already read, whose [7:0] field is the hardware virtual-address width; the word crosses
undecoded so the derivation stays upstream's. Additive GetTilerFeatures (opcode10) supplies
the nonzero raw TILER_FEATURES word from GPU_CONTROL +0x00C, whose [4:0] field is a log2 bin
size and [11:8] the level count. Unlike the other properties this one is **optional**: an
absent word is reported as NotSupported, the query still succeeds with `tiler_features` 0,
and the tiler group stays in the missing mask. Additive GetTextureFeatures (opcode11) supplies
the raw four-word `TEXTURE_FEATURES[0..3]` array from GPU_CONTROL +0x0B0..+0x0BC under the same
optional rule; the driver reads all four words in the same powered block and the wire carries
them verbatim. Only element 0 is an absence gate: upstream returns it as the compressed-format
mask and tests bit 25, behind its own arch check, for AFRC, so a zero there means the backend
has no attestation, the query still succeeds with all four words zero, and the group stays in
the missing mask. Elements 1..3 cross verbatim, zero included — they are attested values, not
derived, and no consumer here decodes them. A successful query therefore attests the whole
group and missing-property bit 3 clears. Additive GetCoreFeatures
(opcode12) supplies the raw CORE_FEATURES word from GPU_CONTROL +0x008 under the **required**
rule of topology and MMU features: the query must succeed — bits [7:0] are the GPU variant —
but the word crosses verbatim, zero included. The Orange Pi 5 Plus measures exactly
0x00000000, and its variant byte 0 is the pinned table's G610 entry, so the measured word is
the resolving one. The bridge derives the variant with upstream's own
expression (`core_features & 0xff`, verbatim from `panthor_kmod.c`) and records whether
`pan_get_model` finds a table entry for `(gpu_id, variant)` in `model_resolved`; the pinned
tree's G610 entry carries variant 0, and a table miss is reported, not failed. Missing-mask
bit 0 clears on every successful query because the word is required.
Additive GetThreadProperties (opcode13) supplies the raw `THREAD_MAX_THREADS` (GPU_CONTROL
+0x0A0), `THREAD_MAX_WORKGROUP_SIZE` (+0x0A4) and `THREAD_FEATURES` (+0x0AC) words under the
same optional rule as tiler and texture features: all three zero means the group is absent,
the backend reports NotSupported, the query still succeeds with three zero words and
missing-property bit 4 stays set. The bridge derives the five `pan_kmod_dev_props` fields
with upstream's exact expressions from `panthor_dev_query_thread_props`: the first two words
become `max_threads_per_core` and `max_threads_per_wg`, `max_tasks_per_core` is
`THREAD_FEATURES >> 24`, `num_registers_per_core` is `THREAD_FEATURES & 0x3fffff`, and
`max_tls_instance_per_core` copies `max_threads_per_core` because there is no TLS register on
v10+. Upstream answers a zero derivation with an assert; KUMO deliberately does not. Aborting
would discard every other attested property, so the derived values are reported verbatim and
bit 4 stays set, marking the group unusable instead. The report grew to 168 bytes: the
enlarged 80-byte `kumo_gpu_info` shifts the whole report, so the five new fields sit at
offsets 144–160; the four bytes that were tail padding after G7h now carry G7j's `has_afbc`.

AFBC is the one discovery group with no hardware register at all. The pinned
`panthor_kmod.c` sets `.afbc_features = 0` as a constant with the comment "This register does
not exist because AFBC is no longer optional", so the bridge does the same and consumes it
through upstream's own `pan_query_afbc()`, which returns true for `pan_arch(gpu_id) >= 5` when
the property is 0. The boolean reaches the report as `has_afbc`, and missing-property bit 5
clears only when that helper returned true — never merely because the query succeeded. This is
service-side knowledge the way upstream has it, not an attested hardware word, so no wire
opcode or `Info` field changes for it.
No additional MMIO read or client-selectable register is introduced. The bridge also reports the
**service's** admitted VA window and maximum buffer size. Those remain service policy rather
than hardware reports, and are now distinct from the attested MMU_FEATURES word the consumer
derives the hardware range from. Coherency, memory-type and shader feature reports are still
absent. The scaffold never initializes
`pan_kmod_dev_props` from guesses. The missing-property mask groups its remaining fields:

| Bit | Fields in the pinned Mesa structure |
|---|---|
| 0 | gpu_variant |
| 1 | ~~tiler_features~~ supplied by G7e; shader_present by G7c — cleared only when a shape is derived |
| 2 | ~~mem_features, l2_features~~ supplied by G7o-a over GetMemoryProperties (mmu_features supplied by G7d); required, so the bit clears on every published report |
| 3 | ~~texture_features[4]~~ supplied by G7i — all four words read in one powered block; bit clears only when the query succeeded |
| 4 | ~~max_threads_per_core, max_tasks_per_core, max_threads_per_wg, num_registers_per_core, max_tls_instance_per_core~~ supplied by G7h; cleared only when all four upstream-asserted derivations are nonzero |
| 5 | ~~afbc_features~~ supplied by G7j as upstream's constant, consumed through `pan_query_afbc`; there is no hardware register |
| 6 | ~~gpu_can_query_timestamp, timestamp_device_coherent, timestamp_frequency, timestamp_cycles_to_ns_factor~~ G7o-a policy: no timestamps (the protocol has no timestamp query), all zero |
| 7 | ~~allowed_group_priorities_mask~~ G7o-a policy: MEDIUM only, because GroupCreate carries no priority |
| 8 | ~~supported_bo_flags, supported_vm_op_flags~~ G7o-a policy: EXECUTABLE, NO_MMAP, GPU_UNCACHED; no VM op flags |
| 9 | ~~is_io_coherent, pgsize_bitmap~~ G7o-a: upstream's `protocol != none` from the programmed protocol (false), and the protocol's 4 KiB granule |

This inventories the structure, not a promise that the first renderer needs every field
or that optional timestamp/performance features must be enabled. G7c selects Panfrost device discovery as the first consumer: its `pan_device.c`
uses `pan_query_core_count` and `pan_query_core_id_range`; `pan_screen.c` also uses the
count in the renderer name. Both helpers read only `shader_present`. A private partial
property value is passed only to those two functions and is never published as a device.
G7d extends that same private partial input with `mmu_features` and hands it to the pinned
header's `pan_kmod_dev_query_user_va_range`. The scaffold's ops table deliberately leaves
`dev_query_user_va_range` NULL, so upstream's own fallback derives `{start 0, size 1 << va_bits}`
from the attested word and KUMO computes no range itself. That partial device is an input to
this one inline helper and is never returned, stored or published. The derived range must
contain the service's own window or the bridge refuses, because an attestation that cannot
hold the window is a contradiction rather than a narrower device.
G7e extends the same private partial input with `tiler_features` and calls the real
`pan_query_tiler_features` from the already-compiled `pan_props.c`, reporting the derived bin
size and level count. Two guards keep that honest. A zero word means the shape is unknown
rather than the device unusable, so the backend reports NotSupported, the query still succeeds
and the tiler group stays in the missing mask; the compute path never depended on tiler
properties. And a bin-size field of 31 is not decoded at all, because upstream computes
`1 << (raw & 0x1f)` with a signed `int` and that shift would overflow — the shape degrades to
unknown instead. This matters more here than for the earlier properties: RESEARCH/009's oracle
table has never recorded a measured TILER_FEATURES, listing `0x809` only as a recollection, so
the board's actual value is what this slice measures.
The next slice obtains the remaining required discovery fields from attested hardware
or native service policy before admitting full device initialization. Optional features may be explicitly unsupported; do not fabricate
hardware values. CS_FEATURES also does not replace Panthor's full CSIF property structure.

Required later operations include native BO allocation/CPU mapping/ownership; VM creation,
VA policy, bind and safe unbind/destroy; native queue/group creation, Submit and observed
completion; and explicit synchronization/cache visibility. Optional FD/dma-buf import,
export, performance counters, timestamps, sparse mappings, eviction and asynchronous VM
operations remain unsupported unless selected and separately implemented. The existing
service admits one cooperative session with fixed bindings and retains DMA backing to
reboot. Freeing a C wrapper cannot change that lifetime.

Before Mesa initialization is admitted, compile and implement the required allocator,
mutex/once, sparse-array/dynarray and runtime-symbol closure. Before G7 closes, show actual
Mesa-rendered output through the native service and prove quiescence plus a fresh usable
session after device loss. No pthread creation/join, Shanghai worker/queue or Shinjuku module
work is included here. — KESTREL 2026-10-27
