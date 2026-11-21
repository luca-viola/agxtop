//! GPU busy accounting from the kernel's gpu_scheduler trace events.
//!
//! A job counts as busy from drm_sched_job_run until drm_sched_job_done.
//! drm_sched_job_queue is logged in the submitting process, so each job's
//! fence is tied to a PID. Overlapping time is split between clients by their
//! share of in-flight jobs, so per-process GPU% sums to the total.
//!
//! Events come from perf_event_open (preferred, see perf.rs) or from tracefs
//! text (root only; changes global tracing settings and restores them).

use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use crate::perf::Perf;
use crate::util::{now, tgid};

const EVENTS: [&str; 3] = ["drm_sched_job_queue", "drm_sched_job_run", "drm_sched_job_done"];
const STALE: f64 = 10.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Queue,
    Run,
    Done,
}

/// One scheduler event, whichever source it came from.
#[derive(Clone, Debug)]
pub struct Ev {
    pub kind: Kind,
    pub ts: f64,
    pub tgid: Option<i32>,
    pub fence: (u64, u64),
    pub client: Option<u64>,
    pub ring: Option<String>,
}

/// Who a GPU job is credited to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Client {
    Pid(i32),
    Cid(u64),
}

pub struct Sample {
    pub busy: f64,
    pub cbusy: HashMap<Client, f64>,
    pub jobs: u64,
    pub rings: HashMap<String, u64>,
}

// ------------------------------------------------------------ tracefs text --
pub struct TraceFs {
    root: PathBuf,
    saved: Vec<(String, String)>,
    pipe: Option<fs::File>,
    buf: Vec<u8>,
    re_line: Regex,
    re_fence: Regex,
    re_client: Regex,
    re_ring: Regex,
}

impl TraceFs {
    pub fn new(root: PathBuf) -> Self {
        TraceFs {
            root,
            saved: Vec::new(),
            pipe: None,
            buf: Vec::new(),
            re_line: Regex::new(
                r"^\s*(.*?)-(\d+)\s+\[\d+\].*?\s(\d+\.\d+):\s+(drm_sched_job_queue|drm_sched_job_run|drm_sched_job_done):(.*)",
            )
            .unwrap(),
            re_fence: Regex::new(r"fence=(\d+):(\d+)").unwrap(),
            re_client: Regex::new(r"client_id:\s*(\d+)").unwrap(),
            re_ring: Regex::new(r"ring=([^,\s]+)").unwrap(),
        }
    }

    fn rd(&self, p: &str) -> io::Result<String> {
        fs::read_to_string(self.root.join(p))
    }

    fn wr(&self, p: &str, v: &str) -> io::Result<()> {
        fs::write(self.root.join(p), v)
    }

    pub fn start(&mut self) -> io::Result<()> {
        let clock = self.rd("trace_clock")?;
        let cur = clock
            .split_whitespace()
            .find(|w| w.starts_with('['))
            .map(|w| w.trim_matches(|c| c == '[' || c == ']').to_string())
            .unwrap_or_else(|| "local".into());
        self.saved.push(("trace_clock".into(), cur));
        self.saved.push(("tracing_on".into(), self.rd("tracing_on")?.trim().to_string()));
        for e in EVENTS {
            let k = format!("events/gpu_scheduler/{e}/enable");
            let v = self.rd(&k)?.trim().to_string();
            self.saved.push((k, v));
        }
        self.wr("trace_clock", "mono")?;
        for e in EVENTS {
            self.wr(&format!("events/gpu_scheduler/{e}/enable"), "1")?;
        }
        self.wr("tracing_on", "1")?;
        self.pipe = Some(
            fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(self.root.join("trace_pipe"))?,
        );
        Ok(())
    }

    /// Restore the tracefs settings saved by start(). Safe to call twice.
    pub fn stop(&mut self) {
        self.pipe = None;
        for (k, v) in std::mem::take(&mut self.saved) {
            let _ = self.wr(&k, &v);
        }
    }

    fn read(&mut self, timeout: f64, out: &mut Vec<Ev>) {
        let Some(pipe) = self.pipe.as_mut() else {
            std::thread::sleep(std::time::Duration::from_secs_f64(timeout));
            return;
        };
        let mut pfd = libc::pollfd { fd: pipe.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, (timeout * 1000.0) as i32) } <= 0 {
            return;
        }
        let mut chunk = vec![0u8; 1 << 16];
        match pipe.read(&mut chunk) {
            Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
            Err(_) => return,
        }
        let Some(cut) = self.buf.iter().rposition(|&b| b == b'\n') else { return };
        let tail = self.buf.split_off(cut + 1);
        let data = std::mem::replace(&mut self.buf, tail);
        for line in String::from_utf8_lossy(&data).lines() {
            out.extend(self.parse(line));
        }
    }

    /// Turn one line of trace_pipe output into an event.
    pub fn parse(&self, line: &str) -> Option<Ev> {
        let m = self.re_line.captures(line)?;
        let tid: i32 = m[2].parse().ok()?;
        let fm = self.re_fence.captures(&m[5])?;
        let rest = &m[5];
        let kind = match &m[4] {
            "drm_sched_job_queue" => Kind::Queue,
            "drm_sched_job_run" => Kind::Run,
            _ => Kind::Done,
        };
        Some(Ev {
            kind,
            ts: m[3].parse().ok()?,
            tgid: (kind == Kind::Queue && tid > 0).then(|| tgid(tid)),
            fence: (fm[1].parse().ok()?, fm[2].parse().ok()?),
            client: self.re_client.captures(rest).and_then(|c| c[1].parse().ok()),
            ring: self.re_ring.captures(rest).map(|r| r[1].to_string()),
        })
    }
}

impl Drop for TraceFs {
    fn drop(&mut self) {
        self.stop();
    }
}

pub enum Source {
    Perf(Perf),
    TraceFs(TraceFs),
}

// ---------------------------------------------------------------- tracer --
pub struct Tracer {
    src: Source,
    evs: Vec<Ev>,
    inflight: HashMap<(u64, u64), (f64, Option<Client>)>,
    owner: HashMap<(u64, u64), (f64, i32)>,
    ccount: HashMap<Option<Client>, i64>,
    busy: f64,
    cbusy: HashMap<Client, f64>,
    jobs: u64,
    rings: HashMap<String, u64>,
    pub attributed: bool,
    last: f64,
}

impl Tracer {
    pub fn new(src: Source) -> Self {
        Tracer {
            src,
            evs: Vec::new(),
            inflight: HashMap::new(),
            owner: HashMap::new(),
            ccount: HashMap::new(),
            busy: 0.0,
            cbusy: HashMap::new(),
            jobs: 0,
            rings: HashMap::new(),
            attributed: false,
            last: now(),
        }
    }

    pub fn lost(&self) -> u64 {
        match &self.src {
            Source::Perf(p) => p.lost,
            Source::TraceFs(_) => 0,
        }
    }

    pub fn stop(&mut self) {
        if let Source::TraceFs(t) = &mut self.src {
            t.stop();
        }
    }

    fn dec(ccount: &mut HashMap<Option<Client>, i64>, c: Option<Client>) {
        if let Some(n) = ccount.get_mut(&c) {
            *n -= 1;
            if *n <= 0 {
                ccount.remove(&c);
            }
        }
    }

    fn advance(&mut self, ts: f64) {
        if ts > self.last {
            let dt = ts - self.last;
            if !self.inflight.is_empty() {
                self.busy += dt;
                let n_all: i64 = self.ccount.values().filter(|n| **n > 0).sum();
                if n_all > 0 {
                    for (c, n) in &self.ccount {
                        if let (Some(c), true) = (c, *n > 0) {
                            *self.cbusy.entry(*c).or_default() += dt * *n as f64 / n_all as f64;
                        }
                    }
                }
            }
            self.last = ts;
        }
    }

    /// Wait up to `timeout` seconds for events and account for them.
    pub fn poll(&mut self, timeout: f64) {
        let mut evs = std::mem::take(&mut self.evs);
        match &mut self.src {
            Source::Perf(p) => p.read(timeout, &mut evs),
            Source::TraceFs(t) => t.read(timeout, &mut evs),
        }
        // per-CPU buffers are each in order; merge them by time
        evs.sort_by(|a, b| a.ts.total_cmp(&b.ts));
        for ev in evs.drain(..) {
            self.process(ev);
        }
        self.evs = evs;
    }

    pub fn process(&mut self, ev: Ev) {
        match ev.kind {
            Kind::Queue => {
                if let Some(pid) = ev.tgid.filter(|p| *p > 0) {
                    self.owner.insert(ev.fence, (ev.ts, pid));
                }
            }
            Kind::Run => {
                self.advance(ev.ts);
                if self.inflight.contains_key(&ev.fence) {
                    return; // duplicate run; ignore
                }
                let client = if let Some((_, pid)) = self.owner.remove(&ev.fence) {
                    self.attributed = true;
                    Some(Client::Pid(pid))
                } else {
                    ev.client.filter(|c| *c != 0).map(Client::Cid)
                };
                self.inflight.insert(ev.fence, (ev.ts, client));
                *self.ccount.entry(client).or_default() += 1;
                self.jobs += 1;
                if let Some(r) = ev.ring {
                    *self.rings.entry(r).or_default() += 1;
                }
            }
            Kind::Done => {
                self.advance(ev.ts);
                if let Some((_, c)) = self.inflight.remove(&ev.fence) {
                    Self::dec(&mut self.ccount, c);
                }
            }
        }
    }

    /// Close the current window and reset the accumulators.
    pub fn sample(&mut self) -> Sample {
        self.sample_at(now())
    }

    pub fn sample_at(&mut self, t: f64) -> Sample {
        self.advance(t);
        let stale: Vec<(u64, u64)> =
            self.inflight.iter().filter(|(_, (t0, _))| t - t0 > STALE).map(|(f, _)| *f).collect();
        for f in stale {
            if let Some((_, c)) = self.inflight.remove(&f) {
                Self::dec(&mut self.ccount, c);
            }
        }
        self.owner.retain(|_, (t0, _)| t - *t0 <= STALE);
        Sample {
            busy: std::mem::take(&mut self.busy),
            cbusy: std::mem::take(&mut self.cbusy),
            jobs: std::mem::take(&mut self.jobs),
            rings: std::mem::take(&mut self.rings),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracer() -> (Tracer, TraceFs) {
        let mut t = Tracer::new(Source::TraceFs(TraceFs::new(PathBuf::from("/nonexistent"))));
        t.last = 0.0;
        (t, TraceFs::new(PathBuf::from("/nonexistent")))
    }

    #[test]
    fn attributes_jobs_to_submitter() {
        let (mut t, p) = tracer();
        t.last = 100.0;
        let pid = std::process::id() as i32;
        for l in [
            format!("  whisper-cli-{pid}  [003] ..... 100.100000: drm_sched_job_queue: dev=406400000.gpu, fence=12:34, ring=compute, job count:1, hw job count:0"),
            "  <idle>-0  [000] d.h1. 100.200000: drm_sched_job_run: dev=406400000.gpu, fence=12:34, ring=compute, job count:1, hw job count:1, client_id:7".into(),
            "  <idle>-0  [000] d.h1. 100.700000: drm_sched_job_done: fence=12:34 signaled".into(),
        ] {
            t.process(p.parse(&l).unwrap());
        }
        let s = t.sample_at(101.0);
        assert!((s.busy - 0.5).abs() < 1e-6, "busy {}", s.busy);
        assert_eq!(s.jobs, 1);
        assert_eq!(s.rings.get("compute"), Some(&1));
        let c = s.cbusy.get(&Client::Pid(pid)).copied().unwrap_or(0.0);
        assert!((c - 0.5).abs() < 1e-6, "client {c}");
        assert!(t.attributed);
    }

    #[test]
    fn splits_overlap_by_inflight_share() {
        let (mut t, p) = tracer();
        for l in [
            "  k-0  [000] ..... 1.000000: drm_sched_job_run: fence=1:1, ring=a, client_id:1",
            "  k-0  [000] ..... 1.000000: drm_sched_job_run: fence=1:2, ring=a, client_id:2",
            "  k-0  [000] ..... 2.000000: drm_sched_job_done: fence=1:1",
            "  k-0  [000] ..... 2.000000: drm_sched_job_done: fence=1:2",
        ] {
            t.process(p.parse(l).unwrap());
        }
        let s = t.sample_at(3.0);
        assert!((s.busy - 1.0).abs() < 1e-6);
        assert!((s.cbusy[&Client::Cid(1)] - 0.5).abs() < 1e-6);
        assert!((s.cbusy[&Client::Cid(2)] - 0.5).abs() < 1e-6);
    }
}
