# QEMU patches

Patches for QEMU 11.1.3 that the host running qemu-dbus-webrtc applies. QEMU works without
them, with the issues each one fixes. They are made with `git format-patch` and apply in order
to the QEMU 11.1.3 sources:

```
cd qemu-11.1.3
for p in /path/to/qemu-dbus-webrtc/packaging/qemu/*.patch; do patch -p1 < "$p"; done
```

| Patch | Fixes | Status |
|---|---|---|
| 0001 `ui/dbus: disable scanout iff dmabuf is current` | With a 2D virtio-vga and `blob=on`, the picture freezes: releasing an older DMA-BUF disables the current scanout. | Upstream (commit `5378d1da44`, August 2026); not needed with a QEMU that includes it. |
| 0002 `ui/dbus: don't assert on tex_id ...` | QEMU aborts when a guest resets a GL virtio-gpu (a Windows display driver reloaded). | Local. |
| 0003 `virtio-gpu: give each output its own EDID serial number` | All screens have the same EDID identity: Windows cannot tell them apart. | Local. |
| 0004 `hw/display/edid: fit the range limits to the preferred mode` | At the default 75 Hz, a 3840x2160 screen is outside the EDID's horizontal rate limit (168 kHz against 160): Windows can drop the mode asked for. | Local. |

The EDID that 0003 and 0004 generate can be checked with QEMU's `build/qemu-edid -x <width>
-y <height>`.

These patches modify QEMU, and are under QEMU's license, the GNU General Public License,
version 2 or later, not under this project's Mozilla Public License.
