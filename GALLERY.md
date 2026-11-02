<!-- j764 -->

# KUMO gallery

[← Back to the project](README.md)

Screenshots and a terminal replay from KUMO's development. These show different stages
of the system, so console styling and diagnostics vary. The QEMU view captures use the
software GOP presentation path; they do not depict OpenGL or Vulkan rendering. Click
an image to open the original PNG.

## Titled views and the current console

[![Two titled KUMO views above the Maple Mono console and login prompt](resources/screenshots/titled-views.png)](resources/screenshots/titled-views.png)

A fresh UEFI boot in QEMU, with per-view title bars and the Maple Mono console face.
The graphics verification checks the view pixels, clipping, chrome, console text,
and arrival at login before capturing the framebuffer.

*QEMU · 800×600 · captured November 2, 2026 with `scripts/graphics-smoke.py --titles`.*

## Boot, views, and login

[![Two KUMO views above the boot console and INDRA login prompt](resources/screenshots/boot-and-views.png)](resources/screenshots/boot-and-views.png)

Two 256×128 views share the screen with the boot console. The left view contains a
client-rendered colour pattern; the right is a solid-blue demonstration surface.
Service startup continues below them to the INDRA login prompt.

*QEMU · 800×600 · supplied capture `pss1.png`.*

## A contained userspace fault

[![KUMO reports a contained microservice fault and continues startup to login](resources/screenshots/fault-containment.png)](resources/screenshots/fault-containment.png)

The TOWER diagnostic reports a fault in a userspace microservice. The kernel and the
remaining startup path continue: graphics, identity, and terminal services reach login.
This capture records an actual fault during bring-up.

*QEMU · 800×600 · supplied capture `pss2.png`.*

## An image as a userspace client

[![KUMO's bitmap viewer presents the white cloud and red KUMO wordmark in a view](resources/screenshots/bitmap-viewer.png)](resources/screenshots/bitmap-viewer.png)

`bmp-view` loads a selected BMP through an explicit file grant, decodes it, and presents
it in a client surface. Here it displays the KUMO cloud and wordmark. The console also
shows file/path refusal checks from the same verification run.

*QEMU · 800×600 · bitmap-viewer verification capture.*

[Bitmap formats, file paths, and capability handoff →](userland/bmp-view/README.md)

## Kilo across the full terminal

[![The Kilo text editor fills a 1600 by 900 KUMO terminal, including its status and help rows](resources/screenshots/kilo-editor.png)](resources/screenshots/kilo-editor.png)

The unmodified upstream Kilo C editor runs through KUMO's relibc and terminal services.
This replay fixture exercises a 200-column, 56-row terminal: document rows, status,
help, and cursor all fit the framebuffer instead of stopping at an 80×24 default.

*1600×900 · host framebuffer replay of terminal output captured from Kilo on Orange Pi.*

[How the Kilo port works →](userland/ports/kilo/README.md)
