//! agxtop — nvtop-style GPU monitor for Asahi Linux (Apple Silicon).
//!
//! Data sources (the driver has no performance counters yet):
//!   * GPU busy %   : kernel gpu_scheduler trace events (run -> done).
//!   * Per-process  : drm_sched_job_queue is logged in the submitting process.
//!   * GPU memory   : drm-* keys in fdinfo, else mapped GPU buffer sizes (~).
//!   * GPU layout   : DRM_IOCTL_ASAHI_GET_PARAMS, else driver boot messages.
//!   * Power / fans : macsmc hwmon sensors (whole machine).

mod perf;
mod sys;
mod tracer;
mod ui;
mod util;

use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tracer::{Source, TraceFs, Tracer};
use ui::{Action, Ui};
use util::now;

const USAGE: &str = "usage: agxtop [--all-drivers]

nvtop-style GPU monitor for Asahi Linux.
  --all-drivers   include GPU devices of every driver, not just asahi
  -h, --help      show this help

Runs without sudo after a one-time:  sudo setcap cap_perfmon,cap_dac_read_search+ep $(which agxtop)";

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
}

fn color256() -> bool {
    let term = std::env::var("TERM").unwrap_or_default();
    std::env::var("COLORTERM").map(|c| !c.is_empty()).unwrap_or(false)
        || term.contains("256color")
        || term.contains("direct")
        || ["xterm-kitty", "alacritty", "foot", "wezterm", "xterm-ghostty"].contains(&term.as_str())
}

fn start_tracefs(dir: PathBuf) -> Source {
    if !dir.join("events/gpu_scheduler").is_dir() {
        eprintln!("No gpu_scheduler trace events under {}.", dir.display());
        std::process::exit(1);
    }
    let mut t = TraceFs::new(dir);
    if let Err(e) = t.start() {
        drop(t); // restores whatever was already changed
        eprintln!("agxtop: cannot set up tracing: {e}");
        std::process::exit(1);
    }
    Source::TraceFs(t)
}

fn run(ui: &mut Ui, term: &mut Terminal<CrosstermBackend<io::Stdout>>, stop: &AtomicBool) -> io::Result<()> {
    let mut win = now();
    ui.refresh_data(0.001); // initial process scan
    term.draw(|f| ui.draw(f.buffer_mut()))?;
    while !stop.load(Ordering::Relaxed) {
        ui.poll_trace(0.05);
        let mut redraw = false;
        while event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => match ui.handle_key(k) {
                    Action::Quit => return Ok(()),
                    Action::Redraw => redraw = true,
                },
                // ratatui's autoresize clears and fully repaints on the next draw
                Event::Resize(..) => redraw = true,
                _ => {} // mouse wheel / clicks: ignored
            }
        }
        let t = now();
        if t - win >= ui.interval {
            ui.refresh_data(t - win);
            win = t;
            redraw = true;
        }
        if redraw {
            term.draw(|f| ui.draw(f.buffer_mut()))?;
        }
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return;
    }
    if let Some(a) = args.iter().find(|a| *a != "--all-drivers") {
        eprintln!("agxtop: unknown argument {a}\n\n{USAGE}");
        std::process::exit(2);
    }
    let drivers: HashSet<String> = if args.iter().any(|a| a == "--all-drivers") {
        HashSet::new()
    } else {
        sys::gpu_driver_names().into_iter().filter(|d| d == "asahi").collect()
    };

    let stop = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT, signal_hook::consts::SIGHUP] {
        let _ = signal_hook::flag::register(sig, Arc::clone(&stop));
    }

    let root = unsafe { libc::geteuid() } == 0;
    let src = match std::env::var("ASAHI_GPUTOP_TRACEFS") {
        // test hook: read a (fake) tracefs; never with borrowed privileges
        Ok(dir) => {
            if !root {
                perf::drop_caps();
            }
            start_tracefs(PathBuf::from(dir))
        }
        Err(_) => match perf::Perf::open("/sys/kernel/tracing") {
            Ok(p) => {
                if !root {
                    perf::drop_caps(); // setcap'd binary: privileges only needed for setup
                }
                Source::Perf(p)
            }
            Err(_) if root => start_tracefs(PathBuf::from("/sys/kernel/tracing")),
            Err(e) => {
                let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or("agxtop".into());
                eprintln!("agxtop can't read GPU events ({e}). One-time setup:\n  sudo setcap cap_perfmon,cap_dac_read_search+ep {exe}");
                std::process::exit(1);
            }
        },
    };
    let tracer = Tracer::new(src);

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        hook(info);
    }));

    let mut ui = Ui::new(tracer, drivers, color256());
    let result = (|| -> io::Result<()> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        term.hide_cursor()?;
        run(&mut ui, &mut term, &stop)
    })();
    restore_terminal();
    ui.stop_trace();
    if let Err(e) = result {
        eprintln!("agxtop: {e}");
        std::process::exit(1);
    }
}
