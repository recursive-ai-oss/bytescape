# Measuring GPU drawing

Bytescape draws with the GPU when it can start one and with the CPU otherwise. This page explains how to measure the zoom animation through the GPU on a machine with a graphics card, and how to read the result. The numbers from a software adapter (a CPU pretending to be a GPU, such as llvmpipe) say nothing about a graphics card, and the tools label them that way.

## Which drawing is in use

- The window starts drawing with the CPU, so the first frame never waits for the graphics driver. The GPU starts on another thread once that frame is on screen, and drawing moves over when it is ready.
- If the GPU cannot start, or reports an error later, Bytescape keeps going with CPU drawing for the rest of the session and prints the reason on standard error. The same happens when five frames in a row take the GPU more than 100 ms each to present, which some displays cause (a remote X server without direct rendering, for example); a run that asked for the GPU with `--backend gpu` is never switched.
- The About box shows the choice on the line under the version: `Drawing: GPU (adapter name)`, `Drawing: software`, or `Drawing: software, because ...` after a failure.
- `--backend gpu|software|auto` on the command line, or the `BYTESCAPE_BACKEND` variable, forces a choice. Without either, the choice is automatic. Pictures and benchmarks that draw into memory use the GPU only when you ask for it with `--backend gpu`, so their output is the same on every machine.
- `BYTESCAPE_GPU_ADAPTER=software` asks for a software adapter. `BYTESCAPE_GPU_FAIL_AT=<step>` (`instance`, `adapter`, `device`, `pipeline`, `surface`, `renderer` or `first-frame`) makes a start-up step fail, to see the fall back.

## Build

```sh
cargo build --release -p bytescape -p bs-gate
```

## Measure on a machine with a graphics card

Run this from a normal desktop session on the machine you want to judge. It opens a 4K window for about a minute per run.

```sh
# Linux, Windows (PowerShell or cmd): the zoom, rendered and presented in a window
target/release/bytescape --bench-zoom --backend gpu --paced --size 3840x2160

# The same, rendered into a texture and waiting for the GPU, with no window
target/release/bytescape --bench-zoom --backend gpu --paced --offscreen --size 3840x2160

# For a real folder instead of the generated scene, add: --path <folder>
```

The synthetic scene is generated from a fixed seed and holds about 20,000 boxes in the view being zoomed. Add `--boxes N` to change that.

To pin the process to four cores, the way a small laptop would have it, start the command with `taskset -c 0-3` (Linux) or `start /affinity F` (Windows). Treat that as a rough stand-in for a laptop, not as the laptop.

The same measurement through the test harness on Linux, which also records first-frame times and the software numbers next to the GPU ones, runs inside a virtual display (Xvfb). Whether a graphics driver can present to a virtual display depends on the driver, so use the commands above when the question is about the real display path:

```sh
BACKEND=wgpu WGPU_ADAPTER=default REAL_PATH='' crates/bs-gate/run.sh results/
```

`REAL_PATH` is a large folder to scan for the real-data series; leave it empty to measure only the generated scene. `WGPU_ADAPTER=software` forces the software adapter. The command writes `results/results.json` and `results/summary.md`.

## What to read

Each run prints `key=value` lines. For the window run, the keys start with `wgpu_windowed.`:

| key | meaning |
|---|---|
| `wgpu_windowed.adapter`, `.adapter_type`, `.adapter_backend` | the graphics card and API in use. `software_adapter=true` means the run says nothing about a graphics card. |
| `wgpu_windowed.zoom.warm.all_frames.p99_ms` | the 99th-percentile frame time while zooming between two views already drawn once |
| `wgpu_windowed.zoom.cold.all_frames.p99_ms` | the same for zooms into folders not seen before; the first frame also carries the layout time |
| `...all_frames.max_ms` | the slowest frame |
| `...all_frames.over_16_7ms`, `.over_33_3ms`, `.n` | how many frames took longer than one and than two 60 Hz frames, out of `n` |
| `...anim_frames`, `...first_frames`, `...final_frames` | the same figures for the animation frames, the first frame of a zoom (the target view is drawn) and the last frame (full quality, with labels) |

A frame time is the CPU time of drawing and presenting the frame. A paced run starts each frame at its real 1/60 s slot, as the window does.

## When the zoom counts as smooth

Judge the paced window run, warm and cold series separately. Both must meet all three limits:

- the 99th-percentile frame time is at most 16.7 ms;
- at most 1% of frames take longer than 16.7 ms;
- no frame takes longer than 33.3 ms.

The harness also prints a stricter reading in which no frame at all may take longer than 16.7 ms. The offscreen run and the unpaced run are diagnostics. Judge on a quiet machine, with the load average noted next to the numbers, and write down the processor, graphics card, driver, display server, scale factor and the commit that was built.

## First frame

`target/release/bytescape --bench-first-frame <folder>` prints `internal.first_frame_presented_ms`. The harness repeats it and reports the spawn-to-first-present time as a median, 95th percentile and maximum, once with the automatic choice and once with `--backend software`. The 95th percentile of the warm series must stay under 100 ms.
