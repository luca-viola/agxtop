#!/usr/bin/env python3
"""agxtop — nvtop-style GPU monitor for Asahi Linux (Apple Silicon).

Data sources (no driver performance counters exist yet):
  * GPU busy %   : kernel gpu_scheduler trace events. A job counts as busy from
                   drm_sched_job_run until drm_sched_job_done. On Apple GPUs a
                   job may still wait in the firmware queue, so this can read
                   higher than real shader load.
  * Per-process  : drm_sched_job_queue is logged in the submitting process,
                   so each job's fence is tied to a PID. When several
                   processes have jobs on the GPU at once, the time is split
                   by their share of in-flight jobs, so the column sums to
                   the total busy %.
  * GPU memory   : drm-* memory keys in /proc/<pid>/fdinfo if the driver
                   publishes them; otherwise the size of GPU buffers mapped
                   into each process (from /proc/<pid>/smaps, shown with ~).
                   Apple GPUs share system RAM (unified memory).
  * Power / fans : macsmc hwmon sensors (whole machine, not GPU only).

Keys: q quit | g/m/p/n sort by GPU%/memory/PID/name | r reverse | +/- interval
Run:  sudo python3 agxtop [--all-drivers]
"""
import collections, curses, errno, glob, locale, os, pwd, re, select, signal, subprocess, sys, time

TRACEFS = os.environ.get("ASAHI_GPUTOP_TRACEFS", "/sys/kernel/tracing")
EVENTS = ["drm_sched_job_queue", "drm_sched_job_run", "drm_sched_job_done"]
STALE = 10.0
HIST = 400

LINE = re.compile(r"^\s*(.*?)-(\d+)\s+\[\d+\].*?\s(\d+\.\d+):\s+(drm_sched_job_queue|drm_sched_job_run|drm_sched_job_done):(.*)")
FENCE = re.compile(r"fence=([0-9a-fx:]+)")
CLIENT = re.compile(r"client_id:\s*(\d+)")
RING = re.compile(r"ring=([^,\s]+)")
MEMKEY = re.compile(r"^drm-(total|resident|memory|shared|active|purgeable)-([\w.-]+):\s+(\d+)\s*(\w*)")
UNITS = {"": 1, "B": 1, "KiB": 1024, "MiB": 1024**2, "GiB": 1024**3}
BLOCKS = " ▁▂▃▄▅▆▇█"


# ----------------------------------------------------------------- tracing --
class Tracer:
    def __init__(self):
        self.saved = {}
        self.fd = None
        self.buf = b""
        self.inflight = {}                       # fence -> (start, key)
        self.owner = {}                          # fence -> (queued ts, tgid)
        self.tgid_cache = {}
        self.ccount = collections.Counter()      # client -> jobs in flight
        self.busy = 0.0
        self.cbusy = collections.Counter()
        self.jobs = 0
        self.rings = collections.Counter()
        self.attributed = False
        self.last = now()

    def _p(self, p): return os.path.join(TRACEFS, p)

    def _rd(self, p):
        with open(self._p(p)) as f: return f.read()

    def _wr(self, p, v):
        with open(self._p(p), "w") as f: f.write(v)

    def start(self):
        m = re.search(r"\[(\w+)\]", self._rd("trace_clock"))
        self.saved["trace_clock"] = m.group(1) if m else "local"
        self.saved["tracing_on"] = self._rd("tracing_on").strip()
        for e in EVENTS:
            k = f"events/gpu_scheduler/{e}/enable"
            self.saved[k] = self._rd(k).strip()
        self._wr("trace_clock", "mono")
        for e in EVENTS:
            self._wr(f"events/gpu_scheduler/{e}/enable", "1")
        self._wr("tracing_on", "1")
        self.fd = os.open(self._p("trace_pipe"), os.O_RDONLY | os.O_NONBLOCK)
        self.last = now()

    def stop(self):
        if self.fd is not None:
            try: os.close(self.fd)
            except OSError: pass
        for k, v in self.saved.items():
            try: self._wr(k, v)
            except OSError: pass

    def _advance(self, ts):
        if ts > self.last:
            dt = ts - self.last
            if self.inflight:
                self.busy += dt
                # Split overlapping time between clients by their share of
                # in-flight jobs, so per-process GPU% adds up to the total.
                n_all = sum(n for n in self.ccount.values() if n > 0)
                if n_all:
                    for c, n in self.ccount.items():
                        if n > 0: self.cbusy[c] += dt * n / n_all
            self.last = ts

    def poll(self, timeout):
        r, _, _ = select.select([self.fd], [], [], timeout)
        if not r: return
        try:
            self.buf += os.read(self.fd, 1 << 16)
        except (BlockingIOError, InterruptedError):
            return
        *lines, self.buf = self.buf.split(b"\n")
        for raw in lines:
            m = LINE.search(raw.decode(errors="replace"))
            if not m: continue
            tid, ts, ev, rest = int(m.group(2)), float(m.group(3)), m.group(4), m.group(5)
            fm = FENCE.search(rest)
            if not fm: continue
            fence = fm.group(1)
            if ev == "drm_sched_job_queue":
                if tid > 0:
                    self.owner[fence] = (ts, self.tgid(tid))
                continue
            self._advance(ts)
            if ev == "drm_sched_job_run":
                if fence in self.inflight:      # duplicate run; ignore
                    continue
                own = self.owner.pop(fence, None)
                if own:
                    client = ("pid", own[1]); self.attributed = True
                else:
                    cm = CLIENT.search(rest)
                    client = ("cid", int(cm.group(1))) if cm else None
                self.inflight[fence] = (ts, client)
                self.ccount[client] += 1
                self.jobs += 1
                rm = RING.search(rest)
                if rm: self.rings[rm.group(1)] += 1
            else:
                job = self.inflight.pop(fence, None)
                if job:
                    self.ccount[job[1]] -= 1
                    if self.ccount[job[1]] <= 0: del self.ccount[job[1]]

    def tgid(self, tid):
        t = self.tgid_cache.get(tid)
        if t is None:
            t = tid
            try:
                with open(f"/proc/{tid}/status") as f:
                    for ln in f:
                        if ln.startswith("Tgid:"):
                            t = int(ln.split()[1]); break
            except OSError:
                pass
            if len(self.tgid_cache) > 4096: self.tgid_cache.clear()
            self.tgid_cache[tid] = t
        return t

    def sample(self):
        """Close the current window; return (busy_s, per_client_busy_s, jobs, rings)."""
        t = now()
        self._advance(t)
        for f, (t0, c) in list(self.inflight.items()):
            if t - t0 > STALE:
                del self.inflight[f]
                self.ccount[c] -= 1
                if self.ccount[c] <= 0: del self.ccount[c]
        for f, (t0, _) in list(self.owner.items()):
            if t - t0 > STALE: del self.owner[f]
        out = (self.busy, dict(self.cbusy), self.jobs, dict(self.rings))
        self.busy, self.cbusy, self.jobs, self.rings = 0.0, collections.Counter(), 0, collections.Counter()
        return out


def now():
    return time.clock_gettime(time.CLOCK_MONOTONIC)


# --------------------------------------------------------------- processes --
def gpu_driver_names():
    names = set()
    for d in glob.glob("/sys/class/drm/renderD*/device/driver"):
        try: names.add(os.path.basename(os.readlink(d)))
        except OSError: pass
    return names


_rdev_driver = {}
def rdev_driver(rdev):
    """Kernel driver behind a /dev/dri character device number."""
    if rdev not in _rdev_driver:
        try:
            link = os.readlink(f"/sys/dev/char/{os.major(rdev)}:{os.minor(rdev)}/device/driver")
            _rdev_driver[rdev] = os.path.basename(link)
        except OSError:
            _rdev_driver[rdev] = None
    return _rdev_driver[rdev]


HDR = re.compile(r"^[0-9a-f]+-[0-9a-f]+\s")
def mapped_gpu_bytes(pid, paths):
    """Size of GPU buffers mapped into the process: device-node mappings,
    shmem-backed GEM objects ("/drm mm object") and dma-bufs."""
    # GPU buffers are mapped as raw page ranges, which the kernel usually
    # leaves out of Rss, so fall back to the mapping size when Rss is zero.
    rss = size = 0
    cur = False
    try:
        with open(f"/proc/{pid}/smaps") as f:
            for ln in f:
                if HDR.match(ln):
                    parts = ln.split(None, 5)
                    name = parts[5].strip() if len(parts) == 6 else ""
                    cur = (name in paths or name.startswith("/drm mm object")
                           or "dmabuf" in name)
                elif cur:
                    if ln.startswith("Rss:"):
                        rss += int(ln.split()[1]) * 1024
                    elif ln.startswith("Size:"):
                        size += int(ln.split()[1]) * 1024
    except (OSError, ValueError):
        return None
    return rss if rss else size


def scan_processes(drivers, extra_pids=()):
    """Return {pid: {name, user, clients:set, mem, mem_est}} for GPU users."""
    procs = {}
    seen_clients = set()
    for piddir in glob.glob("/proc/[0-9]*"):
        pid = int(piddir[6:])
        try:
            fds = os.listdir(piddir + "/fd")
        except OSError:
            continue
        info = None
        for fd in fds:
            try:
                tgt = os.readlink(f"{piddir}/fd/{fd}")
            except OSError:
                continue
            if not tgt.startswith("/dev/dri/"):
                continue
            try:
                rdev = os.stat(f"{piddir}/fd/{fd}").st_rdev
            except OSError:
                continue
            try:
                with open(f"{piddir}/fdinfo/{fd}") as f:
                    lines = f.read().splitlines()
            except OSError:
                lines = []
            kv = {}
            mem = {}
            for ln in lines:
                if ":" in ln:
                    k, v = ln.split(":", 1)
                    kv[k.strip()] = v.strip()
                mm = MEMKEY.match(ln)
                if mm:
                    mem[(mm.group(1), mm.group(2))] = int(mm.group(3)) * UNITS.get(mm.group(4), 1)
            drv = kv.get("drm-driver") or rdev_driver(rdev)
            if drv is None or (drivers and drv not in drivers):
                continue
            cid = kv.get("drm-client-id")
            key = (drv, cid) if cid is not None else (pid, rdev)
            if key in seen_clients:
                continue
            seen_clients.add(key)
            if info is None:
                info = {"clients": set(), "mem": None, "mem_est": False, "driver": drv, "paths": set()}
            info["paths"].add(tgt)
            if cid is not None:
                info["clients"].add(int(cid))
            b = client_mem(mem)
            if b is not None:
                info["mem"] = (info["mem"] or 0) + b
        if info is None:
            if pid not in extra_pids:
                continue
            info = {"clients": set(), "mem": None, "mem_est": False, "driver": "?", "paths": set()}
        if info["mem"] is None and info["paths"]:
            info["mem"] = mapped_gpu_bytes(pid, info["paths"])
            info["mem_est"] = info["mem"] is not None
        try:
            with open(piddir + "/comm") as f: name = f.read().strip()
            with open(piddir + "/cmdline", "rb") as f:
                cmd = f.read().replace(b"\0", b" ").decode(errors="replace").strip()
            uid = os.stat(piddir).st_uid
            try: user = pwd.getpwuid(uid).pw_name
            except KeyError: user = str(uid)
        except OSError:
            continue
        info.update(name=name, cmd=cmd or name, user=user)
        procs[pid] = info
    return procs


def client_mem(mem):
    """Pick the most meaningful memory figure from a client's drm-* keys."""
    for kind in ("resident", "total", "memory"):
        vals = [v for (k, _), v in mem.items() if k == kind]
        if vals: return sum(vals)
    return None


# ------------------------------------------------------- GPU info & health --
def agx_params():
    """Ask the asahi driver for the GPU layout via DRM_IOCTL_ASAHI_GET_PARAMS
    (upstream UAPI, include/uapi/drm/asahi_drm.h). Works on every Asahi
    machine regardless of logs; no root needed. Returns None on failure."""
    import fcntl, struct
    SIZE = 592                                  # sizeof(drm_asahi_params_global)
    # _IOW('d', DRM_COMMAND_BASE + DRM_ASAHI_GET_PARAMS, 24-byte get_params)
    req = (1 << 30) | (24 << 16) | (ord("d") << 8) | 0x40
    for node in sorted(glob.glob("/dev/dri/renderD*")):
        try:
            if os.path.basename(os.readlink(f"/sys/class/drm/{os.path.basename(node)}/device/driver")) != "asahi":
                continue
            buf = bytearray(SIZE)
            ptr = (ctypes_addr(buf))
            arg = bytearray(struct.pack("=IIQQ", 0, 0, ptr, SIZE))
            fd = os.open(node, os.O_RDWR | os.O_CLOEXEC)
            try:
                fcntl.ioctl(fd, req, arg)
            finally:
                os.close(fd)
        except OSError:
            continue
        (gen, variant, rev, chip, dies, nclus, per, fkhz) = struct.unpack_from("=8I", buf, 8)
        masks = struct.unpack_from("=64Q", buf, 40)[:nclus]
        if not (0 < nclus <= 64 and 0 < per <= 64):
            continue                            # layout mismatch (old downstream UAPI)
        active = sum(bin(m & ((1 << per) - 1)).count("1") for m in masks)
        letter = chr(variant) if 0x41 <= variant <= 0x5a else ""
        return {"type": f"G{gen}{letter}", "clusters": nclus, "cores": active or None,
                "total_cores": nclus * per, "dies": dies, "chip_id": chip,
                "max_mhz": fkhz // 1000 or None}
    return None


def ctypes_addr(buf):
    import ctypes
    return ctypes.addressof((ctypes.c_char * len(buf)).from_buffer(buf))


def gpu_info():
    """GPU type / cluster / core counts and this boot's timeout count. Layout
    comes from the driver ioctl when possible, else from the driver's boot
    messages (journal first, kernel ring buffer as fallback)."""
    info = {"type": None, "clusters": None, "cores": None, "timeouts": 0,
            "total_cores": None, "max_mhz": None, "chip_id": None, "src": None}
    p = agx_params()
    if p:
        info.update(p); info["src"] = "ioctl"
    text = ""
    for cmd in (["journalctl", "-k", "-b", "-o", "cat", "--no-pager"], ["dmesg"]):
        try:
            text = subprocess.run(cmd, capture_output=True, text=True, timeout=10).stdout
        except (OSError, subprocess.SubprocessError):
            continue
        if "Active cores" in text or "GPU timeout" in text:
            break
    for ln in text.splitlines():
        if "gpu" not in ln: continue
        m = re.search(r"Type:\s*(\S+)", ln)
        if m and not info["type"]: info["type"] = m.group(1)
        m = re.search(r"Clusters:\s*(\d+)", ln)
        if m and not info["clusters"]: info["clusters"] = int(m.group(1))
        m = re.search(r"Active cores:\s*(\d+)", ln)
        if m and not info["cores"]:
            info["cores"] = int(m.group(1)); info["src"] = info["src"] or "log"
        if "GPU timeout" in ln: info["timeouts"] += 1
    return info


def soc_name(chip_id=None):
    names = {"t8103": "M1", "t6000": "M1 Pro", "t6001": "M1 Max", "t6002": "M1 Ultra",
             "t8112": "M2", "t6020": "M2 Pro", "t6021": "M2 Max", "t6022": "M2 Ultra"}
    try:
        with open("/proc/device-tree/compatible", "rb") as f:
            for c in f.read().split(b"\0"):
                n = names.get(c.decode(errors="replace").removeprefix("apple,"))
                if n: return n
    except OSError:
        pass
    if chip_id:                                 # BCD, e.g. 0x6001 -> t6001
        return names.get(f"t{chip_id:04x}", f"T{chip_id:04x}")
    return None


class KmsgWatch:
    """Counts new 'GPU timeout' kernel messages while the tool runs."""
    def __init__(self):
        self.fd = None
        self.new = 0
        try:
            self.fd = os.open("/dev/kmsg", os.O_RDONLY | os.O_NONBLOCK)
            os.lseek(self.fd, 0, os.SEEK_END)
        except OSError:
            self.fd = None

    def poll(self):
        if self.fd is None: return
        for _ in range(10000):
            try:
                rec = os.read(self.fd, 8192)
            except BlockingIOError:
                return
            except OSError as e:
                if e.errno == errno.EPIPE: continue   # buffer overrun, skip
                return
            if not rec: return
            if b"GPU timeout" in rec: self.new += 1


# ----------------------------------------------------------------- sensors --
def find_hwmon():
    for d in glob.glob("/sys/class/hwmon/hwmon*"):
        try:
            with open(d + "/name") as f: name = f.read().strip()
        except OSError:
            continue
        if name.startswith("macsmc"):
            return d
    return None


def read_sensors(hw):
    out = []
    if not hw: return out
    def label(path):
        try:
            with open(path.replace("_input", "_label")) as f: return f.read().strip()
        except OSError:
            return os.path.basename(path).split("_")[0]
    for p in sorted(glob.glob(hw + "/power*_input"))[:4]:
        try:
            with open(p) as f: v = int(f.read()) / 1e6
            if v > 0: out.append((label(p), v, "W"))
        except (OSError, ValueError): pass
    for p in sorted(glob.glob(hw + "/fan*_input"))[:2]:
        try:
            with open(p) as f: out.append((label(p), int(f.read()), "rpm"))
        except (OSError, ValueError): pass
    return out


def meminfo():
    d = {}
    try:
        with open("/proc/meminfo") as f:
            for ln in f:
                k, v = ln.split(":", 1)
                d[k] = int(v.split()[0]) * 1024
    except OSError:
        pass
    return d.get("MemTotal", 0), d.get("MemAvailable", 0)


def short(b):
    """Compact size: 21.0G, 512M, 48K."""
    if b is None: return "n/a"
    for u in ("B", "K", "M", "G", "T"):
        if b < 1024 or u == "T":
            return f"{b:.0f}{u}" if u in ("B", "K") else f"{b:.1f}{u}"
        b /= 1024


def human(b):
    if b is None: return "n/a"
    for u in ("B", "KiB", "MiB", "GiB", "TiB"):
        if b < 1024 or u == "TiB":
            return f"{b:.0f}{u}" if u in ("B", "KiB") else f"{b:.1f}{u}"
        b /= 1024


# ---------------------------------------------------------------------- UI --
class UI:
    def __init__(self, scr, tracer, drivers):
        self.s = scr
        self.t = tracer
        self.drivers = drivers
        self.interval = 1.0
        self.sort = "g"
        self.reverse = True
        self.gpu_hist = collections.deque(maxlen=HIST)
        self.mem_hist = collections.deque(maxlen=HIST)
        self.jobs_hist = collections.deque(maxlen=HIST)
        self.used_hist = collections.deque(maxlen=HIST)
        self.mem_est = False
        self.mem_zoom = False
        self.show_mem_graph = False     # which graph when only one fits
        self.sel = 0                    # selected row in the process table
        self.sel_pid = None
        self.top = 0                    # first visible table row
        self.page = 1
        self.help = False
        # Turn on mouse reporting so the wheel arrives as mouse events (which
        # are ignored) instead of being translated into arrow keys.
        try:
            curses.mousemask(curses.ALL_MOUSE_EVENTS | curses.REPORT_MOUSE_POSITION)
            curses.mouseinterval(0)
        except curses.error:
            pass
        self.hw = find_hwmon()
        self.ginfo = gpu_info()
        self.soc = soc_name(self.ginfo["chip_id"])
        self.kmsg = KmsgWatch()
        self.procs = {}
        self.busy_pct = 0.0
        self.jobs_s = 0.0
        self.rings = {}
        self.cpct = {}
        self.sensors = []
        self.mem_total_clients = None
        self.ram_total, self.ram_avail = meminfo()
        curses.curs_set(0)
        scr.nodelay(True)
        scr.timeout(0)
        self.color = curses.has_colors()
        if self.color:
            curses.start_color()
            try: curses.use_default_colors(); bg = -1
            except curses.error: bg = curses.COLOR_BLACK
            curses.init_pair(1, curses.COLOR_GREEN, bg)
            curses.init_pair(2, curses.COLOR_YELLOW, bg)
            curses.init_pair(3, curses.COLOR_RED, bg)
            curses.init_pair(4, curses.COLOR_CYAN, bg)
            curses.init_pair(5, curses.COLOR_BLACK, curses.COLOR_CYAN)
            curses.init_pair(6, curses.COLOR_MAGENTA, bg)
            curses.init_pair(7, curses.COLOR_BLUE, bg)
            curses.init_pair(8, curses.COLOR_MAGENTA, curses.COLOR_BLUE)   # GPU sliver on top of apps
            # green -> yellow -> orange -> red gradient
            if curses.COLORS >= 256:
                ramp = [34, 40, 76, 112, 148, 184, 220, 214, 208, 202, 196]
            else:
                ramp = [curses.COLOR_GREEN] * 2 + [curses.COLOR_YELLOW] * 2 + [curses.COLOR_RED]
            self.grad_pairs = []
            for i, c in enumerate(ramp):
                curses.init_pair(10 + i, c, bg)
                self.grad_pairs.append(10 + i)

    def cp(self, n, extra=0):
        return (curses.color_pair(n) if self.color else 0) | extra

    def level(self, pct):
        return 1 if pct < 50 else 2 if pct < 80 else 3

    def grad(self, frac, extra=0):
        """Colour for a position 0..1 along the green->red gradient."""
        if not self.color: return extra
        n = len(self.grad_pairs)
        i = min(n - 1, max(0, int(frac * n)))
        return curses.color_pair(self.grad_pairs[i]) | extra

    def put(self, y, x, txt, attr=0):
        h, w = self.s.getmaxyx()
        if y < 0 or y >= h or x >= w: return
        txt = txt[: max(0, w - x - (1 if y == h - 1 else 0))]
        try: self.s.addstr(y, x, txt, attr)
        except curses.error: pass

    # -- data --
    def refresh_data(self, span):
        busy, cbusy, jobs, rings = self.t.sample()
        self.busy_pct = min(100.0, 100.0 * busy / span) if span > 0 else 0.0
        self.jobs_s = jobs / span if span > 0 else 0.0
        self.rings = rings
        self.cpct = {c: min(100.0, 100.0 * b / span) for c, b in cbusy.items() if c}
        busy_pids = {c[1] for c in self.cpct if c[0] == "pid"}
        self.procs = scan_processes(self.drivers, busy_pids)
        attributed = self.t.attributed or any(c[0] == "cid" for c in self.cpct)
        for pid, p in self.procs.items():
            if attributed:
                v = self.cpct.get(("pid", pid), 0.0)
                v += sum(self.cpct.get(("cid", c), 0.0) for c in p["clients"])
                p["gpu"] = min(100.0, v)
            else:
                p["gpu"] = None
        mems = [p["mem"] for p in self.procs.values() if p["mem"] is not None]
        self.mem_total_clients = sum(mems) if mems else None
        self.mem_est = any(p["mem_est"] for p in self.procs.values())
        self.jobs_hist.append(self.jobs_s)
        self.sensors = read_sensors(self.hw)
        self.kmsg.poll()
        self.ram_total, self.ram_avail = meminfo()
        self.gpu_hist.append(self.busy_pct)
        self.mem_hist.append(self.mem_total_clients or 0)
        self.used_hist.append(self.ram_total - self.ram_avail)

    # -- drawing --
    def bar(self, y, x, w, pct, label, color, gradient=False):
        inner = max(1, w - 2)
        fill = int(round(inner * pct / 100))
        self.put(y, x, "[", self.cp(4))
        if gradient:
            for i in range(fill):
                self.put(y, x + 1 + i, "|", self.grad((i + 0.5) / inner, curses.A_BOLD))
        else:
            self.put(y, x + 1, "|" * fill, self.cp(color, curses.A_BOLD))
        self.put(y, x + 1 + fill, " " * (inner - fill))
        txt = label[:inner]
        self.put(y, x + 1 + inner - len(txt), txt, curses.A_BOLD)
        self.put(y, x + 1 + inner, "]", self.cp(4))

    def stacked_bar(self, y, x, w, total, segs, label):
        """segs: [(bytes, colour pair)] drawn left to right on a 0..total scale."""
        inner = max(1, w - 2)
        self.put(y, x, "[", self.cp(4))
        pos = 0
        acc = 0.0
        for val, col in segs:
            acc += val
            end = min(inner, int(round(inner * acc / total))) if total else 0
            if val > 0 and end <= pos:
                end = min(inner, pos + 1)
            if end > pos:
                self.put(y, x + 1 + pos, "|" * (end - pos), self.cp(col, curses.A_BOLD))
                pos = end
        self.put(y, x + 1 + pos, " " * (inner - pos))
        txt = label[:inner]
        self.put(y, x + 1 + inner - len(txt), txt, curses.A_BOLD)
        self.put(y, x + 1 + inner, "]", self.cp(4))

    def frame(self, y, x, h, w, title, tattr, right):
        self.put(y, x, "┌" + "─" * (w - 2) + "┐", self.cp(4))
        for r in range(1, h - 1):
            self.put(y + r, x, "│", self.cp(4)); self.put(y + r, x + w - 1, "│", self.cp(4))
        self.put(y + h - 1, x, "└" + "─" * (w - 2) + "┘", self.cp(4))
        self.put(y, x + 2, f" {title} ", tattr)
        self.put(y, x + w - len(right) - 4, f" {right} ", self.cp(4))

    def mem_graph(self, y, x, h, w):
        """GPU memory stacked under other used RAM, scaled to total RAM."""
        if h < 3 or w < 20: return
        total = self.ram_total or 1
        if self.mem_zoom and self.used_hist:
            total = max(self.used_hist) * 1.15 or total
        used_now = self.used_hist[-1] if self.used_hist else 0
        gpu_now = min(self.mem_total_clients or 0, used_now)
        est = "~" if self.mem_est else ""
        right = ("zoom " if self.mem_zoom else "") + short(total)
        self.frame(y, x, h, w, "", 0, right)
        g_txt = f"GPU {est}{human(gpu_now)} "
        a_txt = f"+ apps {human(max(0, used_now - gpu_now))} "
        if len(g_txt) + len(a_txt) + len(right) + 8 > w:
            g_txt = f"GPU {est}{short(gpu_now)} "
            a_txt = f"+ {short(max(0, used_now - gpu_now))} "
        tx = x + 3
        self.put(y, tx, g_txt, self.cp(6, curses.A_BOLD)); tx += len(g_txt)
        self.put(y, tx, a_txt, self.cp(7, curses.A_BOLD))
        gh, gw = h - 2, w - 2
        gpus = list(self.mem_hist)[-gw:]
        useds = list(self.used_hist)[-gw:]
        off = gw - len(useds)
        for i, (g, u) in enumerate(zip(gpus, useds)):
            g = min(g, u)
            te = int(round(min(1.0, u / total) * gh * 8))
            ge = int(round(min(1.0, g / total) * gh * 8))
            if g > 0 and ge == 0: ge = 1            # keep a non-zero GPU share visible
            te = max(te, ge)
            col = x + 1 + off + i
            for r in range(gh):
                lo = r * 8
                if te <= lo: break
                if ge >= lo + 8:                    # cell fully GPU
                    ch, c = BLOCKS[8], 6
                elif ge > lo and te >= lo + 8:      # GPU sliver under apps in this cell
                    ch, c = BLOCKS[ge - lo], 8
                elif ge > lo:                       # both end inside this cell
                    ch, c = BLOCKS[min(8, te - lo)], 6
                else:                               # apps only
                    ch, c = BLOCKS[min(8, te - lo)], 7
                self.put(y + h - 2 - r, col, ch, self.cp(c))

    def graph(self, y, x, h, w, values, vmax, title, color, fmt):
        if h < 3 or w < 12: return
        # frame
        self.put(y, x, "┌" + "─" * (w - 2) + "┐", self.cp(4))
        for r in range(1, h - 1):
            self.put(y + r, x, "│", self.cp(4)); self.put(y + r, x + w - 1, "│", self.cp(4))
        self.put(y + h - 1, x, "└" + "─" * (w - 2) + "┘", self.cp(4))
        cur = values[-1] if values else 0
        tattr = self.grad(cur / (vmax or 1), curses.A_BOLD) if color == 1 else self.cp(color, curses.A_BOLD)
        self.put(y, x + 2, f" {title} {fmt(cur)} ", tattr)
        self.put(y, x + w - len(fmt(vmax)) - 4, f" {fmt(vmax)} ", self.cp(4))
        gh, gw = h - 2, w - 2
        vals = list(values)[-gw:]
        off = gw - len(vals)
        vmax = vmax or 1
        for i, v in enumerate(vals):
            eighths = int(round(max(0.0, min(1.0, v / vmax)) * gh * 8))
            col = x + 1 + off + i
            for r in range(gh):
                fill = max(0, min(8, eighths - r * 8))
                if fill:
                    attr = self.grad((r + 0.5) / gh) if color == 1 else self.cp(color)
                    self.put(y + h - 2 - r, col, BLOCKS[fill], attr)

    def draw(self):
        s = self.s
        s.erase()
        h, w = s.getmaxyx()
        if h < 12 or w < 50:
            self.put(0, 0, "Terminal too small for agxtop"); s.refresh(); return
        y = 0
        self.put(y, 0, f" agxtop ", self.cp(5, curses.A_BOLD))
        gi = self.ginfo
        parts = []
        if self.soc: parts.append(f"Apple {self.soc}")
        if gi["type"]: parts.append(gi["type"])
        if gi["cores"]:
            tot = f"/{gi['total_cores']}" if gi["total_cores"] and gi["total_cores"] != gi["cores"] else ""
            parts.append(f"{gi['cores']}{tot} GPU cores" + (f" / {gi['clusters']} clusters" if gi["clusters"] else ""))
        if gi["max_mhz"]: parts.append(f"{gi['max_mhz']} MHz")
        head = " · ".join(parts) or "driver: " + (",".join(sorted(self.drivers)) or "all")
        if w < 120:
            head = " · ".join(p for p in [self.soc, f"{gi['cores']} cores" if gi["cores"] else None] if p) or head
        x = 15
        self.put(y, x, " " + head, curses.A_BOLD); x += len(head) + 4
        rest = (f"jobs/s: {self.jobs_s:,.0f}   {self.interval:g}s   {time.strftime('%H:%M:%S')}   " if w >= 120
                else f"{self.jobs_s:,.0f} jobs/s  ")
        self.put(y, x, rest); x += len(rest)
        tot = gi["timeouts"] + self.kmsg.new
        to = (f"GPU timeouts: {tot} this boot" if w >= 120 else f"timeouts: {tot}") + \
             (f" (+{self.kmsg.new} now!)" if self.kmsg.new else "")
        self.put(y, x, to, self.cp(3, curses.A_BOLD) if self.kmsg.new else self.cp(2 if tot else 1))
        y += 1
        half = (w - 3) // 2
        self.put(y, 1, "GPU", self.cp(4, curses.A_BOLD))
        self.bar(y, 5, half - 5, self.busy_pct, f"{self.busy_pct:.0f}%", 1, gradient=True)
        used = self.ram_total - self.ram_avail
        gpu = min(self.mem_total_clients or 0, used)
        est = "~" if self.mem_est else ""
        self.put(y, half + 2, "MEM", self.cp(4, curses.A_BOLD))
        mlab = f"GPU {est}{human(gpu)} + apps {human(used - gpu)} / {human(self.ram_total)}"
        if len(mlab) > w - half - 12:
            mlab = f"G{est}{short(gpu)}+{short(used - gpu)}/{short(self.ram_total)}"
        self.stacked_bar(y, half + 6, w - half - 7, self.ram_total, [(gpu, 6), (used - gpu, 7)], mlab)
        y += 1
        info_row = False
        if self.mem_total_clients is None:
            self.put(y, 1, "GPU memory: n/a (driver doesn't report)"); info_row = True
        if self.sensors:
            sens = "   ".join(f"{l} {v:.1f}{u}" if u == "W" else f"{l} {v}{u}" for l, v, u in self.sensors)
            self.put(y, half + 2, sens[: w - half - 3], self.cp(2)); info_row = True
        if info_row or h >= 30: y += 1
        if h >= 30:                        # queue line only when there's room
            if self.rings:
                rt = "queues: " + "  ".join(f"{k}:{v}" for k, v in sorted(self.rings.items()))
                self.put(y, 1, rt, self.cp(4))
            y += 1

        # graphs
        table_min = 6 if h >= 30 else 8
        gh = max(5, min(14, (h - y - table_min) // 1))
        gh = min(gh, h - y - table_min)
        if gh >= 5:
            def right_graph(gx, gww):
                if self.mem_total_clients is not None:
                    self.mem_graph(y, gx, gh, gww)
                else:
                    jmax = max(self.jobs_hist) * 1.2 if self.jobs_hist and max(self.jobs_hist) else 1
                    self.graph(y, gx, gh, gww, self.jobs_hist, jmax, "jobs/s", 6, lambda v: f"{v:,.0f}")
            if w < 60:
                if self.show_mem_graph: right_graph(0, w)
                else: self.graph(y, 0, gh, w, self.gpu_hist, 100, "GPU busy", 1, lambda v: f"{v:.0f}%")
            else:
                gw = (w - 1) // 2
                self.graph(y, 0, gh, gw, self.gpu_hist, 100, "GPU busy" if gw >= 40 else "GPU",
                           1, lambda v: f"{v:.0f}%")
                right_graph(gw + 1, w - gw - 1)
            y += gh

        # process table
        hdr = f"{'PID':>7} {'USER':<10} {'GPU%':>6} {'GPU MEM':>10}  COMMAND"
        self.put(y, 0, hdr.ljust(w), self.cp(5, curses.A_BOLD))
        y += 1
        keyf = {
            "g": lambda kv: (kv[1]["gpu"] or 0),
            "m": lambda kv: (kv[1]["mem"] or 0),
            "p": lambda kv: kv[0],
            "n": lambda kv: kv[1]["name"].lower(),
        }[self.sort]
        rows = sorted(self.procs.items(), key=keyf, reverse=self.reverse)
        self.rows = [pid for pid, _ in rows]
        # keep the same process selected across refreshes and re-sorts
        if self.sel_pid in self.rows:
            self.sel = self.rows.index(self.sel_pid)
        self.sel = max(0, min(self.sel, len(rows) - 1))
        self.sel_pid = self.rows[self.sel] if rows else None
        self.page = max(1, h - 1 - y)
        if self.sel < self.top: self.top = self.sel
        if self.sel >= self.top + self.page: self.top = self.sel - self.page + 1
        self.top = max(0, min(self.top, max(0, len(rows) - self.page)))
        if len(rows) > self.page:
            pos = f" {self.top + 1}-{min(len(rows), self.top + self.page)}/{len(rows)} "
            self.put(y - 1, w - len(pos) - 1, pos, self.cp(5, curses.A_BOLD))
        for i, (pid, p) in enumerate(rows[self.top:self.top + self.page]):
            g = "n/a" if p["gpu"] is None else f"{p['gpu']:.1f}"
            mem = ("~" if p["mem_est"] else "") + human(p["mem"])
            line = f"{pid:>7} {p['user'][:10]:<10} {g:>6} {mem:>10}  {p['cmd']}"
            if self.top + i == self.sel:
                self.put(y, 0, line.ljust(w), curses.A_REVERSE | curses.A_BOLD)
            else:
                attr = self.grad((p["gpu"] or 0) / 100) if (p["gpu"] or 0) >= 1 else 0
                self.put(y, 0, line, attr)
            y += 1
        if not rows:
            self.put(y, 2, "No processes have the GPU open.")

        # footer
        sortname = {"g": "GPU%", "m": "memory", "p": "PID", "n": "name"}[self.sort]
        foot = " h help " + (" Tab graph " if w < 60 else "") + f" q quit  g/m/p/n sort ({sortname}{' ↓' if self.reverse else ' ↑'})  r reverse  z zoom mem  +/- interval "
        if not self.t.attributed and self.busy_pct > 0:
            foot += "  [no per-process attribution from trace yet]"
        self.put(h - 1, 0, foot.ljust(w), self.cp(5))
        if self.help:
            self.draw_help()
        s.refresh()

    def draw_help(self):
        lines = [
            ("Keys", None),
            ("  ↑ / ↓            select process", 0),
            ("  PgUp / PgDn      scroll a page", 0),
            ("  Home / End       first / last process", 0),
            ("  g  m  p  n       sort by GPU% / memory / PID / name", 0),
            ("                   (press again to reverse)", 0),
            ("  r                reverse sort order", 0),
            ("  z                zoom memory graph to what's in use", 0),
            ("  Tab              switch graph (narrow terminals)", 0),
            ("  +  -             slower / faster refresh", 0),
            ("  h  ?  F1         this help        q  quit", 0),
            ("", 0),
            ("Reading the screen", None),
            ("  GPU busy   time the GPU had work queued or running;", 0),
            ("             can read higher than real shader load", 0),
            ("  GPU%       each process's share of that time", 0),
            ("  ~ memory   estimate from GPU buffers each process maps", 0),
            ("  MEM bar    magenta = GPU, blue = everything else", 0),
            ("  timeouts   GPU jobs the driver had to kill this boot", 0),
            ("", 0),
            ("              press any key to close", 0),
        ]
        h, w = self.s.getmaxyx()
        bw = min(w - 2, max(len(t) for t, _ in lines) + 4)
        bh = min(h - 2, len(lines) + 2)
        y0, x0 = max(0, (h - bh) // 2), max(0, (w - bw) // 2)
        for r in range(bh):
            self.put(y0 + r, x0, " " * bw, self.cp(5))
        self.put(y0, x0, "┌" + "─" * (bw - 2) + "┐", self.cp(5))
        self.put(y0 + bh - 1, x0, "└" + "─" * (bw - 2) + "┘", self.cp(5))
        for r in range(1, bh - 1):
            self.put(y0 + r, x0, "│", self.cp(5)); self.put(y0 + r, x0 + bw - 1, "│", self.cp(5))
        self.put(y0, x0 + 2, " agxtop help ", self.cp(5, curses.A_BOLD))
        for i, (t, kind) in enumerate(lines[: bh - 2]):
            self.put(y0 + 1 + i, x0 + 2, t[: bw - 4], self.cp(5, curses.A_BOLD if kind is None else 0))

    def move(self, delta):
        n = len(getattr(self, "rows", []))
        if not n: return
        self.sel = max(0, min(n - 1, self.sel + delta))
        self.sel_pid = self.rows[self.sel]

    def handle_key(self, ch):
        if ch == curses.KEY_MOUSE:            # wheel / clicks: ignore
            try: curses.getmouse()
            except curses.error: pass
            return
        if ch == curses.KEY_RESIZE:
            self.s.clear()                    # force a full repaint at the new size
            return
        if self.help:                         # any key closes help
            self.help = False
            return
        c = chr(ch) if 0 <= ch < 256 else None
        if c in ("q", "Q"): return False
        if c in ("h", "?") or ch == curses.KEY_F1: self.help = True
        elif ch == curses.KEY_UP: self.move(-1)
        elif ch == curses.KEY_DOWN: self.move(1)
        elif ch == curses.KEY_PPAGE: self.move(-self.page)
        elif ch == curses.KEY_NPAGE: self.move(self.page)
        elif ch == curses.KEY_HOME: self.move(-10**6)
        elif ch == curses.KEY_END: self.move(10**6)
        elif c and c in "gmpn":
            if self.sort == c: self.reverse = not self.reverse
            else: self.sort, self.reverse = c, c not in "np"
        elif c == "r": self.reverse = not self.reverse
        elif c in ("z", "Z"): self.mem_zoom = not self.mem_zoom
        elif c == "\t": self.show_mem_graph = not self.show_mem_graph
        elif c in ("+", "="): self.interval = min(10.0, self.interval * 2)
        elif c in ("-", "_"): self.interval = max(0.25, self.interval / 2)

    def run(self):
        win = now()
        self.refresh_data(0.001)  # initial process scan
        self.draw()
        while True:
            self.t.poll(0.05)
            ch = self.s.getch()
            if ch != -1:
                if self.handle_key(ch) is False:
                    return
                self.draw()
            t = now()
            if t - win >= self.interval:
                self.refresh_data(t - win)
                win = t
                self.draw()


def main():
    if os.geteuid() != 0 and "ASAHI_GPUTOP_TRACEFS" not in os.environ:
        sys.exit("agxtop needs root to read kernel trace events: sudo python3 agxtop")
    if not os.path.isdir(os.path.join(TRACEFS, "events/gpu_scheduler")):
        sys.exit(f"No gpu_scheduler trace events under {TRACEFS}.")
    drivers = set() if "--all-drivers" in sys.argv else (gpu_driver_names() & {"asahi"})
    locale.setlocale(locale.LC_ALL, "")
    tracer = Tracer()
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    try:
        tracer.start()
        curses.wrapper(lambda scr: UI(scr, tracer, drivers).run())
    except KeyboardInterrupt:
        pass
    finally:
        tracer.stop()


if __name__ == "__main__":
    main()
