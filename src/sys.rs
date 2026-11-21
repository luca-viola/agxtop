//! Everything read from /proc, /sys, /dev and the kernel log.

use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::OnceLock;

use crate::util::{dev_major, dev_minor, username};

fn basename_link(p: impl AsRef<Path>) -> Option<String> {
    fs::read_link(p).ok()?.file_name().map(|s| s.to_string_lossy().into_owned())
}

fn glob_sorted(dir: &str, prefix: &str, suffix: &str) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    (n.starts_with(prefix) && n.ends_with(suffix)).then(|| format!("{dir}/{n}"))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

// --------------------------------------------------------------- processes --
pub fn gpu_driver_names() -> HashSet<String> {
    glob_sorted("/sys/class/drm", "renderD", "")
        .iter()
        .filter_map(|d| basename_link(format!("{d}/device/driver")))
        .collect()
}

fn rdev_driver(rdev: u64, cache: &mut HashMap<u64, Option<String>>) -> Option<String> {
    cache
        .entry(rdev)
        .or_insert_with(|| {
            basename_link(format!("/sys/dev/char/{}:{}/device/driver", dev_major(rdev), dev_minor(rdev)))
        })
        .clone()
}

#[derive(Clone, Debug, Default)]
pub struct Proc {
    pub pid: i32,
    pub name: String,
    pub cmd: String,
    pub user: String,
    pub clients: HashSet<u64>,
    pub mem: Option<f64>,
    pub mem_est: bool,
    pub gpu: Option<f64>,
    /// resident and swapped-out size of the whole process, in bytes
    pub rss: Option<f64>,
    pub swap: Option<f64>,
    /// CPU time used so far (user + system), seconds; `cpu` is the % since
    /// the last refresh, filled in by the UI
    pub cpu_secs: Option<f64>,
    pub cpu: Option<f64>,
    paths: HashSet<String>,
}

/// user + system CPU time from /proc/<pid>/stat, in seconds
fn cpu_secs(pid: i32) -> Option<f64> {
    let t = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // fields after "(comm)": state is first, utime and stime are 12th and 13th
    let mut it = t[t.rfind(')')? + 1..].split_whitespace().skip(11);
    let ticks = it.next()?.parse::<f64>().ok()? + it.next()?.parse::<f64>().ok()?;
    Some(ticks / unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64)
}

/// (resident, swapped out) in bytes from /proc/<pid>/status
fn rss_swap(pid: i32) -> (Option<f64>, Option<f64>) {
    let t = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let kb = |key: &str| {
        t.lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.split_whitespace().next()?.parse::<f64>().ok())
            .map(|v| v * 1024.0)
    };
    (kb("VmRSS:"), kb("VmSwap:"))
}

/// Size of GPU buffers mapped into the process: device-node mappings,
/// shmem-backed GEM objects ("/drm mm object") and dma-bufs. GPU buffers are
/// mapped as raw page ranges the kernel usually leaves out of Rss, so fall
/// back to the mapping size when Rss is zero.
fn mapped_gpu_bytes(pid: i32, paths: &HashSet<String>) -> Option<f64> {
    let text = fs::read_to_string(format!("/proc/{pid}/smaps")).ok()?;
    let (mut rss, mut size) = (0u64, 0u64);
    let mut cur = false;
    for ln in text.lines() {
        let first = ln.split_whitespace().next().unwrap_or("");
        let is_hdr = first.contains('-') && first.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
        if is_hdr {
            // the mapping name is everything after the 5th field
            let name = ln.split_whitespace().skip(5).collect::<Vec<_>>().join(" ");
            cur = paths.contains(&name) || name.starts_with("/drm mm object") || name.contains("dmabuf");
        } else if cur {
            let kb = || ln.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024;
            if ln.starts_with("Rss:") {
                rss += kb();
            } else if ln.starts_with("Size:") {
                size += kb();
            }
        }
    }
    Some(if rss > 0 { rss } else { size } as f64)
}

/// Pick the most meaningful memory figure from a client's drm-* keys.
fn client_mem(mem: &HashMap<(String, String), f64>) -> Option<f64> {
    for kind in ["resident", "total", "memory"] {
        let vals: Vec<f64> = mem.iter().filter(|((k, _), _)| k == kind).map(|(_, v)| *v).collect();
        if !vals.is_empty() {
            return Some(vals.iter().sum());
        }
    }
    None
}

fn memkey() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^drm-(total|resident|memory|shared|active|purgeable)-([\w.-]+):\s+(\d+)\s*(\w*)").unwrap()
    })
}

pub struct Scanner {
    rdev_cache: HashMap<u64, Option<String>>,
}

impl Scanner {
    pub fn new() -> Self {
        Scanner { rdev_cache: HashMap::new() }
    }

    /// Processes with a GPU device open (plus `extra` PIDs seen submitting jobs).
    pub fn scan(&mut self, drivers: &HashSet<String>, extra: &HashSet<i32>) -> HashMap<i32, Proc> {
        let mut procs = HashMap::new();
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let Ok(rd) = fs::read_dir("/proc") else { return procs };
        for e in rd.flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
            let piddir = format!("/proc/{pid}");
            let fds = match fs::read_dir(format!("{piddir}/fd")) {
                Ok(fds) => Some(fds),
                Err(_) if extra.contains(&pid) => None, // e.g. another user's process
                Err(_) => continue,
            };
            let mut info: Option<Proc> = None;
            for fd in fds.into_iter().flatten().flatten() {
                let fdpath = fd.path();
                let Ok(tgt) = fs::read_link(&fdpath) else { continue };
                let tgt = tgt.to_string_lossy().into_owned();
                if !tgt.starts_with("/dev/dri/") {
                    continue;
                }
                let Ok(md) = fs::metadata(&fdpath) else { continue };
                let rdev = md.rdev();
                let fdname = fd.file_name().to_string_lossy().into_owned();
                let text = fs::read_to_string(format!("{piddir}/fdinfo/{fdname}")).unwrap_or_default();
                let mut kv: HashMap<&str, &str> = HashMap::new();
                let mut mem: HashMap<(String, String), f64> = HashMap::new();
                for ln in text.lines() {
                    if let Some((k, v)) = ln.split_once(':') {
                        kv.insert(k.trim(), v.trim());
                    }
                    if let Some(m) = memkey().captures(ln) {
                        let unit = match &m[4] {
                            "KiB" => 1024.0,
                            "MiB" => 1024f64.powi(2),
                            "GiB" => 1024f64.powi(3),
                            _ => 1.0,
                        };
                        let v: f64 = m[3].parse().unwrap_or(0.0);
                        mem.insert((m[1].to_string(), m[2].to_string()), v * unit);
                    }
                }
                let drv = kv
                    .get("drm-driver")
                    .map(|s| s.to_string())
                    .or_else(|| rdev_driver(rdev, &mut self.rdev_cache));
                let Some(drv) = drv else { continue };
                if !drivers.is_empty() && !drivers.contains(&drv) {
                    continue;
                }
                let cid = kv.get("drm-client-id").map(|s| s.to_string());
                let key = match &cid {
                    Some(c) => (drv.clone(), c.clone()),
                    None => (format!("pid:{pid}"), rdev.to_string()),
                };
                if !seen.insert(key) {
                    continue;
                }
                let p = info.get_or_insert_with(|| Proc { pid, ..Default::default() });
                p.paths.insert(tgt);
                if let Some(c) = cid.and_then(|c| c.parse().ok()) {
                    p.clients.insert(c);
                }
                if let Some(b) = client_mem(&mem) {
                    p.mem = Some(p.mem.unwrap_or(0.0) + b);
                }
            }
            let mut p = match info {
                Some(p) => p,
                None if extra.contains(&pid) => Proc { pid, ..Default::default() },
                None => continue,
            };
            if p.mem.is_none() && !p.paths.is_empty() {
                p.mem = mapped_gpu_bytes(pid, &p.paths);
                p.mem_est = p.mem.is_some();
            }
            let Ok(name) = fs::read_to_string(format!("{piddir}/comm")) else { continue };
            let Ok(cmd) = fs::read(format!("{piddir}/cmdline")) else { continue };
            let Ok(md) = fs::metadata(&piddir) else { continue };
            p.name = name.trim().to_string();
            let cmd = String::from_utf8_lossy(&cmd).replace('\0', " ").trim().to_string();
            p.cmd = if cmd.is_empty() { p.name.clone() } else { cmd };
            p.user = username(md.uid());
            p.cpu_secs = cpu_secs(pid);
            (p.rss, p.swap) = rss_swap(pid);
            procs.insert(pid, p);
        }
        procs
    }
}

// ------------------------------------------------------- GPU info & health --
#[derive(Clone, Debug, Default)]
pub struct GpuInfo {
    pub gtype: Option<String>,
    pub clusters: Option<u32>,
    pub cores: Option<u32>,
    pub total_cores: Option<u32>,
    pub max_mhz: Option<u32>,
    pub chip_id: Option<u32>,
    pub timeouts: u64,
    pub src: Option<&'static str>,
}

/// struct drm_asahi_params_global (upstream UAPI, include/uapi/drm/asahi_drm.h)
#[repr(C)]
struct ParamsGlobal {
    features: u64,
    gpu_generation: u32,
    gpu_variant: u32,
    gpu_revision: u32,
    chip_id: u32,
    num_dies: u32,
    num_clusters_total: u32,
    num_cores_per_cluster: u32,
    max_frequency_khz: u32,
    core_masks: [u64; 64],
    vm_start: u64,
    vm_end: u64,
    vm_kernel_min_size: u64,
    max_commands_per_submission: u32,
    max_attachments: u32,
    command_timestamp_frequency_hz: u64,
}

/// struct drm_asahi_get_params
#[repr(C)]
struct GetParams {
    param_group: u32,
    pad: u32,
    pointer: u64,
    size: u64,
}

const fn iow(ty: u8, nr: u8, size: usize) -> u64 {
    (1 << 30) | ((size as u64) << 16) | ((ty as u64) << 8) | nr as u64
}
// DRM_IOCTL_ASAHI_GET_PARAMS = _IOW('d', DRM_COMMAND_BASE + 0, struct drm_asahi_get_params)
const DRM_IOCTL_ASAHI_GET_PARAMS: u64 = iow(b'd', 0x40, std::mem::size_of::<GetParams>());

/// Ask the asahi driver for the GPU layout. Works on every Asahi machine
/// regardless of logs; no root needed.
pub fn agx_params() -> Option<GpuInfo> {
    for node in glob_sorted("/dev/dri", "renderD", "") {
        let base = node.rsplit('/').next().unwrap();
        if basename_link(format!("/sys/class/drm/{base}/device/driver")).as_deref() != Some("asahi") {
            continue;
        }
        let Ok(f) = fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC).open(&node)
        else {
            continue;
        };
        let mut p: ParamsGlobal = unsafe { std::mem::zeroed() };
        let mut arg = GetParams {
            param_group: 0,
            pad: 0,
            pointer: &mut p as *mut ParamsGlobal as u64,
            size: std::mem::size_of::<ParamsGlobal>() as u64,
        };
        let rc = unsafe { libc::ioctl(f.as_raw_fd(), DRM_IOCTL_ASAHI_GET_PARAMS as _, &mut arg) };
        if rc != 0 {
            continue;
        }
        let (nclus, per) = (p.num_clusters_total, p.num_cores_per_cluster);
        if !(1..=64).contains(&nclus) || !(1..=64).contains(&per) {
            continue; // layout mismatch (old downstream UAPI)
        }
        let mask = if per == 64 { u64::MAX } else { (1u64 << per) - 1 };
        let active: u32 = p.core_masks[..nclus as usize].iter().map(|m| (m & mask).count_ones()).sum();
        let letter = char::from_u32(p.gpu_variant).filter(|c| c.is_ascii_uppercase());
        return Some(GpuInfo {
            gtype: Some(format!("G{}{}", p.gpu_generation, letter.map(String::from).unwrap_or_default())),
            clusters: Some(nclus),
            cores: (active > 0).then_some(active),
            total_cores: Some(nclus * per),
            max_mhz: (p.max_frequency_khz >= 1000).then_some(p.max_frequency_khz / 1000),
            chip_id: Some(p.chip_id),
            timeouts: 0,
            src: Some("ioctl"),
        });
    }
    None
}

/// GPU layout (ioctl first, then the driver's boot messages) and this boot's
/// GPU timeout count.
pub fn gpu_info() -> GpuInfo {
    let mut info = agx_params().unwrap_or_default();
    let mut text = String::new();
    for cmd in [&["journalctl", "-k", "-b", "-o", "cat", "--no-pager"][..], &["dmesg"][..]] {
        let Ok(out) = std::process::Command::new(cmd[0])
            .args(&cmd[1..])
            .stderr(std::process::Stdio::null())
            .output()
        else {
            continue;
        };
        text = String::from_utf8_lossy(&out.stdout).into_owned();
        if text.contains("Active cores") || text.contains("GPU timeout") {
            break;
        }
    }
    let num_after = |ln: &str, key: &str| -> Option<u32> {
        let i = ln.find(key)? + key.len();
        ln[i..].trim_start().split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
    };
    for ln in text.lines().filter(|l| l.contains("gpu")) {
        if info.gtype.is_none() {
            if let Some(i) = ln.find("Type:") {
                info.gtype = ln[i + 5..].split_whitespace().next().map(String::from);
            }
        }
        if info.clusters.is_none() {
            info.clusters = num_after(ln, "Clusters:");
        }
        if info.cores.is_none() {
            if let Some(c) = num_after(ln, "Active cores:") {
                info.cores = Some(c);
                info.src = info.src.or(Some("log"));
            }
        }
        if ln.contains("GPU timeout") {
            info.timeouts += 1;
        }
    }
    info
}

pub fn soc_name(chip_id: Option<u32>) -> Option<String> {
    let names = |k: &str| -> Option<&'static str> {
        Some(match k {
            "t8103" => "M1",
            "t6000" => "M1 Pro",
            "t6001" => "M1 Max",
            "t6002" => "M1 Ultra",
            "t8112" => "M2",
            "t6020" => "M2 Pro",
            "t6021" => "M2 Max",
            "t6022" => "M2 Ultra",
            _ => return None,
        })
    };
    if let Ok(b) = fs::read("/proc/device-tree/compatible") {
        for c in b.split(|&x| x == 0) {
            let c = String::from_utf8_lossy(c);
            if let Some(n) = names(c.trim_start_matches("apple,")) {
                return Some(n.into());
            }
        }
    }
    // chip_id is BCD, e.g. 0x6001 -> t6001
    chip_id.filter(|c| *c != 0).map(|c| names(&format!("t{c:04x}")).map(String::from).unwrap_or(format!("T{c:04x}")))
}

/// Counts new "GPU timeout" kernel messages while the tool runs: from
/// /dev/kmsg when readable (root), else by following the journal.
pub struct KmsgWatch {
    f: Option<fs::File>,
    child: Option<std::process::Child>,
    partial: Vec<u8>,
    pub new: u64,
}

fn nonblocking(f: &fs::File) {
    unsafe {
        let fl = libc::fcntl(f.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(f.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
}

impl KmsgWatch {
    pub fn new() -> Self {
        let kmsg = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/kmsg")
            .ok()
            .and_then(|mut f| f.seek(SeekFrom::End(0)).ok().map(|_| f));
        if kmsg.is_some() {
            return KmsgWatch { f: kmsg, child: None, partial: Vec::new(), new: 0 };
        }
        let child = std::process::Command::new("journalctl")
            .args(["-k", "-f", "-n", "0", "-o", "cat", "--no-pager"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok();
        let f = child.as_ref().and_then(|c| {
            let fd = c.stdout.as_ref()?.as_raw_fd();
            let f = unsafe { <fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(libc::dup(fd)) };
            nonblocking(&f);
            Some(f)
        });
        KmsgWatch { f, child, partial: Vec::new(), new: 0 }
    }

    pub fn poll(&mut self) {
        let Some(f) = self.f.as_mut() else { return };
        let mut buf = [0u8; 8192];
        for _ in 0..10000 {
            match f.read(&mut buf) {
                Ok(0) => return,
                Ok(n) if self.child.is_some() => {
                    // journal: a byte stream of lines
                    self.partial.extend_from_slice(&buf[..n]);
                    let Some(cut) = self.partial.iter().rposition(|&b| b == b'\n') else { continue };
                    let rest = self.partial.split_off(cut + 1);
                    let done = std::mem::replace(&mut self.partial, rest);
                    self.new += done.split(|&b| b == b'\n').filter(|l| l.windows(11).any(|w| w == b"GPU timeout")).count() as u64;
                }
                Ok(n) => {
                    // kmsg: one record per read
                    if buf[..n].windows(11).any(|w| w == b"GPU timeout") {
                        self.new += 1;
                    }
                }
                Err(e) if e.raw_os_error() == Some(libc::EPIPE) => continue, // overrun
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    }
}

impl Drop for KmsgWatch {
    fn drop(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

// ----------------------------------------------------------------- sensors --
/// The SMC's main sensor device (macsmc_hwmon; macsmc_battery/_ac have no
/// power or fan readings).
pub fn find_hwmon() -> Option<String> {
    let named: Vec<(String, String)> = glob_sorted("/sys/class/hwmon", "hwmon", "")
        .into_iter()
        .filter_map(|d| Some((fs::read_to_string(format!("{d}/name")).ok()?.trim().to_string(), d)))
        .collect();
    named
        .iter()
        .find(|(n, _)| n == "macsmc_hwmon")
        .or_else(|| named.iter().find(|(n, _)| n.starts_with("macsmc")))
        .map(|(_, d)| d.clone())
}

pub struct Sensor {
    pub label: String,
    pub value: f64,
    pub unit: &'static str,
}

pub fn read_sensors(hw: Option<&str>) -> Vec<Sensor> {
    let mut out = Vec::new();
    let Some(hw) = hw else { return out };
    let label = |p: &str| {
        fs::read_to_string(p.replace("_input", "_label"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| p.rsplit('/').next().unwrap_or("").split('_').next().unwrap_or("").into())
    };
    let read = |p: &str| fs::read_to_string(p).ok().and_then(|s| s.trim().parse::<f64>().ok());
    for p in glob_sorted(hw, "power", "_input").iter().take(4) {
        if let Some(v) = read(p).map(|v| v / 1e6).filter(|v| *v > 0.0) {
            out.push(Sensor { label: label(p), value: v, unit: "W" });
        }
    }
    for p in glob_sorted(hw, "fan", "_input").iter().take(2) {
        if let Some(v) = read(p) {
            out.push(Sensor { label: label(p), value: v, unit: "rpm" });
        }
    }
    out
}

/// (MemTotal, MemAvailable) in bytes.
pub fn meminfo() -> (f64, f64) {
    let text = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let get = |k: &str| {
        text.lines()
            .find(|l| l.starts_with(k) && l[k.len()..].starts_with(':'))
            .and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok())
            .map_or(0.0, |v| v * 1024.0)
    };
    (get("MemTotal"), get("MemAvailable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ioctl_number_matches_header() {
        assert_eq!(std::mem::size_of::<ParamsGlobal>(), 592);
        assert_eq!(DRM_IOCTL_ASAHI_GET_PARAMS, 0x4018_6440);
    }
}
