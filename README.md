# Baxter's Screen Record — Linux

Records the screen to H.264/MP4, with a record space you can crop to any rectangle.
Modular Rust workspace, MIT licensed, FFmpeg dynamic linking, egui UI.

---

## ⚠ Requirements — read this first

**This build requires Ubuntu 26.04 or later, running a Wayland session.**

**It will hard fail on an Xorg (X11) session, deliberately.** BSR captures the screen
through the XDG desktop portal and PipeWire, which is a Wayland path. Under X11 there is
no portal route to the desktop, and a root-window grab returns **solid black with no
error** — so a recording would look successful and contain nothing.

Rather than produce that file, BSR refuses to start:

```
run.sh: this is an Xorg (X11) session. BSR requires Wayland — under X11 the
        desktop cannot be captured and a recording comes out solid black.
```

The capture backend refuses independently, so the guard holds however the app is launched.
Ubuntu 26.04 and later default to Wayland; if you are on an Xorg session, log out and pick
a Wayland one at the login screen.

For the same reason, **never set `GDK_BACKEND=x11`** for this app. `run.sh` scrubs it and
says so if it finds it — some terminals export it.

### Also required

| | |
|---|---|
| Session | Wayland, with `xdg-desktop-portal` and a backend (e.g. `xdg-desktop-portal-gnome`) |
| Capture | `gstreamer1.0-pipewire`, `gstreamer1.0-plugins-base` |
| Encoding | FFmpeg 8.x runtime — `libavcodec62`, `libavformat62`, `libavutil60`, `libswscale9` |

Installing the `.deb` pulls all of these in. The first recording shows GNOME's
screen-share prompt once; approving it caches a restore token so later runs are silent.

---

## Install

```bash
sudo apt install ./baxters-screen-record_1.0.0-1_amd64.deb
```

Installs to `/opt/baxters/screen-record/`. Launch from Activities, or:

```bash
/opt/baxters/screen-record/run.sh
```

**`run.sh` is the only supported entry point.** It scrubs Snap contamination, refuses an
Xorg session, and checks that `pipewiresrc` is present before starting. Running the binary
directly skips all of that.

Remove it completely with `sudo apt purge baxters-screen-record`.

## The record space

Records the full screen by default. To record less:

* **Type it** — Left/Top/Right/Bottom insets in the Record space panel.
* **Click it** — tick *Live view*, press a corner button, then click that corner on the
  preview.
* **From an agent** — set `capture.region` in the config file before launch, or send
  `IpcCommand::SetRecordSpace { region }` to a running instance. No pointer involved;
  corners may be given in either order and are normalised.

Sizes are rounded down to even numbers, because H.264's 4:2:0 chroma cannot represent an
odd width or height.

**A recording contains everything on screen**, including other windows. *Hide while
recording* (on by default) minimises BSR so it is not in its own shot; stop it again from
the tray icon.

## Build from source

```bash
sudo apt install libavcodec-dev libavformat-dev libavutil-dev libswscale-dev \
                 libswresample-dev libxdo-dev \
                 libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
                 gstreamer1.0-pipewire gstreamer1.0-tools
cargo build --release -p bsr-ui
cargo test --workspace          # 135 passed, 0 failed, 0 ignored
./packaging/deb/build_deb.sh    # -> dist/baxters-screen-record_<version>_amd64.deb
```

There is deliberately **no synthetic capture fallback**: with no real backend the crate
refuses to compile. A screen recorder that invents frames is worse than one that will not
start.

## Modules

| crate | |
|---|---|
| `bsr-core` | app shell, config, ring buffer |
| `bsr-capture` | capture backends — XDG portal + PipeWire on Linux, DXGI on Windows |
| `bsr-encode` | FFmpeg H.264 encoder |
| `bsr-mux` | MP4 muxer |
| `bsr-ui` | egui UI, tray, hotkeys |
| `bsr-ipc` | IPC and telemetry |
| `bsr-hrt` | Hot Rod Tuner integration |

## Licence

MIT. FFmpeg is linked dynamically and is LGPL — see `THIRD_PARTY_LICENSES`.
