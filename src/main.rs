use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use nvml_wrapper::{
    enum_wrappers::device::{Clock, PerformancePolicy, TemperatureSensor},
    enums::device::UsedGpuMemory,
    Nvml,
};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    prelude::CrosstermBackend,
    style::{Color, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph},
    Frame, Terminal,
};
use std::{
    collections::HashMap,
    io::stdout,
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthStr;

/// NVML TUI monitor
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Sampling interval in milliseconds
    #[arg(short, long, default_value_t = 1000)]
    delay_ms: u64,

    /// GPU index to monitor (default: 0)
    #[arg(short, long, default_value_t = 0)]
    gpu: u32,

    /// Number of data points to keep in history
    #[arg(long, default_value_t = 300)]
    history: usize,
}

const GRAPH_COLORS: [Color; 6] = [
    Color::Green,
    Color::Cyan,
    Color::Yellow,
    Color::Magenta,
    Color::Red,
    Color::Blue,
];

struct TimeSeries {
    label: &'static str,
    unit: &'static str,
    data: Vec<f64>,
    max_val: f64,
    color: Color,
    capacity: usize,
}

impl TimeSeries {
    fn new(
        label: &'static str,
        unit: &'static str,
        max_val: f64,
        color: Color,
        capacity: usize,
    ) -> Self {
        Self {
            label,
            unit,
            data: Vec::with_capacity(capacity),
            max_val,
            color,
            capacity,
        }
    }

    fn push(&mut self, val: f64) {
        self.data.push(val);
        if self.data.len() > self.capacity {
            self.data.remove(0);
        }
    }

    fn current(&self) -> f64 {
        self.data.last().copied().unwrap_or(0.0)
    }

    fn chart_data(&self) -> Vec<(f64, f64)> {
        let padding = self.capacity.saturating_sub(self.data.len());

        (0..self.capacity)
            .map(|i| {
                let val = if i < padding {
                    0.0
                } else {
                    self.data[i - padding]
                };
                (i as f64, val)
            })
            .collect()
    }
}

struct Counter {
    label: &'static str,
    unit: &'static str,
    value: String,
}

struct GpuProcess {
    pid: u32,
    name: String,
    mem_mb: u64,
    sm_util: u32,
    mem_util: u32,
    enc_util: u32,
    dec_util: u32,
    is_compute: bool,
    is_graphics: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum SortColumn {
    Pid,
    Name,
    Mem,
    SmUtil,
    MemUtil,
    EncUtil,
    DecUtil,
}

impl SortColumn {
    fn next(self) -> Self {
        match self {
            SortColumn::Pid => SortColumn::Name,
            SortColumn::Name => SortColumn::Mem,
            SortColumn::Mem => SortColumn::SmUtil,
            SortColumn::SmUtil => SortColumn::MemUtil,
            SortColumn::MemUtil => SortColumn::EncUtil,
            SortColumn::EncUtil => SortColumn::DecUtil,
            SortColumn::DecUtil => SortColumn::Pid,
        }
    }

    fn prev(self) -> Self {
        match self {
            SortColumn::Pid => SortColumn::DecUtil,
            SortColumn::Name => SortColumn::Pid,
            SortColumn::Mem => SortColumn::Name,
            SortColumn::SmUtil => SortColumn::Mem,
            SortColumn::MemUtil => SortColumn::SmUtil,
            SortColumn::EncUtil => SortColumn::MemUtil,
            SortColumn::DecUtil => SortColumn::EncUtil,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SortColumn::Pid => "PID",
            SortColumn::Name => "Command",
            SortColumn::Mem => "Mem",
            SortColumn::SmUtil => "SM%",
            SortColumn::MemUtil => "Mem%",
            SortColumn::EncUtil => "Enc%",
            SortColumn::DecUtil => "Dec%",
        }
    }
}

enum AppScreen {
    Graphs,
    Top,
}

struct App {
    gpu_name: String,
    gpu_idx: u32,

    gpu_util: TimeSeries,
    mem_util: TimeSeries,
    gpu_temp: TimeSeries,
    power: TimeSeries,
    fan_speed: TimeSeries,
    fb_used: TimeSeries,

    max_power_w: u64,

    counters: Vec<Counter>,

    processes: Vec<GpuProcess>,
    sort_column: SortColumn,
    sort_ascending: bool,
    selected_index: usize,
    viewport_offset: usize,
    filter: String,
    filter_editing: bool,
    kill_confirm: Option<u32>,

    screen: AppScreen,
}

impl App {
    fn new(gpu_name: String, gpu_idx: u32, history: usize) -> Self {
        Self {
            gpu_name,
            gpu_idx,
            gpu_util: TimeSeries::new("GPU Core", "%", 100.0, GRAPH_COLORS[0], history),
            fb_used: TimeSeries::new("VRAM Used", "MB", 1.0, GRAPH_COLORS[1], history),
            gpu_temp: TimeSeries::new("Temperature", "°C", 100.0, GRAPH_COLORS[2], history),
            power: TimeSeries::new("Power", "W", 500.0, GRAPH_COLORS[3], history),
            fan_speed: TimeSeries::new("Fan Speed", "%", 100.0, GRAPH_COLORS[4], history),
            mem_util: TimeSeries::new("Memory Bandwidth", "%", 100.0, GRAPH_COLORS[5], history),
            max_power_w: 0,
            counters: Vec::new(),
            processes: Vec::new(),
            sort_column: SortColumn::Mem,
            sort_ascending: false,
            selected_index: 0,
            viewport_offset: 0,
            filter: String::new(),
            filter_editing: false,
            kill_confirm: None,
            screen: AppScreen::Graphs,
        }
    }

    fn switch_screen(&mut self) {
        self.screen = match self.screen {
            AppScreen::Graphs => AppScreen::Top,
            AppScreen::Top => AppScreen::Graphs,
        };
    }

    fn sample(&mut self, nvml: &Nvml) {
        let device = match nvml.device_by_index(self.gpu_idx) {
            Ok(d) => d,
            Err(_) => return,
        };
        if let Ok(u) = device.utilization_rates() {
            self.gpu_util.push(u.gpu as f64);
            self.mem_util.push(u.memory as f64);
        } else {
            self.gpu_util.push(0.0);
            self.mem_util.push(0.0);
        }

        self.gpu_temp
            .push(device.temperature(TemperatureSensor::Gpu).unwrap_or(0) as f64);

        // i *think* NVML always reports power in milliwatts, unsure though
        let power_w = device.power_usage().unwrap_or(0) as f64 / 1000.0;
        self.power.push(power_w);

        if let Ok(limit) = device.enforced_power_limit() {
            let limit_w = limit as u64 / 1000;
            self.power.max_val = limit_w as f64;
            self.max_power_w = limit_w;
        }

        self.fan_speed.push(device.fan_speed(0).unwrap_or(0) as f64);

        if let Ok(mem) = device.memory_info() {
            let used_mb = mem.used / (1024 * 1024);
            let total_mb = mem.total / (1024 * 1024);
            self.fb_used.max_val = total_mb as f64;
            self.fb_used.push(used_mb as f64);
        } else {
            self.fb_used.push(0.0);
        }

        let mut counters = Vec::new();

        counters.push(Counter {
            label: "Driver",
            unit: "",
            value: nvml
                .sys_driver_version()
                .unwrap_or_else(|_| "Unknown".to_string()),
        });

        if let Ok(cuda) = nvml.sys_cuda_driver_version() {
            counters.push(Counter {
                label: "CUDA",
                unit: "",
                value: format!(
                    "{}.{}",
                    nvml_wrapper::cuda_driver_version_major(cuda),
                    nvml_wrapper::cuda_driver_version_minor(cuda)
                ),
            });
        }

        counters.push(Counter {
            label: "",
            unit: "",
            value: "".into(),
        });

        if let Ok(clk) = device.clock(
            Clock::SM,
            nvml_wrapper::enum_wrappers::device::ClockId::Current,
        ) {
            counters.push(Counter {
                label: "SM Clock",
                unit: "MHz",
                value: clk.to_string(),
            });
        }
        if let Ok(clk) = device.clock(
            Clock::Memory,
            nvml_wrapper::enum_wrappers::device::ClockId::Current,
        ) {
            counters.push(Counter {
                label: "Mem Clock",
                unit: "MHz",
                value: clk.to_string(),
            });
        }

        if let Ok(enc) = device.encoder_utilization() {
            counters.push(Counter {
                label: "Encoder",
                unit: "%",
                value: enc.utilization.to_string(),
            });
        }
        if let Ok(dec) = device.decoder_utilization() {
            counters.push(Counter {
                label: "Decoder",
                unit: "%",
                value: dec.utilization.to_string(),
            });
        }

        if let Ok(bar) = device.bar1_memory_info() {
            counters.push(Counter {
                label: "BAR1 Used",
                unit: "MB",
                value: (bar.used / (1024 * 1024)).to_string(),
            });
        }

        if let Ok(rx) =
            device.pcie_throughput(nvml_wrapper::enum_wrappers::device::PcieUtilCounter::Receive)
        {
            counters.push(Counter {
                label: "PCIe RX",
                unit: "MB/s",
                value: (rx / 1024).to_string(),
            });
        }
        if let Ok(tx) =
            device.pcie_throughput(nvml_wrapper::enum_wrappers::device::PcieUtilCounter::Send)
        {
            counters.push(Counter {
                label: "PCIe TX",
                unit: "MB/s",
                value: (tx / 1024).to_string(),
            });
        }

        if let Ok(replay) = device.pcie_replay_counter() {
            counters.push(Counter {
                label: "PCIe Replays",
                unit: "",
                value: replay.to_string(),
            });
        }

        if let Ok(v) = device.violation_status(PerformancePolicy::Power) {
            let pct = if v.reference_time > 0 {
                v.violation_time * 100 / v.reference_time
            } else {
                0
            };
            counters.push(Counter {
                label: "Pwr limit",
                unit: "%",
                value: pct.to_string(),
            });
        }
        if let Ok(v) = device.violation_status(PerformancePolicy::Thermal) {
            counters.push(Counter {
                label: "Thrml limit",
                unit: "",
                value: if v.violation_time > 0 {
                    "yes".into()
                } else {
                    "no".into()
                },
            });
        }

        self.counters = counters;

        let mut proc_map: HashMap<u32, GpuProcess> = HashMap::new();

        let add_proc = |map: &mut HashMap<u32, GpuProcess>,
                        pid: u32,
                        mem: &UsedGpuMemory,
                        nvml: &Nvml,
                        compute: bool,
                        graphics: bool| {
            let mem_mb = match mem {
                UsedGpuMemory::Used(b) => b / (1024 * 1024),
                UsedGpuMemory::Unavailable => 0,
            };
            let entry = map.entry(pid).or_insert_with(|| {
                let name = nvml
                    .sys_process_name(pid, 256)
                    .unwrap_or_else(|_| "<unknown>".into());
                GpuProcess {
                    pid,
                    name,
                    mem_mb: 0,
                    sm_util: 0,
                    mem_util: 0,
                    enc_util: 0,
                    dec_util: 0,
                    is_compute: false,
                    is_graphics: false,
                }
            });
            entry.mem_mb = entry.mem_mb.max(mem_mb);
            entry.is_compute |= compute;
            entry.is_graphics |= graphics;
        };

        if let Ok(procs) = device.running_compute_processes() {
            for p in &procs {
                add_proc(&mut proc_map, p.pid, &p.used_gpu_memory, nvml, true, false);
            }
        }
        if let Ok(procs) = device.running_graphics_processes() {
            for p in &procs {
                add_proc(&mut proc_map, p.pid, &p.used_gpu_memory, nvml, false, true);
            }
        }

        if let Ok(util_samples) = device.process_utilization_stats(None) {
            for s in &util_samples {
                if let Some(entry) = proc_map.get_mut(&s.pid) {
                    entry.sm_util = s.sm_util;
                    entry.mem_util = s.mem_util;
                    entry.enc_util = s.enc_util;
                    entry.dec_util = s.dec_util;
                }
            }
        }

        self.processes = proc_map.into_values().collect();
    }
}

fn draw(frame: &mut Frame, app: &mut App, device_count: u32, tick_rate: Duration) {
    let outer = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(frame.area());

    let title = Line::from(vec![
        Span::styled(
            " nvmon ",
            Style::default().fg(Color::Black).bg(Color::Green).bold(),
        ),
        if device_count > 1 {
            Span::raw(format!(
                " GPU {}/{}: {} ",
                app.gpu_idx,
                device_count - 1,
                app.gpu_name
            ))
        } else {
            Span::raw(format!(" GPU {}: {} ", app.gpu_idx, app.gpu_name))
        },
        Span::styled(
            format!(
                "[q] quit{}, [tab] switch to {}{}",
                if device_count > 1 {
                    ", [←][→] change gpu"
                } else {
                    ""
                },
                match app.screen {
                    AppScreen::Graphs => "processes",
                    AppScreen::Top => "graphs",
                },
                match app.screen {
                    AppScreen::Graphs =>
                        format!(", [+][−] change sample rate (current: {:?})", tick_rate),
                    AppScreen::Top => {
                        let dir = if app.sort_ascending { "asc" } else { "desc" };
                        let filter_str = if app.filter.is_empty() {
                            String::new()
                        } else {
                            format!(", filter: {}", app.filter)
                        };
                        format!(
                            ", [↑][↓] scroll, [k] kill, [s][S] sort col, [o] order, [f] filter | sort: {} {}{}",
                            app.sort_column.label(), dir, filter_str
                        )
                    }
                }
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(title), outer[0]);

    let body = Layout::horizontal([Constraint::Min(40), Constraint::Length(28)]).split(outer[1]);

    match app.screen {
        AppScreen::Graphs => {
            draw_graphs(frame, app, body[0]);
            draw_counters(frame, app, body[1]);
        }
        AppScreen::Top => {
            draw_top(frame, app, body[0]);
            draw_gpu_info(frame, app, body[1]);
        }
    }
}

fn draw_graphs(frame: &mut Frame, app: &App, area: Rect) {
    let series: Vec<&TimeSeries> = vec![
        &app.gpu_util,
        &app.fb_used,
        &app.gpu_temp,
        &app.power,
        &app.fan_speed,
        &app.mem_util,
    ];

    let constraints: Vec<Constraint> = series
        .iter()
        .map(|_| Constraint::Ratio(1, series.len() as u32))
        .collect();
    let chunks = Layout::vertical(constraints).split(area);

    for (i, ts) in series.iter().enumerate() {
        let current = ts.current();
        let title_str = if std::ptr::eq(*ts, &app.power) {
            format!(
                " {}: {:.0} {} / {} {} ",
                ts.label, current, ts.unit, app.max_power_w, ts.unit
            )
        } else if std::ptr::eq(*ts, &app.fb_used) {
            format!(
                " {}: {:.0} / {:.0} {} ",
                ts.label, current, ts.max_val, ts.unit
            )
        } else {
            format!(" {}: {:.0} {} ", ts.label, current, ts.unit)
        };

        let chart_data = ts.chart_data();
        let len = chart_data.len() as f64;

        let dataset = Dataset::default()
            .marker(Marker::Octant)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(ts.color))
            .data(&chart_data);

        let x_axis = Axis::default().bounds([0.0, if len > 1.0 { len - 1.0 } else { 1.0 }]);

        let max = format!("{:.0}", ts.max_val);
        let w = max.len();
        let y_axis = Axis::default()
            .bounds([0.0, ts.max_val])
            .labels::<Vec<String>>(vec![
                format!("{:>w$}", 0).into(),
                format!("{:>w$.0}", ts.max_val / 2.0).into(),
                max.into(),
            ]);

        let chart = Chart::new(vec![dataset])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(title_str)
                    .title_style(Style::default().fg(ts.color).bold()),
            )
            .x_axis(x_axis)
            .y_axis(y_axis);

        frame.render_widget(chart, chunks[i]);
    }
}

fn draw_counters(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    for c in &app.counters {
        let val_with_unit = if c.unit.is_empty() {
            c.value.clone()
        } else {
            format!("{} {}", c.value, c.unit)
        };

        let left = c.label;
        let right = val_with_unit;

        let spacing = (26usize).saturating_sub(left.width() + right.width());

        lines.push(Line::from(vec![
            Span::styled(left.to_string(), Style::default().fg(Color::DarkGray)),
            Span::raw(" ".repeat(spacing)),
            Span::styled(right.to_string(), Style::default().fg(Color::White).bold()),
        ]));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" Counters ")
        .title_style(Style::default().fg(Color::White).bold());

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

fn draw_top(frame: &mut Frame, app: &mut App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    let columns = [
        SortColumn::Pid,
        SortColumn::Name,
        SortColumn::Mem,
        SortColumn::SmUtil,
        SortColumn::MemUtil,
        SortColumn::EncUtil,
        SortColumn::DecUtil,
    ];

    let mut header_spans: Vec<Span> = columns
        .iter()
        .map(|col| {
            let arrow = if *col == app.sort_column {
                if app.sort_ascending {
                    " ▲"
                } else {
                    " ▼"
                }
            } else {
                ""
            };

            let combined_label = format!("{} {}", arrow, col.label());

            let text = match col {
                SortColumn::Name => format!("{:<32}", combined_label),
                _ => format!("{:>8}", combined_label),
            };

            let style = if *col == app.sort_column {
                Style::default().fg(Color::Green).bold()
            } else {
                Style::default().fg(Color::DarkGray).bold()
            };

            Span::styled(text, style)
        })
        .collect();

    // interleave " │ " between columns
    for i in (1..header_spans.len()).rev() {
        header_spans.insert(i, Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
    }

    header_spans.push(Span::styled(
        " | Type",
        Style::default().fg(Color::DarkGray).bold(),
    ));

    lines.push(Line::from(header_spans));
    lines.push(Line::from(Span::styled(
        "─".repeat(area.width.saturating_sub(2) as usize),
        Style::default().fg(Color::DarkGray),
    )));

    let mut display_procs: Vec<&GpuProcess> = app.processes.iter().collect();

    let filter_lower = app.filter.to_lowercase();
    if !filter_lower.is_empty() {
        display_procs.retain(|p| p.name.to_lowercase().contains(&filter_lower));
    }

    let asc = app.sort_ascending;
    display_procs.sort_by(|a, b| {
        let ord = match app.sort_column {
            SortColumn::Pid => a.pid.cmp(&b.pid),
            SortColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortColumn::Mem => a.mem_mb.cmp(&b.mem_mb),
            SortColumn::SmUtil => a.sm_util.cmp(&b.sm_util),
            SortColumn::MemUtil => a.mem_util.cmp(&b.mem_util),
            SortColumn::EncUtil => a.enc_util.cmp(&b.enc_util),
            SortColumn::DecUtil => a.dec_util.cmp(&b.dec_util),
        };
        if asc {
            ord
        } else {
            ord.reverse()
        }
    });

    let procs_len = display_procs.len();

    let visible_height = area.height.saturating_sub(4) as usize; // borders + header + separator

    // Clamp selected_index to valid range
    let selected = if procs_len == 0 {
        0
    } else {
        app.selected_index.min(procs_len - 1)
    };

    // Adjust viewport so the selected item is always visible
    let mut viewport = app.viewport_offset;
    if selected < viewport {
        viewport = selected;
    } else if selected >= viewport + visible_height {
        viewport = selected + 1 - visible_height;
    }
    app.viewport_offset = viewport;
    app.selected_index = selected;

    for (i, proc) in display_procs
        .iter()
        .enumerate()
        .skip(viewport)
        .take(visible_height)
    {
        let is_selected = i == selected;
        let is_kill_target = app.kill_confirm == Some(proc.pid);

        let base_str = format!(
            "{:>8} │ {:<32} │ {:>8} │ {:>8} │ {:>8} │ {:>8} │ {:>8} │ ",
            proc.pid,
            if proc.name.len() > 32 {
                &proc.name[..32]
            } else {
                &proc.name
            },
            format!("{} MB", proc.mem_mb),
            format!("{}%", proc.sm_util),
            format!("{}%", proc.mem_util),
            format!("{}%", proc.enc_util),
            format!("{}%", proc.dec_util),
        );

        let base_style = if is_kill_target {
            Style::default().fg(Color::White).bg(Color::Red).bold()
        } else if is_selected {
            Style::default().fg(Color::Black).bg(Color::White).bold()
        } else {
            Style::default().fg(Color::Gray)
        };

        let mut spans = vec![Span::styled(base_str, base_style)];

        let graphics_style = if is_kill_target {
            Style::default().fg(Color::White).bg(Color::Red).bold()
        } else if is_selected {
            Style::default().fg(Color::Green).bg(Color::White).bold()
        } else {
            Style::default().fg(Color::Green)
        };

        let compute_style = if is_kill_target {
            Style::default().fg(Color::White).bg(Color::Red).bold()
        } else if is_selected {
            Style::default().fg(Color::Magenta).bg(Color::White).bold()
        } else {
            Style::default().fg(Color::Magenta)
        };

        match (proc.is_graphics, proc.is_compute) {
            (true, true) => {
                spans.push(Span::styled("GRAPHICS", graphics_style));
                spans.push(Span::styled(" | ", base_style));
                spans.push(Span::styled("COMPUTE ", compute_style));
            }
            (true, false) => {
                spans.push(Span::styled("GRAPHICS ", graphics_style));
            }
            (false, true) => {
                spans.push(Span::styled("COMPUTE ", compute_style));
            }
            (false, false) => {
                spans.push(Span::styled("?", base_style));
            }
        }

        lines.push(Line::from(spans));
    }

    if procs_len == 0 {
        lines.push(Line::from(Span::styled(
            "  No GPU processes found",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let mut title = format!(" Processes ({}) ", procs_len);
    if let Some(pid) = app.kill_confirm {
        title = format!(" Kill PID {}? [y]es / [n]o ", pid);
    }
    if app.filter_editing {
        title = format!(" Filter: {}▏", app.filter);
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if app.kill_confirm.is_some() {
            Color::Red
        } else if app.filter_editing {
            Color::Yellow
        } else {
            Color::DarkGray
        }))
        .title(title)
        .title_style(
            Style::default()
                .fg(if app.kill_confirm.is_some() {
                    Color::Red
                } else if app.filter_editing {
                    Color::Yellow
                } else {
                    Color::White
                })
                .bold(),
        );

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

fn draw_gpu_info(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    let series: Vec<&TimeSeries> = vec![
        &app.gpu_util,
        &app.fb_used,
        &app.gpu_temp,
        &app.power,
        &app.fan_speed,
        &app.mem_util,
    ];

    for ts in &series {
        let val_str = if std::ptr::eq(*ts, &app.power) {
            format!("{:.0}/{} {}", ts.current(), app.max_power_w, ts.unit)
        } else if std::ptr::eq(*ts, &app.fb_used) {
            format!("{:.0}/{:.0} {}", ts.current(), ts.max_val, ts.unit)
        } else {
            format!("{:.0} {}", ts.current(), ts.unit)
        };
        let left = ts.label;
        let right = val_str.as_str();

        let spacing = (26usize).saturating_sub(left.width() + right.width());

        lines.push(Line::from(vec![
            Span::styled(left.to_string(), Style::default().fg(ts.color)),
            Span::raw(" ".repeat(spacing)),
            Span::styled(right.to_string(), Style::default().fg(Color::White).bold()),
        ]));
    }

    lines.push(Line::from(""));

    for c in &app.counters {
        let val_with_unit = if c.unit.is_empty() {
            c.value.clone()
        } else {
            format!("{} {}", c.value, c.unit)
        };

        let left = c.label;
        let right = val_with_unit;

        let spacing = (26usize).saturating_sub(left.width() + right.width());

        lines.push(Line::from(vec![
            Span::styled(left.to_string(), Style::default().fg(Color::DarkGray)),
            Span::raw(" ".repeat(spacing)),
            Span::styled(right.to_string(), Style::default().fg(Color::White).bold()),
        ]));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" GPU Info ")
        .title_style(Style::default().fg(Color::White).bold());

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let nvml = Nvml::init().map_err(|e| {
        eprintln!("Failed to initialize NVML: {}", e);
        eprintln!("Make sure NVIDIA drivers are installed.");
        e
    })?;

    let device_count = nvml.device_count()?;
    if args.gpu >= device_count {
        eprintln!(
            "GPU index {} out of range (found {} GPU(s))",
            args.gpu, device_count
        );
        return Ok(());
    }
    let mut current_gpu: usize = args.gpu as usize;

    let mut apps = Vec::new();
    for idx in 0..device_count {
        let gpu_name = nvml
            .device_by_index(idx)?
            .name()
            .unwrap_or_else(|_| "Unknown".to_string());
        apps.push(App::new(gpu_name, idx, args.history));
    }

    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut tick_rate = Duration::from_millis(args.delay_ms);
    let mut last_tick = Instant::now();

    const TICK_DELTA: Duration = Duration::from_millis(50);

    apps.iter_mut().for_each(|app| app.sample(&nvml));

    loop {
        let app: &mut App = apps.get_mut(current_gpu).unwrap();

        terminal.draw(|frame| draw(frame, app, device_count, tick_rate))?;

        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Tab => app.switch_screen(),
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break
                        }

                        KeyCode::Char('+') | KeyCode::Char('=') => {
                            tick_rate = if tick_rate == Duration::from_millis(1) {
                                TICK_DELTA
                            } else {
                                tick_rate
                                    .saturating_add(TICK_DELTA)
                                    .min(Duration::from_millis(2500))
                            };
                        }
                        KeyCode::Char('-') => {
                            tick_rate = tick_rate
                                .saturating_sub(TICK_DELTA)
                                .max(Duration::from_millis(1));
                        }
                        KeyCode::Left => {
                            current_gpu =
                                (current_gpu + device_count as usize - 1) % device_count as usize;
                        }
                        KeyCode::Right => {
                            current_gpu = (current_gpu + 1) % device_count as usize;
                        }

                        code if matches!(app.screen, AppScreen::Top) => {
                            if let Some(pid) = app.kill_confirm.take() {
                                if code == KeyCode::Char('y') {
                                    #[cfg(unix)]
                                    unsafe {
                                        libc::kill(pid as i32, libc::SIGTERM);
                                    }

                                    #[cfg(windows)]
                                    {
                                        use windows_sys::Win32::Foundation::FALSE;
                                        use windows_sys::Win32::System::Threading::{
                                            OpenProcess, TerminateProcess, PROCESS_TERMINATE,
                                        };

                                        let handle =
                                            OpenProcess(PROCESS_TERMINATE, FALSE, pid as u32);
                                        if handle != 0 {
                                            TerminateProcess(handle, 1);
                                        }
                                    }
                                }
                            } else if app.filter_editing {
                                match code {
                                    KeyCode::Enter | KeyCode::Esc => app.filter_editing = false,
                                    KeyCode::Backspace => {
                                        app.filter.pop();
                                    }
                                    KeyCode::Char(c) => app.filter.push(c),
                                    _ => {}
                                }
                            } else {
                                match code {
                                    KeyCode::Up => {
                                        app.selected_index = app.selected_index.saturating_sub(1);
                                    }
                                    KeyCode::Down => {
                                        app.selected_index += 1;
                                    }
                                    KeyCode::Char('s') => {
                                        app.sort_column =
                                            if key.modifiers.contains(KeyModifiers::SHIFT) {
                                                app.sort_column.prev()
                                            } else {
                                                app.sort_column.next()
                                            };
                                    }
                                    KeyCode::Char('S') => app.sort_column = app.sort_column.prev(),
                                    KeyCode::Char('o') => app.sort_ascending = !app.sort_ascending,
                                    KeyCode::Char('f') => app.filter_editing = true,
                                    KeyCode::Char('k') => {
                                        // Apply sort and filter to get the correct PID for the selected process
                                        let mut display_procs: Vec<&GpuProcess> =
                                            app.processes.iter().collect();

                                        let filter_lower = app.filter.to_lowercase();
                                        if !filter_lower.is_empty() {
                                            display_procs.retain(|p| {
                                                p.name.to_lowercase().contains(&filter_lower)
                                            });
                                        }

                                        let asc = app.sort_ascending;
                                        display_procs.sort_by(|a, b| {
                                            let ord = match app.sort_column {
                                                SortColumn::Pid => a.pid.cmp(&b.pid),
                                                SortColumn::Name => a
                                                    .name
                                                    .to_lowercase()
                                                    .cmp(&b.name.to_lowercase()),
                                                SortColumn::Mem => a.mem_mb.cmp(&b.mem_mb),
                                                SortColumn::SmUtil => a.sm_util.cmp(&b.sm_util),
                                                SortColumn::MemUtil => a.mem_util.cmp(&b.mem_util),
                                                SortColumn::EncUtil => a.enc_util.cmp(&b.enc_util),
                                                SortColumn::DecUtil => a.dec_util.cmp(&b.dec_util),
                                            };
                                            if asc {
                                                ord
                                            } else {
                                                ord.reverse()
                                            }
                                        });

                                        if let Some(proc) = display_procs.get(app.selected_index) {
                                            app.kill_confirm = Some(proc.pid);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            apps.iter_mut().for_each(|app| app.sample(&nvml));
            last_tick = Instant::now();
        }
    }

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    Ok(())
}
