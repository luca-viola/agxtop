//! nvtop-style screen. Drawing is done cell-by-cell into a ratatui Buffer so
//! the layout matches the original curses version exactly.

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use std::collections::{HashMap, HashSet, VecDeque};

use crate::sys::{self, GpuInfo, KmsgWatch, Proc, Scanner, Sensor};
use crate::tracer::{Client, Tracer};
use crate::util::{clock_hms, fmt_g, human, short, thousands};

const HIST: usize = 400;
const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Gpu,
    Mem,
    Pid,
    Name,
}

fn push(h: &mut VecDeque<f64>, v: f64) {
    if h.len() == HIST {
        h.pop_front();
    }
    h.push_back(v);
}

fn hmax(h: &VecDeque<f64>) -> f64 {
    h.iter().copied().fold(0.0, f64::max)
}

fn joined_len(items: &[(String, Style)], sep: &str) -> usize {
    let n: usize = items.iter().map(|i| i.0.chars().count()).sum();
    n + sep.chars().count() * items.len().saturating_sub(1)
}

/// Split items into rows no wider than `width`, keeping each item whole.
fn pack(items: &[(String, Style)], sep: &str, width: usize) -> Vec<Vec<(String, Style)>> {
    let mut rows: Vec<Vec<(String, Style)>> = Vec::new();
    let mut cur: Vec<(String, Style)> = Vec::new();
    for it in items {
        cur.push(it.clone());
        if cur.len() > 1 && joined_len(&cur, sep) > width {
            let last = cur.pop().unwrap();
            rows.push(std::mem::replace(&mut cur, vec![last]));
        }
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    rows
}

fn put_row(p: &mut Painter, y: i32, mut x: i32, items: &[(String, Style)], sep: &str) {
    for (i, (t, st)) in items.iter().enumerate() {
        if i > 0 {
            p.put(y, x, sep, Style::default());
            x += sep.chars().count() as i32;
        }
        p.put(y, x, t, *st);
        x += t.chars().count() as i32;
    }
}

fn rnd(v: f64) -> i64 {
    v.round() as i64
}

/// Drawing primitives over a ratatui buffer.
struct Painter<'a> {
    buf: &'a mut Buffer,
    w: i32,
    h: i32,
    ramp: &'a [Color],
}

impl Painter<'_> {
    fn put(&mut self, y: i32, x: i32, txt: &str, st: Style) {
        if y < 0 || y >= self.h || x < 0 || x >= self.w {
            return;
        }
        self.buf.set_stringn(x as u16, y as u16, txt, (self.w - x) as usize, st);
    }

    fn cp(n: u8) -> Style {
        let s = Style::default();
        match n {
            1 => s.fg(Color::Green),
            2 => s.fg(Color::Yellow),
            3 => s.fg(Color::Red),
            4 => s.fg(Color::Cyan),
            5 => s.fg(Color::Black).bg(Color::Cyan),
            6 => s.fg(Color::Magenta),
            7 => s.fg(Color::Blue),
            8 => s.fg(Color::Magenta).bg(Color::Blue), // GPU sliver on top of apps
            _ => s,
        }
    }

    fn cpb(n: u8) -> Style {
        Self::cp(n).add_modifier(Modifier::BOLD)
    }

    /// Colour for a position 0..1 along the green->red gradient.
    fn grad(&self, frac: f64) -> Style {
        let n = self.ramp.len();
        let i = ((frac * n as f64) as i64).clamp(0, n as i64 - 1) as usize;
        Style::default().fg(self.ramp[i])
    }

    fn bar(&mut self, y: i32, x: i32, w: i32, pct: f64, label: &str, color: u8, gradient: bool) {
        let inner = (w - 2).max(1);
        let fill = rnd(inner as f64 * pct / 100.0).clamp(0, inner as i64) as i32;
        self.put(y, x, "[", Self::cp(4));
        if gradient {
            for i in 0..fill {
                let st = self.grad((i as f64 + 0.5) / inner as f64).add_modifier(Modifier::BOLD);
                self.put(y, x + 1 + i, "|", st);
            }
        } else {
            self.put(y, x + 1, &"|".repeat(fill as usize), Self::cpb(color));
        }
        self.put(y, x + 1 + fill, &" ".repeat((inner - fill) as usize), Style::default());
        let txt: String = label.chars().take(inner as usize).collect();
        let tl = txt.chars().count() as i32;
        self.put(y, x + 1 + inner - tl, &txt, Style::default().add_modifier(Modifier::BOLD));
        self.put(y, x + 1 + inner, "]", Self::cp(4));
    }

    /// segs: [(bytes, colour pair)] drawn left to right on a 0..total scale.
    fn stacked_bar(&mut self, y: i32, x: i32, w: i32, total: f64, segs: &[(f64, u8)], label: &str) {
        let inner = (w - 2).max(1);
        self.put(y, x, "[", Self::cp(4));
        let (mut pos, mut acc) = (0i32, 0.0);
        for &(val, col) in segs {
            acc += val;
            let mut end = if total > 0.0 { (rnd(inner as f64 * acc / total) as i32).min(inner) } else { 0 };
            if val > 0.0 && end <= pos {
                end = (pos + 1).min(inner);
            }
            if end > pos {
                self.put(y, x + 1 + pos, &"|".repeat((end - pos) as usize), Self::cpb(col));
                pos = end;
            }
        }
        self.put(y, x + 1 + pos, &" ".repeat((inner - pos).max(0) as usize), Style::default());
        let txt: String = label.chars().take(inner as usize).collect();
        let tl = txt.chars().count() as i32;
        self.put(y, x + 1 + inner - tl, &txt, Style::default().add_modifier(Modifier::BOLD));
        self.put(y, x + 1 + inner, "]", Self::cp(4));
    }

    fn boxed(&mut self, y: i32, x: i32, h: i32, w: i32, st: Style) {
        let hl = "─".repeat((w - 2).max(0) as usize);
        self.put(y, x, &format!("┌{hl}┐"), st);
        for r in 1..h - 1 {
            self.put(y + r, x, "│", st);
            self.put(y + r, x + w - 1, "│", st);
        }
        self.put(y + h - 1, x, &format!("└{hl}┘"), st);
    }

    #[allow(clippy::too_many_arguments)]
    fn graph(&mut self, y: i32, x: i32, h: i32, w: i32, values: &VecDeque<f64>, vmax: f64, title: &str,
             color: u8, fmt: &dyn Fn(f64) -> String) {
        if h < 3 || w < 12 {
            return;
        }
        self.boxed(y, x, h, w, Self::cp(4));
        let cur = values.back().copied().unwrap_or(0.0);
        let vm = if vmax > 0.0 { vmax } else { 1.0 };
        let tattr = if color == 1 {
            self.grad(cur / vm).add_modifier(Modifier::BOLD)
        } else {
            Self::cpb(color)
        };
        self.put(y, x + 2, &format!(" {title} {} ", fmt(cur)), tattr);
        let mx = fmt(vmax);
        self.put(y, x + w - mx.chars().count() as i32 - 4, &format!(" {mx} "), Self::cp(4));
        let (gh, gw) = (h - 2, (w - 2) as usize);
        let skip = values.len().saturating_sub(gw);
        let off = gw - (values.len() - skip);
        for (i, v) in values.iter().skip(skip).enumerate() {
            let eighths = rnd((v / vm).clamp(0.0, 1.0) * gh as f64 * 8.0);
            let col = x + 1 + (off + i) as i32;
            for r in 0..gh {
                let fill = (eighths - r as i64 * 8).clamp(0, 8) as usize;
                if fill > 0 {
                    let st = if color == 1 { self.grad((r as f64 + 0.5) / gh as f64) } else { Self::cp(color) };
                    self.put(y + h - 2 - r, col, &BLOCKS[fill].to_string(), st);
                }
            }
        }
    }
}

pub struct Ui {
    tracer: Tracer,
    scanner: Scanner,
    drivers: HashSet<String>,
    pub interval: f64,
    sort: Sort,
    reverse: bool,
    gpu_hist: VecDeque<f64>,
    mem_hist: VecDeque<f64>,
    jobs_hist: VecDeque<f64>,
    used_hist: VecDeque<f64>,
    mem_est: bool,
    mem_zoom: bool,
    show_mem_graph: bool,
    sel: usize,
    sel_pid: Option<i32>,
    top: usize,
    page: usize,
    rows: Vec<i32>,
    help: bool,
    hw: Option<String>,
    ginfo: GpuInfo,
    soc: Option<String>,
    kmsg: KmsgWatch,
    procs: HashMap<i32, Proc>,
    busy_pct: f64,
    jobs_s: f64,
    rings: HashMap<String, u64>,
    sensors: Vec<Sensor>,
    mem_total_clients: Option<f64>,
    ram_total: f64,
    ram_avail: f64,
    ramp: Vec<Color>,
}

pub enum Action {
    Quit,
    Redraw,
}

impl Ui {
    pub fn new(tracer: Tracer, drivers: HashSet<String>, color256: bool) -> Self {
        let ginfo = sys::gpu_info();
        let soc = sys::soc_name(ginfo.chip_id);
        let (ram_total, ram_avail) = sys::meminfo();
        // green -> yellow -> orange -> red gradient
        let ramp = if color256 {
            [34, 40, 76, 112, 148, 184, 220, 214, 208, 202, 196].iter().map(|c| Color::Indexed(*c)).collect()
        } else {
            vec![Color::Green, Color::Green, Color::Yellow, Color::Yellow, Color::Red]
        };
        Ui {
            tracer,
            scanner: Scanner::new(),
            drivers,
            interval: 1.0,
            sort: Sort::Gpu,
            reverse: true,
            gpu_hist: VecDeque::new(),
            mem_hist: VecDeque::new(),
            jobs_hist: VecDeque::new(),
            used_hist: VecDeque::new(),
            mem_est: false,
            mem_zoom: false,
            show_mem_graph: false,
            sel: 0,
            sel_pid: None,
            top: 0,
            page: 1,
            rows: Vec::new(),
            help: false,
            hw: sys::find_hwmon(),
            ginfo,
            soc,
            kmsg: KmsgWatch::new(),
            procs: HashMap::new(),
            busy_pct: 0.0,
            jobs_s: 0.0,
            rings: HashMap::new(),
            sensors: Vec::new(),
            mem_total_clients: None,
            ram_total,
            ram_avail,
            ramp,
        }
    }

    pub fn poll_trace(&mut self, timeout: f64) {
        self.tracer.poll(timeout);
    }

    pub fn stop_trace(&mut self) {
        self.tracer.stop();
    }

    // ------------------------------------------------------------- data --
    pub fn refresh_data(&mut self, span: f64) {
        let s = self.tracer.sample();
        self.busy_pct = if span > 0.0 { (100.0 * s.busy / span).min(100.0) } else { 0.0 };
        self.jobs_s = if span > 0.0 { s.jobs as f64 / span } else { 0.0 };
        self.rings = s.rings;
        let cpct: HashMap<Client, f64> = s
            .cbusy
            .iter()
            .map(|(c, b)| (*c, if span > 0.0 { (100.0 * b / span).min(100.0) } else { 0.0 }))
            .collect();
        let busy_pids: HashSet<i32> =
            cpct.keys().filter_map(|c| if let Client::Pid(p) = c { Some(*p) } else { None }).collect();
        self.procs = self.scanner.scan(&self.drivers, &busy_pids);
        let attributed = self.tracer.attributed || cpct.keys().any(|c| matches!(c, Client::Cid(_)));
        for (pid, p) in self.procs.iter_mut() {
            p.gpu = attributed.then(|| {
                let v = cpct.get(&Client::Pid(*pid)).copied().unwrap_or(0.0)
                    + p.clients.iter().map(|c| cpct.get(&Client::Cid(*c)).copied().unwrap_or(0.0)).sum::<f64>();
                v.min(100.0)
            });
        }
        let mems: Vec<f64> = self.procs.values().filter_map(|p| p.mem).collect();
        self.mem_total_clients = (!mems.is_empty()).then(|| mems.iter().sum());
        self.mem_est = self.procs.values().any(|p| p.mem_est);
        push(&mut self.jobs_hist, self.jobs_s);
        self.sensors = sys::read_sensors(self.hw.as_deref());
        self.kmsg.poll();
        (self.ram_total, self.ram_avail) = sys::meminfo();
        push(&mut self.gpu_hist, self.busy_pct);
        push(&mut self.mem_hist, self.mem_total_clients.unwrap_or(0.0));
        push(&mut self.used_hist, self.ram_total - self.ram_avail);
    }

    // ---------------------------------------------------------- drawing --
    fn mem_graph(&self, p: &mut Painter, y: i32, x: i32, h: i32, w: i32) {
        if h < 3 || w < 20 {
            return;
        }
        let mut total = if self.ram_total > 0.0 { self.ram_total } else { 1.0 };
        if self.mem_zoom && !self.used_hist.is_empty() {
            let z = hmax(&self.used_hist) * 1.15;
            if z > 0.0 {
                total = z;
            }
        }
        let used_now = self.used_hist.back().copied().unwrap_or(0.0);
        let gpu_now = self.mem_total_clients.unwrap_or(0.0).min(used_now);
        let est = if self.mem_est { "~" } else { "" };
        let right = format!("{}{}", if self.mem_zoom { "zoom " } else { "" }, short(Some(total)));
        p.boxed(y, x, h, w, Painter::cp(4));
        p.put(y, x + 2, "  ", Style::default());
        p.put(y, x + w - right.chars().count() as i32 - 4, &format!(" {right} "), Painter::cp(4));
        let apps = (used_now - gpu_now).max(0.0);
        let mut g_txt = format!("GPU {est}{} ", human(Some(gpu_now)));
        let mut a_txt = format!("+ apps {} ", human(Some(apps)));
        if (g_txt.chars().count() + a_txt.chars().count() + right.chars().count() + 8) as i32 > w {
            g_txt = format!("GPU {est}{} ", short(Some(gpu_now)));
            a_txt = format!("+ {} ", short(Some(apps)));
        }
        let tx = x + 3;
        p.put(y, tx, &g_txt, Painter::cpb(6));
        p.put(y, tx + g_txt.chars().count() as i32, &a_txt, Painter::cpb(7));
        let (gh, gw) = (h - 2, (w - 2) as usize);
        let n = self.used_hist.len().min(self.mem_hist.len());
        let skip = n.saturating_sub(gw);
        let off = gw - (n - skip);
        let gpus = self.mem_hist.iter().skip(self.mem_hist.len() - n + skip);
        let useds = self.used_hist.iter().skip(self.used_hist.len() - n + skip);
        for (i, (g, u)) in gpus.zip(useds).enumerate() {
            let g = g.min(*u);
            let mut te = rnd((u / total).min(1.0) * gh as f64 * 8.0);
            let mut ge = rnd((g / total).min(1.0) * gh as f64 * 8.0);
            if g > 0.0 && ge == 0 {
                ge = 1; // keep a non-zero GPU share visible
            }
            te = te.max(ge);
            let col = x + 1 + (off + i) as i32;
            for r in 0..gh {
                let lo = r as i64 * 8;
                if te <= lo {
                    break;
                }
                let (ch, c) = if ge >= lo + 8 {
                    (BLOCKS[8], 6) // cell fully GPU
                } else if ge > lo && te >= lo + 8 {
                    (BLOCKS[(ge - lo) as usize], 8) // GPU sliver under apps
                } else if ge > lo {
                    (BLOCKS[(te - lo).min(8) as usize], 6) // both end inside this cell
                } else {
                    (BLOCKS[(te - lo).min(8) as usize], 7) // apps only
                };
                p.put(y + h - 2 - r, col, &ch.to_string(), Painter::cp(c));
            }
        }
    }

    pub fn draw(&mut self, buf: &mut Buffer) {
        let (w, h) = (buf.area.width as i32, buf.area.height as i32);
        let ramp = self.ramp.clone();
        let mut p = Painter { buf, w, h, ramp: &ramp };
        if h < 12 || w < 50 {
            p.put(0, 0, "Terminal too small for agxtop", Style::default());
            return;
        }
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let mut y = 0;
        p.put(y, 0, " agxtop ", Painter::cpb(5));
        let gi = &self.ginfo;
        let mut parts: Vec<String> = Vec::new();
        if let Some(s) = &self.soc {
            parts.push(format!("Apple {s}"));
        }
        if let Some(t) = &gi.gtype {
            parts.push(t.clone());
        }
        if let Some(c) = gi.cores {
            let tot = match gi.total_cores {
                Some(t) if t != c => format!("/{t}"),
                _ => String::new(),
            };
            let cl = gi.clusters.map(|n| format!(" / {n} clusters")).unwrap_or_default();
            parts.push(format!("{c}{tot} GPU cores{cl}"));
        }
        if let Some(m) = gi.max_mhz {
            parts.push(format!("{m} MHz"));
        }
        let head = if parts.is_empty() {
            let mut d: Vec<&String> = self.drivers.iter().collect();
            d.sort();
            let d = d.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(",");
            format!("driver: {}", if d.is_empty() { "all" } else { &d })
        } else {
            parts.join(" · ")
        };
        let tot = gi.timeouts + self.kmsg.new;
        let now_s = if self.kmsg.new > 0 { format!(" (+{} now!)", self.kmsg.new) } else { String::new() };
        let tst = if self.kmsg.new > 0 { Painter::cpb(3) } else { Painter::cp(if tot > 0 { 2 } else { 1 }) };
        let plain = Style::default();
        let head_items: Vec<(String, Style)> =
            if parts.is_empty() { vec![(head, bold)] } else { parts.into_iter().map(|s| (s, bold)).collect() };
        let stat_items = vec![
            (format!("jobs/s: {}", thousands(self.jobs_s)), plain),
            (format!("{}s", fmt_g(self.interval)), plain),
            (clock_hms(), plain),
            (format!("GPU timeouts: {tot} this boot{now_s}"), tst),
        ];
        // one line when it fits, else chip info and stats on their own lines,
        // both starting in the same column
        let hx = 16;
        let avail = (w - 1 - hx).max(1) as usize;
        let hlen = joined_len(&head_items, " · ");
        if hlen + 4 + joined_len(&stat_items, "   ") <= avail {
            put_row(&mut p, y, hx, &head_items, " · ");
            put_row(&mut p, y, hx + hlen as i32 + 4, &stat_items, "   ");
            y += 1;
        } else {
            for row in pack(&head_items, " · ", avail) {
                put_row(&mut p, y, hx, &row, " · ");
                y += 1;
            }
            for row in pack(&stat_items, "   ", avail) {
                put_row(&mut p, y, hx, &row, "   ");
                y += 1;
            }
        }

        let half = (w - 3) / 2;
        p.put(y, 1, "GPU", Painter::cpb(4));
        p.bar(y, 5, half - 5, self.busy_pct, &format!("{:.0}%", self.busy_pct), 1, true);
        let used = self.ram_total - self.ram_avail;
        let gpu = self.mem_total_clients.unwrap_or(0.0).min(used);
        let est = if self.mem_est { "~" } else { "" };
        p.put(y, half + 2, "MEM", Painter::cpb(4));
        p.stacked_bar(y, half + 6, w - half - 7, self.ram_total, &[(gpu, 6), (used - gpu, 7)], "");
        y += 1;
        // memory figures on their own line, left-aligned under the MEM bar
        let right = (w - 1 - (half + 2)).max(1); // width of the right half
        let plain = Style::default();
        let lab = |f: &dyn Fn(Option<f64>) -> String, apps: &str| -> Vec<(String, Style)> {
            vec![
                ("GPU ".into(), plain),
                (format!("{est}{}", f(Some(gpu))), Painter::cpb(6)),
                (format!(" + {apps}"), plain),
                (f(Some(used - gpu)), Painter::cpb(7)),
                (" / ".into(), plain),
                (f(Some(self.ram_total)), plain),
            ]
        };
        let mlab = [lab(&human, "apps "), lab(&short, "apps "), lab(&short, "")]
            .into_iter()
            .find(|l| joined_len(l, "") <= right as usize)
            .unwrap_or_else(|| lab(&short, ""));
        put_row(&mut p, y, half + 2, &mlab, "");
        if self.mem_total_clients.is_none() {
            p.put(y, 1, "GPU memory: n/a (driver doesn't report)", plain);
        }
        y += 1;
        if !self.sensors.is_empty() {
            // left-aligned under MEM, one line; wraps whole items only if needed
            let items: Vec<(String, Style)> = self
                .sensors
                .iter()
                .map(|s| {
                    let v = if s.unit == "W" { format!("{:.1}{}", s.value, s.unit) } else { format!("{:.0}{}", s.value, s.unit) };
                    (format!("{} {v}", s.label), Painter::cp(2))
                })
                .collect();
            // fewest rows that fit, with the items spread evenly over them
            let n = items.len();
            let per = |width: i32| {
                (1..=n)
                    .map(|k| n.div_ceil(k))
                    .find(|&per| items.chunks(per).all(|c| joined_len(c, "  ") <= width as usize))
                    .unwrap_or(1)
            };
            // always from the left edge, so it doesn't move on resize
            for row in items.chunks(per(w - 2)) {
                put_row(&mut p, y, 1, row, "  ");
                y += 1;
            }
        }
        if h >= 30 {
            // queue line only when there's room
            if !self.rings.is_empty() {
                let mut r: Vec<_> = self.rings.iter().collect();
                r.sort();
                let rt = "queues: ".to_string()
                    + &r.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join("  ");
                p.put(y, 1, &rt, Painter::cp(4));
            }
            y += 1;
        }

        // graphs
        let table_min = if h >= 30 { 6 } else { 8 };
        let gh = (h - y - table_min).clamp(5, 14).min(h - y - table_min);
        if gh >= 5 {
            let pct = |v: f64| format!("{v:.0}%");
            let right_graph = |p: &mut Painter, gx: i32, gww: i32| {
                if self.mem_total_clients.is_some() {
                    self.mem_graph(p, y, gx, gh, gww);
                } else {
                    let m = hmax(&self.jobs_hist);
                    let jmax = if m > 0.0 { m * 1.2 } else { 1.0 };
                    p.graph(y, gx, gh, gww, &self.jobs_hist, jmax, "jobs/s", 6, &|v| thousands(v));
                }
            };
            if w < 60 {
                if self.show_mem_graph {
                    right_graph(&mut p, 0, w);
                } else {
                    p.graph(y, 0, gh, w, &self.gpu_hist, 100.0, "GPU busy", 1, &pct);
                }
            } else {
                let gw = (w - 1) / 2;
                p.graph(y, 0, gh, gw, &self.gpu_hist, 100.0, if gw >= 40 { "GPU busy" } else { "GPU" }, 1, &pct);
                right_graph(&mut p, gw + 1, w - gw - 1);
            }
            y += gh;
        }

        // process table
        let hdr = format!("{:>7} {:<10} {:>6} {:>10}  COMMAND", "PID", "USER", "GPU%", "GPU MEM");
        p.put(y, 0, &format!("{hdr:<width$}", width = w as usize), Painter::cpb(5));
        y += 1;
        // PID order first, then a stable sort, so equal rows don't reshuffle
        let mut rows: Vec<&Proc> = self.procs.values().collect();
        rows.sort_by_key(|p| p.pid);
        let (sort, rev) = (self.sort, self.reverse);
        rows.sort_by(|a, b| {
            let o = match sort {
                Sort::Gpu => a.gpu.unwrap_or(0.0).total_cmp(&b.gpu.unwrap_or(0.0)),
                Sort::Mem => a.mem.unwrap_or(0.0).total_cmp(&b.mem.unwrap_or(0.0)),
                Sort::Pid => a.pid.cmp(&b.pid),
                Sort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            };
            if rev { o.reverse() } else { o }
        });
        self.rows = rows.iter().map(|p| p.pid).collect();
        // keep the same process selected across refreshes and re-sorts
        if let Some(i) = self.sel_pid.and_then(|sp| self.rows.iter().position(|&r| r == sp)) {
            self.sel = i;
        }
        self.sel = self.sel.min(rows.len().saturating_sub(1));
        self.sel_pid = self.rows.get(self.sel).copied();
        self.page = (h - 1 - y).max(1) as usize;
        if self.sel < self.top {
            self.top = self.sel;
        }
        if self.sel >= self.top + self.page {
            self.top = self.sel + 1 - self.page;
        }
        self.top = self.top.min(rows.len().saturating_sub(self.page));
        if rows.len() > self.page {
            let pos = format!(" {}-{}/{} ", self.top + 1, rows.len().min(self.top + self.page), rows.len());
            p.put(y - 1, w - pos.chars().count() as i32 - 1, &pos, Painter::cpb(5));
        }
        for (i, pr) in rows.iter().skip(self.top).take(self.page).enumerate() {
            let g = pr.gpu.map_or("n/a".into(), |g| format!("{g:.1}"));
            let mem = format!("{}{}", if pr.mem_est { "~" } else { "" }, human(pr.mem));
            let user: String = pr.user.chars().take(10).collect();
            let line = format!("{:>7} {:<10} {:>6} {:>10}  {}", pr.pid, user, g, mem, pr.cmd);
            if self.top + i == self.sel {
                let st = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);
                p.put(y, 0, &format!("{line:<width$}", width = w as usize), st);
            } else {
                let gv = pr.gpu.unwrap_or(0.0);
                let st = if gv >= 1.0 { p.grad(gv / 100.0) } else { Style::default() };
                p.put(y, 0, &line, st);
            }
            y += 1;
        }
        if rows.is_empty() {
            p.put(y, 2, "No processes have the GPU open.", Style::default());
        }

        // footer
        let sortname = match self.sort {
            Sort::Gpu => "GPU%",
            Sort::Mem => "memory",
            Sort::Pid => "PID",
            Sort::Name => "name",
        };
        let mut foot = format!(
            " h help {} q quit  g/m/p/n sort ({sortname}{})  r reverse  z zoom mem  +/- interval ",
            if w < 60 { " Tab graph " } else { "" },
            if self.reverse { " ↓" } else { " ↑" }
        );
        if self.tracer.lost() > 0 {
            foot += &format!("  [{} events lost]", self.tracer.lost());
        }
        if !self.tracer.attributed && self.busy_pct > 0.0 {
            foot += "  [no per-process attribution from trace yet]";
        }
        p.put(h - 1, 0, &format!("{foot:<width$}", width = w as usize), Painter::cp(5));
        if self.help {
            Self::draw_help(&mut p);
        }
    }

    fn draw_help(p: &mut Painter) {
        let lines: [(&str, bool); 21] = [
            ("Keys", true),
            ("  ↑ / ↓            select process", false),
            ("  PgUp / PgDn      scroll a page", false),
            ("  Home / End       first / last process", false),
            ("  g  m  p  n       sort by GPU% / memory / PID / name", false),
            ("                   (press again to reverse)", false),
            ("  r                reverse sort order", false),
            ("  z                zoom memory graph to what's in use", false),
            ("  Tab              switch graph (narrow terminals)", false),
            ("  +  -             slower / faster refresh", false),
            ("  h  ?  F1         this help        q  quit", false),
            ("", false),
            ("Reading the screen", true),
            ("  GPU busy   time the GPU had work queued or running;", false),
            ("             can read higher than real shader load", false),
            ("  GPU%       each process's share of that time", false),
            ("  ~ memory   estimate from GPU buffers each process maps", false),
            ("  MEM bar    magenta = GPU, blue = everything else", false),
            ("  timeouts   GPU jobs the driver had to kill this boot", false),
            ("", false),
            ("              press any key to close", false),
        ];
        let (h, w) = (p.h, p.w);
        let longest = lines.iter().map(|(t, _)| t.chars().count()).max().unwrap_or(0) as i32;
        let bw = (w - 2).min(longest + 4);
        let bh = (h - 2).min(lines.len() as i32 + 2);
        let (y0, x0) = (((h - bh) / 2).max(0), ((w - bw) / 2).max(0));
        let st = Painter::cp(5);
        for r in 0..bh {
            p.put(y0 + r, x0, &" ".repeat(bw as usize), st);
        }
        p.boxed(y0, x0, bh, bw, st);
        p.put(y0, x0 + 2, " agxtop help ", Painter::cpb(5));
        for (i, (t, head)) in lines.iter().take((bh - 2).max(0) as usize).enumerate() {
            let t: String = t.chars().take((bw - 4).max(0) as usize).collect();
            p.put(y0 + 1 + i as i32, x0 + 2, &t, if *head { Painter::cpb(5) } else { st });
        }
    }

    fn move_sel(&mut self, delta: i64) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        self.sel = (self.sel as i64 + delta).clamp(0, n as i64 - 1) as usize;
        self.sel_pid = Some(self.rows[self.sel]);
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> Action {
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        if self.help {
            // any key closes help
            self.help = false;
            return Action::Redraw;
        }
        let page = self.page as i64;
        match k.code {
            KeyCode::Char('q' | 'Q') => return Action::Quit,
            KeyCode::Char('h' | '?') | KeyCode::F(1) => self.help = true,
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-page),
            KeyCode::PageDown => self.move_sel(page),
            KeyCode::Home => self.move_sel(-1_000_000),
            KeyCode::End => self.move_sel(1_000_000),
            KeyCode::Char(c @ ('g' | 'm' | 'p' | 'n')) => {
                let s = match c {
                    'g' => Sort::Gpu,
                    'm' => Sort::Mem,
                    'p' => Sort::Pid,
                    _ => Sort::Name,
                };
                if self.sort == s {
                    self.reverse = !self.reverse;
                } else {
                    self.sort = s;
                    self.reverse = !matches!(s, Sort::Pid | Sort::Name);
                }
            }
            KeyCode::Char('r') => self.reverse = !self.reverse,
            KeyCode::Char('z' | 'Z') => self.mem_zoom = !self.mem_zoom,
            KeyCode::Tab => self.show_mem_graph = !self.show_mem_graph,
            KeyCode::Char('+' | '=') => self.interval = (self.interval * 2.0).min(10.0),
            KeyCode::Char('-' | '_') => self.interval = (self.interval / 2.0).max(0.25),
            _ => {}
        }
        Action::Redraw
    }
}
