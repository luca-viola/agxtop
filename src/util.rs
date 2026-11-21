use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;

/// CLOCK_MONOTONIC seconds, same clock as tracefs "mono".
pub fn now() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
}

thread_local! {
    static TGID: RefCell<HashMap<i32, i32>> = RefCell::new(HashMap::new());
    static USERS: RefCell<HashMap<u32, String>> = RefCell::new(HashMap::new());
}

/// Thread-group (process) id of a thread id.
pub fn tgid(tid: i32) -> i32 {
    TGID.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(t) = c.get(&tid) {
            return *t;
        }
        let t = fs::read_to_string(format!("/proc/{tid}/status"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("Tgid:"))
                    .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
            })
            .unwrap_or(tid);
        if c.len() > 4096 {
            c.clear();
        }
        c.insert(tid, t);
        t
    })
}

pub fn username(uid: u32) -> String {
    USERS.with(|c| {
        c.borrow_mut()
            .entry(uid)
            .or_insert_with(|| {
                let mut buf = vec![0 as libc::c_char; 4096];
                let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
                let mut res: *mut libc::passwd = std::ptr::null_mut();
                let rc = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut res) };
                if rc == 0 && !res.is_null() {
                    unsafe { std::ffi::CStr::from_ptr(pw.pw_name) }.to_string_lossy().into_owned()
                } else {
                    uid.to_string()
                }
            })
            .clone()
    })
}

pub fn dev_major(d: u64) -> u64 {
    ((d >> 8) & 0xfff) | ((d >> 32) & !0xfff)
}

pub fn dev_minor(d: u64) -> u64 {
    (d & 0xff) | ((d >> 12) & !0xff)
}

fn scaled(b: f64, units: &[&str]) -> String {
    let mut b = b;
    for (i, u) in units.iter().enumerate() {
        if b < 1024.0 || i == units.len() - 1 {
            return if i < 2 { format!("{b:.0}{u}") } else { format!("{b:.1}{u}") };
        }
        b /= 1024.0;
    }
    unreachable!()
}

/// Compact size: 21.0G, 512M, 48K.
pub fn short(b: Option<f64>) -> String {
    b.map_or("n/a".into(), |b| scaled(b, &["B", "K", "M", "G", "T"]))
}

pub fn human(b: Option<f64>) -> String {
    b.map_or("n/a".into(), |b| scaled(b, &["B", "KiB", "MiB", "GiB", "TiB"]))
}

/// 12345.6 -> "12,346"
pub fn thousands(v: f64) -> String {
    let s = format!("{:.0}", v.max(0.0));
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Like Python's {:g} for the refresh interval: 0.25, 0.5, 1, 2.
pub fn fmt_g(v: f64) -> String {
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

pub fn clock_hms() -> String {
    let t = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formats() {
        assert_eq!(short(Some(21.0 * 1024f64.powi(3))), "21.0G");
        assert_eq!(short(Some(48.0 * 1024.0)), "48K");
        assert_eq!(human(Some(512.0 * 1024f64.powi(2))), "512.0MiB");
        assert_eq!(human(None), "n/a");
        assert_eq!(thousands(1234567.4), "1,234,567");
        assert_eq!(thousands(12.0), "12");
        assert_eq!(fmt_g(0.25), "0.25");
        assert_eq!(fmt_g(2.0), "2");
    }
}
