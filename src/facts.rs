//! What pictures, recordings and films say about themselves, for the listing's columns:
//! when a photo was taken (or a film made), how big a picture or a film is, how long a
//! recording plays. Rows ask as they are drawn, and every row does when the list sorts
//! or groups by one of these; workers read them and the frame never waits.
//!
//! A picture's header and EXIF come from its first 64 KB, quick enough to read again.
//! A film or a recording needs ffprobe, tens of milliseconds a file, so at most two run
//! at once, one whose folder is no longer shown is killed, and what they found is kept
//! on disk by path, size and time changed.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::fsx::Entry;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Facts {
    pub taken: Option<SystemTime>,
    /// Width and height as shown (a photo stored turned counts turned).
    pub dims: Option<(u32, u32)>,
    /// Seconds.
    pub duration: Option<f64>,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Picture,
    Media,
}

fn kind(ext: &str) -> Option<Kind> {
    match ext {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" => Some(Kind::Picture),
        // Not "ts" or "mts": in a code folder those are TypeScript, a probe each for nothing.
        "mp4" | "m4v" | "mov" | "mkv" | "webm" | "avi" | "3gp" | "mpg" | "mpeg" | "ogv" | "wmv" | "flv" | "m2ts" | "mp3" | "flac" | "ogg" | "oga" | "opus" | "m4a" | "aac" | "wav" | "wma" | "aiff" | "aif" | "ape" | "wv" | "mka" => Some(Kind::Media),
        _ => None,
    }
}

#[derive(Default)]
struct Cache {
    map: HashMap<PathBuf, Facts>,
    pending: HashSet<PathBuf>,
    generation: u64,
}

static CACHE: RwLock<Option<Cache>> = RwLock::new(None);
static QUEUE: Mutex<Vec<(PathBuf, Kind)>> = Mutex::new(Vec::new());
/// The folders shown: work for files outside them is dropped, a probe running for one
/// is killed. Empty: no limit (nothing has said yet).
static FOCUS: RwLock<Vec<PathBuf>> = RwLock::new(Vec::new());

struct Work {
    pics: VecDeque<PathBuf>,
    media: VecDeque<PathBuf>,
    tools: Option<crate::drives::Tools>,
    ctx: Option<egui::Context>,
    started: bool,
}

static WORK: Mutex<Work> = Mutex::new(Work { pics: VecDeque::new(), media: VecDeque::new(), tools: None, ctx: None, started: false });
static WAKE: Condvar = Condvar::new();
/// Cap on what is kept in memory; past it the cache starts again.
const MOST: usize = 200_000;

/// What is known of `e`: Some (maybe empty: not a picture or a recording, or nothing
/// read) once read, None while being read (it is then asked for, for `flush`).
pub fn get(e: &Entry) -> Option<Facts> {
    let Some(k) = (!e.is_dir).then(|| kind(e.ext())).flatten() else { return Some(Facts::default()) };
    {
        let g = CACHE.read().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = g.as_ref() {
            if let Some(f) = c.map.get(&e.path) {
                return Some(*f);
            }
            if c.pending.contains(&e.path) {
                return None;
            }
        }
    }
    let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(Cache::default).pending.insert(e.path.clone());
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).push((e.path.clone(), k));
    None
}

pub fn generation() -> u64 {
    CACHE.read().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(0, |c| c.generation)
}

/// Whether anything landed since the last call (the frame asks once).
pub fn changed() -> bool {
    static SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let g = generation();
    SEEN.swap(g, std::sync::atomic::Ordering::Relaxed) != g
}

/// Forget `p` (it changed on disk): read again when next asked.
pub fn invalidate(p: &Path) {
    let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = g.as_mut() {
        if c.map.remove(p).is_some() {
            c.generation += 1;
        }
    }
}

fn wanted(p: &Path) -> bool {
    let f = FOCUS.read().unwrap_or_else(|e| e.into_inner());
    f.is_empty() || f.iter().any(|d| p.starts_with(d))
}

/// The folders shown now (a flattened one counts with everything under it). Work for
/// other files is dropped, and they are asked for again if shown again.
pub fn focus(dirs: Vec<PathBuf>) {
    {
        let f = FOCUS.read().unwrap_or_else(|e| e.into_inner());
        if *f == dirs {
            return;
        }
    }
    *FOCUS.write().unwrap_or_else(|e| e.into_inner()) = dirs;
    let mut dropped = vec![];
    {
        let mut g = WORK.lock().unwrap_or_else(|e| e.into_inner());
        let w = &mut *g;
        for q in [&mut w.pics, &mut w.media] {
            q.retain(|p| wanted(p) || {
                dropped.push(p.clone());
                false
            });
        }
    }
    unpend(&dropped);
}

fn unpend(paths: &[PathBuf]) {
    if paths.is_empty() {
        return;
    }
    let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = g.as_mut() {
        for p in paths {
            c.pending.remove(p);
        }
    }
}

fn land(p: PathBuf, f: Facts) {
    let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
    let c = g.get_or_insert_with(Cache::default);
    if c.map.len() >= MOST {
        c.map.clear();
    }
    c.pending.remove(&p);
    c.map.insert(p, f);
    c.generation += 1;
}

/// Hand what rows asked for this frame to the workers (started on first use), with the
/// tools to use: the system's, or under test the fakes this thread was given.
pub fn flush(ctx: &egui::Context) {
    let batch = std::mem::take(&mut *QUEUE.lock().unwrap_or_else(|e| e.into_inner()));
    if batch.is_empty() {
        return;
    }
    let mut w = WORK.lock().unwrap_or_else(|e| e.into_inner());
    w.tools = crate::drives::Tools::current();
    w.ctx = Some(ctx.clone());
    for (p, k) in batch {
        if k == Kind::Picture { w.pics.push_back(p) } else { w.media.push_back(p) }
    }
    if !w.started {
        w.started = true;
        for k in [Kind::Picture, Kind::Picture, Kind::Media, Kind::Media] {
            let _ = std::thread::Builder::new().name("media facts".into()).spawn(move || worker(k));
        }
    }
    WAKE.notify_all();
}

fn worker(k: Kind) {
    loop {
        let (p, tools, ctx) = {
            let mut w = WORK.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                let next = if k == Kind::Picture { w.pics.pop_front() } else { w.media.pop_front() };
                if let Some(p) = next {
                    break (p, w.tools.clone(), w.ctx.clone());
                }
                w = WAKE.wait(w).unwrap_or_else(|e| e.into_inner());
            }
        };
        if !wanted(&p) {
            unpend(&[p]);
            continue;
        }
        let f = if k == Kind::Picture { Some(picture(&p)) } else { media(&p, tools.as_ref()) };
        match f {
            Some(f) => land(p, f),
            // Stopped: its folder was left.
            None => unpend(&[p]),
        }
        if let Some(c) = ctx {
            c.request_repaint();
        }
    }
}

// ------------------------------------------------------------------ pictures

/// A photo's EXIF date and its size from the header.
pub fn picture(p: &Path) -> Facts {
    let Ok(mut f) = std::fs::File::open(p) else { return Facts::default() };
    if !f.metadata().is_ok_and(|m| m.is_file()) {
        return Facts::default();
    }
    let mut b = Vec::with_capacity(64 << 10);
    if (&mut f).take(64 << 10).read_to_end(&mut b).is_err() {
        return Facts::default();
    }
    // A JPEG whose EXIF holds a big preview can start its picture further in.
    if b.starts_with(&[0xFF, 0xD8]) && header_dims(&b).is_none() && b.len() == 64 << 10 {
        let _ = (&mut f).take(960 << 10).read_to_end(&mut b);
    }
    let exif = crate::exif::block(&b).map(crate::exif::parse);
    let turned = exif.as_ref().is_some_and(|x| x.orientation >= 5);
    let dims = header_dims(&b).map(|(w, h)| if turned { (h, w) } else { (w, h) });
    // The camera's clock, in no time zone: read as local time, as it was shown.
    let taken = exif.and_then(|x| x.taken).and_then(|[y, mo, d, h, mi, s]| crate::timefmt::parse_local(&format!("{y}-{mo}-{d}T{h}:{mi}:{s}")));
    Facts { taken, dims, duration: None }
}

/// Width and height from a PNG, GIF, BMP, WebP or JPEG header.
pub fn header_dims(b: &[u8]) -> Option<(u32, u32)> {
    let be16 = |i: usize| Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32);
    let le16 = |i: usize| Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32);
    let le24 = |i: usize| Some(u32::from_le_bytes([*b.get(i)?, *b.get(i + 1)?, *b.get(i + 2)?, 0]));
    let le32 = |i: usize| Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?));
    let be32 = |i: usize| Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?));
    let d = if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.get(12..16) == Some(b"IHDR") {
        (be32(16)?, be32(20)?)
    } else if b.starts_with(b"GIF8") {
        (le16(6)?, le16(8)?)
    } else if b.starts_with(b"BM") {
        (le32(18)?, (le32(22)? as i32).unsigned_abs())
    } else if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        match b.get(12..16)? {
            b"VP8 " => (le16(26)? & 0x3FFF, le16(28)? & 0x3FFF),
            b"VP8L" => {
                let bits = le32(21)?;
                (1 + (bits & 0x3FFF), 1 + ((bits >> 14) & 0x3FFF))
            }
            b"VP8X" => (1 + le24(24)?, 1 + le24(27)?),
            _ => return None,
        }
    } else if b.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2;
        loop {
            while b.get(i) == Some(&0xFF) && b.get(i + 1) == Some(&0xFF) {
                i += 1;
            }
            if b.get(i) != Some(&0xFF) {
                return None;
            }
            let m = *b.get(i + 1)?;
            match m {
                // Start of frame (not DHT, JPG or DAC, which share the range).
                0xC0..=0xCF if !matches!(m, 0xC4 | 0xC8 | 0xCC) => break (be16(i + 7)?, be16(i + 5)?),
                0xD8 | 0x01 | 0xD0..=0xD7 => i += 2,
                0xDA | 0xD9 => return None,
                _ => i += 2 + be16(i + 2)? as usize,
            }
        }
    } else {
        return None;
    };
    (d.0 > 0 && d.1 > 0).then_some(d)
}

// ------------------------------------------------------------------ films and recordings

/// What ffprobe says, from the kept answers when the file is as it was. None when it
/// was stopped.
fn media(p: &Path, tools: Option<&crate::drives::Tools>) -> Option<Facts> {
    use std::os::unix::fs::MetadataExt;
    let Some(t) = tools.filter(|t| t.has("ffprobe")) else { return Some(Facts::default()) };
    let Ok(m) = std::fs::metadata(p) else { return Some(Facts::default()) };
    let stamp = (m.size(), m.mtime(), m.mtime_nsec());
    if let Some(f) = kept(p, stamp) {
        return Some(f);
    }
    let mut c = crate::media::tool_cmd(t.cmd("ffprobe"));
    c.args(["-v", "error", "-of", "flat", "-show_format", "-show_streams"]).arg(p);
    let out = crate::media::output_capped(c, Duration::from_secs(15), 4 << 20, &|| !wanted(p));
    if !wanted(p) {
        return None;
    }
    let f = out.as_deref().and_then(crate::media::parse_probe).map(|pr| Facts {
        taken: pr.video.and(pr.created),
        dims: pr.video,
        duration: (pr.duration > 0.0).then_some(pr.duration),
    });
    let f = f.unwrap_or_default();
    keep(p, stamp, f);
    Some(f)
}

type Stamp = (u64, i64, i64);

struct Kept {
    loaded: bool,
    map: Option<HashMap<PathBuf, (Stamp, Facts)>>,
}

static KEPT: Mutex<Kept> = Mutex::new(Kept { loaded: false, map: None });

pub fn kept_path() -> PathBuf {
    // Tests keep theirs away from the home folder of whoever runs them.
    #[cfg(test)]
    return std::env::temp_dir().join(format!("maia-test-media-facts-{}", std::process::id()));
    #[cfg(not(test))]
    dirs::cache_dir().unwrap_or_else(|| crate::fsx::home().join(".cache")).join("maia/media-facts")
}

/// One line a file: size, time changed (seconds, nanoseconds), taken (seconds or -),
/// width, height, duration, then the path (last, so it may hold anything but a line
/// break).
fn line(p: &Path, s: Stamp, f: &Facts) -> Option<String> {
    let path = p.to_str().filter(|x| !x.contains('\n'))?;
    let taken = f.taken.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or("-".into(), |d| d.as_secs().to_string());
    let (w, h) = f.dims.unwrap_or((0, 0));
    Some(format!("{}\t{}\t{}\t{taken}\t{w}\t{h}\t{}\t{path}\n", s.0, s.1, s.2, f.duration.unwrap_or(0.0)))
}

fn parse_line(l: &str) -> Option<(PathBuf, Stamp, Facts)> {
    let v: Vec<&str> = l.splitn(8, '\t').collect();
    let [size, sec, nsec, taken, w, h, dur, path] = v.as_slice() else { return None };
    let (w, h): (u32, u32) = (w.parse().ok()?, h.parse().ok()?);
    let dur: f64 = dur.parse().ok()?;
    let f = Facts { taken: taken.parse::<u64>().ok().map(|s| UNIX_EPOCH + Duration::from_secs(s)), dims: (w > 0 && h > 0).then_some((w, h)), duration: (dur > 0.0).then_some(dur) };
    Some((PathBuf::from(path), (size.parse().ok()?, sec.parse().ok()?, nsec.parse().ok()?), f))
}

fn kept(p: &Path, s: Stamp) -> Option<Facts> {
    let mut k = KEPT.lock().unwrap_or_else(|e| e.into_inner());
    if !k.loaded {
        k.loaded = true;
        let path = kept_path();
        // Grown past a few MB with answers for files long changed: start again.
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 8 << 20) {
            let _ = std::fs::remove_file(&path);
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            let map = k.map.get_or_insert_with(HashMap::new);
            for (p, st, f) in text.lines().filter_map(parse_line) {
                map.insert(p, (st, f));
            }
        }
    }
    k.map.as_ref()?.get(p).filter(|(st, _)| *st == s).map(|(_, f)| *f)
}

fn keep(p: &Path, s: Stamp, f: Facts) {
    let Some(l) = line(p, s, &f) else { return };
    KEPT.lock().unwrap_or_else(|e| e.into_inner()).map.get_or_insert_with(HashMap::new).insert(p.to_path_buf(), (s, f));
    let path = kept_path();
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(l.as_bytes());
    }
}

// ------------------------------------------------------------------ sorting and grouping

/// The number an item sorts by for these keys; None when it has none (or it isn't read
/// yet), which sorts last.
pub fn sort_key(e: &Entry, k: crate::config::SortKey) -> Option<u64> {
    use crate::config::SortKey as K;
    let f = get(e)?;
    match k {
        K::Taken => f.taken.map(|t| match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs() | 1 << 63,
            Err(d) => (1u64 << 63) - d.duration().as_secs(),
        }),
        K::Dimensions => f.dims.map(|(w, h)| w as u64 * h as u64),
        K::Duration => f.duration.map(|d| (d * 1000.0) as u64),
        _ => None,
    }
}

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
pub const NO_DATE: &str = "No date";

/// "October 2026": the month an item was taken, in local time.
pub fn month_label(e: &Entry) -> String {
    match get(e).and_then(|f| f.taken) {
        Some(t) => {
            let tm = crate::timefmt::local(t);
            format!("{} {}", MONTHS[tm.tm_mon.clamp(0, 11) as usize], tm.tm_year + 1900)
        }
        None => NO_DATE.into(),
    }
}

/// Months newest first, then what has no date.
pub fn month_rank(l: &str) -> usize {
    let Some((m, y)) = l.split_once(' ') else { return usize::MAX };
    match (MONTHS.iter().position(|x| *x == m), y.parse::<usize>()) {
        (Some(m), Ok(y)) if y < 10_000 => (10_000 - y) * 12 + (11 - m),
        _ => usize::MAX,
    }
}

/// "4032 × 3024".
pub fn fmt_dims(d: (u32, u32)) -> String {
    format!("{} × {}", d.0, d.1)
}

/// A UTC time as ffprobe writes it ("2024-05-01T10:00:00.000000Z").
pub fn parse_utc(s: &str) -> Option<SystemTime> {
    let n: Vec<i64> = s.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).take(6).filter_map(|p| p.parse().ok()).collect();
    let [y, mo, d, h, mi, sec] = n.as_slice() else { return None };
    if !(1..=12).contains(mo) || !(1..=31).contains(d) || *y < 1971 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let (y, mo) = if *mo <= 2 { (y - 1, mo + 9) } else { (*y, mo - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * mo + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec;
    (secs > 0).then(|| UNIX_EPOCH + Duration::from_secs(secs as u64))
}
