use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use nvml_wrapper::{
    enum_wrappers::device::{Clock, PerformancePolicy, TemperatureSensor},
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
    io::stdout,
    time::{Duration, Instant},
};

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
    }
}

fn draw(frame: &mut Frame, app: &App) {
    let outer = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(frame.area());

    let title = Line::from(vec![
        Span::styled(
            " nvmon ",
            Style::default().fg(Color::Black).bg(Color::Green).bold(),
        ),
        Span::raw(format!("  GPU {}: {} ", app.gpu_idx, app.gpu_name)),
        Span::styled(
            format!(
                "[q] quit, [←][→] change gpu, [tab] switch to {}",
                match app.screen {
                    AppScreen::Graphs => "processes",
                    AppScreen::Top => "graphs",
                }
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(title), outer[0]);

    let body = Layout::horizontal([Constraint::Min(40), Constraint::Length(24)]).split(outer[1]);

    draw_graphs(frame, app, body[0]);
    draw_counters(frame, app, body[1]);
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

        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<12}", c.label),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("{:>10}", val_with_unit),
                Style::default().fg(Color::White).bold(),
            ),
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

    let gpu_name = nvml
        .device_by_index(args.gpu)?
        .name()
        .unwrap_or_else(|_| "Unknown".to_string());

    let mut app = App::new(gpu_name, args.gpu, args.history);

    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let tick_rate = Duration::from_millis(args.delay_ms);
    let mut last_tick = Instant::now();

    app.sample(&nvml);

    loop {
        terminal.draw(|frame| draw(frame, &app))?;

        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Tab => app.switch_screen(),
                        KeyCode::Char('q') => break,
                        KeyCode::Esc => break,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break
                        }
                        _ => {}
                    }
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            app.sample(&nvml);
            last_tick = Instant::now();
        }
    }

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    Ok(())
}
