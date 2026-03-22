# nvmon

> *What if the nv in nvtop actually stood for nvidia?*

`nvmon` is a terminal-based monitoring tool for NVIDIA GPUs, using NVML.
It provides real-time plotting of metrics such as GPU utilization, memory usage, temperature and power consumption, as well as a process explorer.
Really, it's just `nvtop` but it's NVIDIA only and has more graphs. I made this because I wanted more graphs, ok?

## Installation

```bash
cargo install nvmon
```

## Usage

All keybinds are printed in the terminal when you run `nvmon`.
Use `nvmon --help` for command line args but i doubt you'll need any of them.
