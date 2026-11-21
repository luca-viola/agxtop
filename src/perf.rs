//! Read gpu_scheduler tracepoints through perf_event_open instead of tracefs.
//!
//! Nothing global is changed (no tracing_on / enable files), every buffer is
//! per-CPU and private to this process, and it all goes away when the process
//! exits. Needs CAP_PERFMON, plus read access to the events' id/format files
//! (root-only tracefs -> CAP_DAC_READ_SEARCH). Both are dropped after setup.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::sync::atomic::{fence, Ordering};

use crate::tracer::{Ev, Kind};

const EVENTS: [(&str, Kind); 3] = [
    ("drm_sched_job_queue", Kind::Queue),
    ("drm_sched_job_run", Kind::Run),
    ("drm_sched_job_done", Kind::Done),
];

const PERF_TYPE_TRACEPOINT: u32 = 2;
const PERF_SAMPLE_TID: u64 = 1 << 1;
const PERF_SAMPLE_TIME: u64 = 1 << 2;
const PERF_SAMPLE_RAW: u64 = 1 << 10;
const PERF_RECORD_LOST: u32 = 2;
const PERF_RECORD_SAMPLE: u32 = 9;
const FLAG_WATERMARK: u64 = 1 << 14;
const FLAG_USE_CLOCKID: u64 = 1 << 25;
const PERF_EVENT_IOC_SET_OUTPUT: u64 = 0x2405; // _IO('$', 5)
const ATTR_SIZE: usize = 112; // PERF_ATTR_SIZE_VER5, enough for clockid
const PERF_FLAG_FD_CLOEXEC: libc::c_ulong = 1 << 3;

/// Byte offset of a field inside the raw tracepoint record.
#[derive(Clone, Copy, Default)]
struct Field {
    off: usize,
    size: usize,
}

#[derive(Clone)]
struct Layout {
    kind: Kind,
    fence_context: Field,
    fence_seqno: Field,
    client_id: Option<Field>,
    name: Option<Field>, // __data_loc char[] (ring name)
}

fn parse_format(text: &str) -> HashMap<String, Field> {
    // "\tfield:u64 fence_context;\toffset:24;\tsize:8;\tsigned:0;"
    let mut out = HashMap::new();
    for ln in text.lines() {
        let ln = ln.trim();
        let Some(rest) = ln.strip_prefix("field:") else { continue };
        let mut parts = rest.split(';').map(str::trim);
        let decl = parts.next().unwrap_or("");
        let name = decl.split_whitespace().last().unwrap_or("").split('[').next().unwrap_or("");
        let num = |key: &str, parts: &[&str]| {
            parts.iter().find_map(|p| p.strip_prefix(key)).and_then(|v| v.parse::<usize>().ok())
        };
        let parts: Vec<&str> = parts.collect();
        if let (Some(off), Some(size)) = (num("offset:", &parts), num("size:", &parts)) {
            out.insert(name.to_string(), Field { off, size });
        }
    }
    out
}

fn rd_u(raw: &[u8], f: Field) -> Option<u64> {
    let b = raw.get(f.off..f.off + f.size)?;
    Some(match f.size {
        8 => u64::from_ne_bytes(b.try_into().ok()?),
        4 => u32::from_ne_bytes(b.try_into().ok()?) as u64,
        _ => return None,
    })
}

fn rd_loc_str(raw: &[u8], f: Field) -> Option<String> {
    let loc = rd_u(raw, Field { off: f.off, size: 4 })? as usize;
    let (off, len) = (loc & 0xffff, loc >> 16);
    let s = raw.get(off..off + len)?;
    let s = s.split(|&c| c == 0).next().unwrap_or(s);
    Some(String::from_utf8_lossy(s).into_owned())
}

struct Ring {
    base: *mut u8,
    map_len: usize,
    data_off: usize,
    data_size: usize,
}

impl Ring {
    fn head(&self) -> u64 {
        let v = unsafe { std::ptr::read_volatile(self.base.add(1024) as *const u64) };
        fence(Ordering::Acquire);
        v
    }
    fn tail(&self) -> u64 {
        unsafe { std::ptr::read_volatile(self.base.add(1032) as *const u64) }
    }
    fn set_tail(&self, t: u64) {
        fence(Ordering::Release);
        unsafe { std::ptr::write_volatile(self.base.add(1032) as *mut u64, t) };
    }
    fn copy(&self, pos: u64, out: &mut [u8]) {
        let data = unsafe { self.base.add(self.data_off) };
        for (i, b) in out.iter_mut().enumerate() {
            *b = unsafe { *data.add((pos as usize + i) % self.data_size) };
        }
    }
}

pub struct Perf {
    fds: Vec<i32>,
    rings: Vec<Ring>,
    pollfds: Vec<libc::pollfd>,
    layouts: HashMap<u16, Layout>, // common_type (tracepoint id) -> layout
    pub lost: u64,
}

fn perf_open(id: u64, cpu: i32, wakeup_bytes: u32) -> io::Result<i32> {
    perf_open_raw(PERF_TYPE_TRACEPOINT, id, 1, PERF_SAMPLE_TID | PERF_SAMPLE_TIME | PERF_SAMPLE_RAW, -1, cpu, wakeup_bytes, 0)
}

#[allow(clippy::too_many_arguments)]
fn perf_open_raw(ty: u32, config: u64, period: u64, sample_type: u64, pid: i32, cpu: i32, wakeup_bytes: u32,
                 extra_flags: u64)
    -> io::Result<i32> {
    let mut attr = [0u8; ATTR_SIZE];
    let put32 = |a: &mut [u8], off: usize, v: u32| a[off..off + 4].copy_from_slice(&v.to_ne_bytes());
    let put64 = |a: &mut [u8], off: usize, v: u64| a[off..off + 8].copy_from_slice(&v.to_ne_bytes());
    put32(&mut attr, 0, ty); // type
    put32(&mut attr, 4, ATTR_SIZE as u32); // size
    put64(&mut attr, 8, config); // config
    put64(&mut attr, 16, period); // sample_period
    put64(&mut attr, 24, sample_type);
    put64(&mut attr, 40, FLAG_WATERMARK | FLAG_USE_CLOCKID | extra_flags); // enabled from the start
    put32(&mut attr, 48, wakeup_bytes); // wakeup_watermark
    put32(&mut attr, 92, libc::CLOCK_MONOTONIC as u32); // clockid: same clock as now()
    let fd = unsafe {
        libc::syscall(libc::SYS_perf_event_open, attr.as_ptr(), pid, cpu, -1, PERF_FLAG_FD_CLOEXEC)
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(fd as i32)
    }
}

fn online_cpus() -> Vec<i32> {
    let s = fs::read_to_string("/sys/devices/system/cpu/online").unwrap_or_else(|_| "0".into());
    let mut v = Vec::new();
    for part in s.trim().split(',') {
        let mut it = part.split('-').filter_map(|x| x.parse::<i32>().ok());
        if let Some(a) = it.next() {
            let b = it.next().unwrap_or(a);
            v.extend(a..=b);
        }
    }
    v
}

impl Perf {
    pub fn open(tracefs: &str) -> io::Result<Perf> {
        let mut layouts = HashMap::new();
        let mut ids = Vec::new();
        for (name, kind) in EVENTS {
            let dir = format!("{tracefs}/events/gpu_scheduler/{name}");
            let id: u64 = fs::read_to_string(format!("{dir}/id"))?
                .trim()
                .parse()
                .map_err(|_| io::Error::other("bad tracepoint id"))?;
            let f = parse_format(&fs::read_to_string(format!("{dir}/format"))?);
            let need = |n: &str| f.get(n).copied().ok_or_else(|| io::Error::other(format!("{name}: no field {n}")));
            layouts.insert(
                id as u16,
                Layout {
                    kind,
                    fence_context: need("fence_context")?,
                    fence_seqno: need("fence_seqno")?,
                    client_id: f.get("client_id").copied(),
                    name: f.get("name").copied(),
                },
            );
            ids.push(id);
        }
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let mut p = Perf { fds: Vec::new(), rings: Vec::new(), pollfds: Vec::new(), layouts, lost: 0 };
        for cpu in online_cpus() {
            // one ring buffer per CPU; the other events on that CPU write into it
            let mut leader = None;
            for &id in &ids {
                let fd = match perf_open(id, cpu, (page * 2) as u32) {
                    Ok(fd) => fd,
                    Err(e) if p.fds.is_empty() => return Err(e),
                    Err(_) => continue, // e.g. CPU went offline
                };
                p.fds.push(fd);
                match leader {
                    None => {
                        let ring = Self::map(fd, page)?;
                        p.pollfds.push(libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
                        p.rings.push(ring);
                        leader = Some(fd);
                    }
                    Some(l) => {
                        if unsafe { libc::ioctl(fd, PERF_EVENT_IOC_SET_OUTPUT as _, l) } != 0 {
                            return Err(io::Error::last_os_error());
                        }
                    }
                }
            }
        }
        Ok(p)
    }

    /// mmap 1 control page + 2^n data pages, smaller if the mlock limit bites.
    fn map(fd: i32, page: usize) -> io::Result<Ring> {
        let mut last = io::Error::other("mmap");
        for n in [8usize, 4, 2, 1] {
            let len = page * (n + 1);
            let base = unsafe {
                libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0)
            };
            if base == libc::MAP_FAILED {
                last = io::Error::last_os_error();
                continue;
            }
            let base = base as *mut u8;
            let rd = |off: usize| unsafe { std::ptr::read_volatile(base.add(off) as *const u64) } as usize;
            let (data_off, data_size) = match (rd(1040), rd(1048)) {
                (0, _) | (_, 0) => (page, page * n), // pre-4.1 kernels
                v => v,
            };
            return Ok(Ring { base, map_len: len, data_off, data_size });
        }
        Err(last)
    }

    /// Wait up to `timeout` seconds, then drain every CPU buffer.
    pub fn read(&mut self, timeout: f64, out: &mut Vec<Ev>) {
        unsafe { libc::poll(self.pollfds.as_mut_ptr(), self.pollfds.len() as _, (timeout * 1000.0) as i32) };
        let mut hdr = [0u8; 8];
        let mut rec = Vec::new();
        for ring in &self.rings {
            let head = ring.head();
            let mut tail = ring.tail();
            while tail + 8 <= head {
                ring.copy(tail, &mut hdr);
                let ty = u32::from_ne_bytes(hdr[0..4].try_into().unwrap());
                let size = u16::from_ne_bytes(hdr[6..8].try_into().unwrap()) as u64;
                if size < 8 || tail + size > head {
                    break;
                }
                rec.resize(size as usize, 0);
                ring.copy(tail, &mut rec);
                match ty {
                    PERF_RECORD_SAMPLE => {
                        if let Some(ev) = self.decode(&rec[8..]) {
                            out.push(ev);
                        }
                    }
                    PERF_RECORD_LOST if rec.len() >= 24 => {
                        self.lost += u64::from_ne_bytes(rec[16..24].try_into().unwrap());
                    }
                    _ => {}
                }
                tail += size;
            }
            ring.set_tail(tail);
        }
    }

    fn decode(&self, b: &[u8]) -> Option<Ev> {
        // { u32 pid, tid; u64 time; u32 size; char data[size]; }
        let pid = u32::from_ne_bytes(b.get(0..4)?.try_into().ok()?) as i32;
        let time = u64::from_ne_bytes(b.get(8..16)?.try_into().ok()?);
        let size = u32::from_ne_bytes(b.get(16..20)?.try_into().ok()?) as usize;
        let raw = b.get(20..20 + size)?;
        let common_type = u16::from_ne_bytes(raw.get(0..2)?.try_into().ok()?);
        let l = self.layouts.get(&common_type)?;
        Some(Ev {
            kind: l.kind,
            ts: time as f64 * 1e-9,
            tgid: Some(pid),
            fence: (rd_u(raw, l.fence_context)?, rd_u(raw, l.fence_seqno)?),
            client: l.client_id.and_then(|f| rd_u(raw, f)),
            ring: l.name.and_then(|f| rd_loc_str(raw, f)),
        })
    }
}

impl Drop for Perf {
    fn drop(&mut self) {
        for r in &self.rings {
            unsafe { libc::munmap(r.base as *mut libc::c_void, r.map_len) };
        }
        for fd in &self.fds {
            unsafe { libc::close(*fd) };
        }
    }
}

/// Drop every capability (file caps from setcap are only needed for setup).
pub fn drop_caps() {
    #[repr(C)]
    struct Hdr {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let hdr = Hdr { version: 0x2008_0522, pid: 0 }; // _LINUX_CAPABILITY_VERSION_3
    let data = [Data { effective: 0, permitted: 0, inheritable: 0 }, Data { effective: 0, permitted: 0, inheritable: 0 }];
    unsafe { libc::syscall(libc::SYS_capset, &hdr, data.as_ptr()) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises attr layout, use_clockid, mmap and ring reading with an
    /// unprivileged software event on this process.
    #[test]
    fn ring_buffer_with_software_event() {
        const PERF_TYPE_SOFTWARE: u32 = 1;
        const PERF_COUNT_SW_TASK_CLOCK: u64 = 1;
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let fd = perf_open_raw(PERF_TYPE_SOFTWARE, PERF_COUNT_SW_TASK_CLOCK, 1_000_000,
                               PERF_SAMPLE_TID | PERF_SAMPLE_TIME, 0, -1, page as u32,
                               (1 << 5) | (1 << 6)) // exclude_kernel|hv: allowed at paranoid 2
            .expect("perf_event_open (software)");
        let ring = Perf::map(fd, page).expect("mmap");
        let t0 = crate::util::now();
        let mut x = 0u64;
        while crate::util::now() - t0 < 0.05 { x = x.wrapping_add(1); }
        std::hint::black_box(x);
        let (head, mut tail) = (ring.head(), ring.tail());
        let (mut n, mut hdr) = (0, [0u8; 8]);
        let me = std::process::id();
        while tail + 8 <= head {
            ring.copy(tail, &mut hdr);
            let ty = u32::from_ne_bytes(hdr[0..4].try_into().unwrap());
            let size = u16::from_ne_bytes(hdr[6..8].try_into().unwrap()) as u64;
            let mut rec = vec![0u8; size as usize];
            ring.copy(tail, &mut rec);
            if ty == PERF_RECORD_SAMPLE {
                let pid = u32::from_ne_bytes(rec[8..12].try_into().unwrap());
                let ts = u64::from_ne_bytes(rec[16..24].try_into().unwrap()) as f64 * 1e-9;
                assert_eq!(pid, me);
                assert!((ts - t0).abs() < 1.0, "timestamp {ts} not on CLOCK_MONOTONIC (now {t0})");
                n += 1;
            }
            tail += size;
        }
        ring.set_tail(tail);
        assert!(n >= 10, "only {n} samples");
        unsafe { libc::munmap(ring.base as *mut libc::c_void, ring.map_len); libc::close(fd); }
    }
    #[test]
    fn parses_format_and_decodes_record() {
        let fmt = "name: drm_sched_job_run\nID: 1234\nformat:\n\tfield:unsigned short common_type;\toffset:0;\tsize:2;\tsigned:0;\n\tfield:int common_pid;\toffset:4;\tsize:4;\tsigned:1;\n\n\tfield:__data_loc char[] name;\toffset:8;\tsize:4;\tsigned:0;\n\tfield:u32 job_count;\toffset:12;\tsize:4;\tsigned:0;\n\tfield:u64 fence_context;\toffset:24;\tsize:8;\tsigned:0;\n\tfield:u64 fence_seqno;\toffset:32;\tsize:8;\tsigned:0;\n\tfield:u64 client_id;\toffset:40;\tsize:8;\tsigned:0;\n";
        let f = parse_format(fmt);
        assert_eq!(f["fence_seqno"].off, 32);
        assert_eq!(f["name"].size, 4);
        let mut raw = vec![0u8; 56];
        raw[0..2].copy_from_slice(&1234u16.to_ne_bytes());
        raw[8..12].copy_from_slice(&((8u32 << 16) | 48).to_ne_bytes()); // "render\0\0" at 48
        raw[48..54].copy_from_slice(b"render");
        raw[24..32].copy_from_slice(&5u64.to_ne_bytes());
        raw[32..40].copy_from_slice(&77u64.to_ne_bytes());
        raw[40..48].copy_from_slice(&42u64.to_ne_bytes());
        let mut layouts = HashMap::new();
        layouts.insert(1234, Layout {
            kind: Kind::Run,
            fence_context: f["fence_context"],
            fence_seqno: f["fence_seqno"],
            client_id: f.get("client_id").copied(),
            name: f.get("name").copied(),
        });
        let p = Perf { fds: vec![], rings: vec![], pollfds: vec![], layouts, lost: 0 };
        let mut sample = Vec::new();
        sample.extend_from_slice(&321u32.to_ne_bytes());
        sample.extend_from_slice(&322u32.to_ne_bytes());
        sample.extend_from_slice(&1_500_000_000u64.to_ne_bytes());
        sample.extend_from_slice(&(raw.len() as u32).to_ne_bytes());
        sample.extend_from_slice(&raw);
        let ev = p.decode(&sample).unwrap();
        assert_eq!(ev.fence, (5, 77));
        assert_eq!(ev.client, Some(42));
        assert_eq!(ev.tgid, Some(321));
        assert_eq!(ev.ring.as_deref(), Some("render"));
        assert!((ev.ts - 1.5).abs() < 1e-9);
    }
}
