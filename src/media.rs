//! Media in the inspector through the desktop's tools rather than bundled decoders:
//! `ffprobe` for details, `ffmpeg` streaming video frames, `ffplay` for sound, and
//! poppler's `pdfinfo` / `pdftoppm` for PDF pages. Every child process dies with Maia
//! (parent-death signal) and is killed when the preview goes away.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::imgdec::Rgba;

pub fn have(tool: &str) -> bool {
    static CACHE: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, v)) = c.iter().find(|(t, _)| t == tool) {
        return *v;
    }
    let v = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()));
    c.push((tool.to_string(), v));
    v
}

/// A command whose process ends when Maia does (even on a crash).
pub(crate) fn tool(bin: &str) -> Command {
    tool_cmd(Command::new(bin))
}

/// `tool` for a command made elsewhere (a fake under test, through `drives::Tools`).
pub(crate) fn tool_cmd(mut c: Command) -> Command {
    c.stdin(Stdio::null()).stderr(Stdio::null());
    unsafe {
        use std::os::unix::process::CommandExt;
        c.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    c
}

/// Output of a short-lived tool, or None after `timeout`.
pub(crate) fn output(c: Command, timeout: Duration) -> Option<Vec<u8>> {
    output_capped(c, timeout, u64::MAX, &|| false)
}

/// `output`, but None once the tool writes more than `cap` bytes or `stop` says so (the
/// tool is killed either way).
pub(crate) fn output_capped(mut c: Command, timeout: Duration, cap: u64, stop: &dyn Fn() -> bool) -> Option<Vec<u8>> {
    let mut child = c.stdout(Stdio::piped()).spawn().ok()?;
    let out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut v = vec![];
        let _ = out.take(cap.saturating_add(1)).read_to_end(&mut v);
        v
    });
    let t0 = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => return reader.join().ok().filter(|v| v.len() as u64 <= cap),
            Ok(Some(_)) => return None,
            // Past the cap the reader stops and drops the pipe, so the tool dies on its
            // next write.
            Ok(None) if t0.elapsed() < timeout && !stop() => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

// ------------------------------------------------------------------ probe

#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub duration: f64,
    /// Width and height of a real video stream (cover art doesn't count).
    pub video: Option<(u32, u32)>,
    pub audio: bool,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// The film's codec when there is a film, else the sound's.
    pub codec: String,
    /// The sound's codec.
    pub acodec: String,
    /// When the container says it was made (its creation_time).
    pub created: Option<std::time::SystemTime>,
}

/// `ffprobe -of flat` output (key="value" lines; no JSON parser needed).
pub fn parse_probe(flat: &[u8]) -> Option<Probe> {
    let text = String::from_utf8_lossy(flat);
    let mut kv = std::collections::HashMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim();
            let v = v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v);
            kv.insert(k.to_string(), v.replace("\\\"", "\"").replace("\\\\", "\\"));
        }
    }
    let get = |k: &str| kv.get(k).map(String::as_str);
    let mut p = Probe { duration: get("format.duration").and_then(|d| d.parse().ok()).unwrap_or(0.0), ..Default::default() };
    // Tags may be spelled in any case (TITLE, title).
    let tag = |k: &str| -> Option<String> {
        kv.iter().find(|(kk, _)| kk.strip_prefix("format.tags.").is_some_and(|t| t.eq_ignore_ascii_case(k))).map(|(_, v)| v.clone()).filter(|s| !s.trim().is_empty())
    };
    p.title = tag("title");
    p.artist = tag("artist");
    p.album = tag("album");
    p.created = tag("creation_time").and_then(|t| crate::facts::parse_utc(&t));
    for i in 0.. {
        let s = |f: &str| get(&format!("streams.stream.{i}.{f}"));
        let Some(kind) = s("codec_type") else { break };
        let cover = s("disposition.attached_pic") == Some("1");
        let codec = s("codec_name").unwrap_or("").to_string();
        match kind {
            "video" if !cover => {
                let w: u32 = s("width").and_then(|v| v.parse().ok()).unwrap_or(0);
                let h: u32 = s("height").and_then(|v| v.parse().ok()).unwrap_or(0);
                if w > 0 && h > 0 && p.video.is_none() {
                    p.video = Some((w, h));
                    p.codec = codec;
                }
            }
            "audio" => {
                p.audio = true;
                if p.acodec.is_empty() {
                    p.acodec = codec.clone();
                }
                if p.codec.is_empty() {
                    p.codec = codec;
                }
            }
            _ => {}
        }
    }
    (p.video.is_some() || p.audio).then_some(p)
}

pub fn probe(path: &Path) -> Option<Probe> {
    if !have("ffprobe") {
        return None;
    }
    let mut c = tool("ffprobe");
    c.args(["-v", "error", "-of", "flat", "-show_format", "-show_streams"]).arg(path);
    parse_probe(&output(c, Duration::from_secs(10))?)
}

pub fn fmt_time(t: f64) -> String {
    let t = t.max(0.0) as u64;
    if t >= 3600 { format!("{}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60) } else { format!("{}:{:02}", t / 60, t % 60) }
}

// ------------------------------------------------------------------ player

/// Latest decoded video frame (sequence number, RGBA pixels).
type FrameSlot = Arc<Mutex<(u64, Vec<u8>)>>;

pub struct Player {
    path: PathBuf,
    pub duration: f64,
    pub size: Option<(u32, u32)>,
    has_audio: bool,
    video: Option<Child>,
    audio: Option<Child>,
    frame: FrameSlot,
    ended: Arc<AtomicBool>,
    /// Playing since `started` from `from` seconds; paused at `from` otherwise.
    started: Option<Instant>,
    from: f64,
    pub muted: bool,
    ctx: egui::Context,
}

impl Player {
    pub fn new(path: &Path, probe: &Probe, max_w: u32, ctx: &egui::Context) -> Player {
        // Even dimensions no wider than the preview (rawvideo frames are exact sizes).
        let size = probe.video.map(|(w, h)| {
            let w2 = w.min(max_w.max(64));
            let h2 = ((h as u64 * w2 as u64 / w.max(1) as u64) as u32).max(2);
            (w2 & !1, h2 & !1)
        });
        Player { path: path.to_path_buf(), duration: probe.duration, size, has_audio: probe.audio, video: None, audio: None, frame: Arc::new(Mutex::new((0, vec![]))), ended: Arc::new(AtomicBool::new(false)), started: None, from: 0.0, muted: false, ctx: ctx.clone() }
    }

    pub fn playing(&self) -> bool {
        self.started.is_some()
    }

    pub fn position(&self) -> f64 {
        let p = self.from + self.started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        if self.duration > 0.0 { p.min(self.duration) } else { p }
    }

    /// Process ids of the running decoders (tests check they go away).
    pub fn pids(&self) -> Vec<u32> {
        self.video.iter().chain(self.audio.iter()).map(|c| c.id()).collect()
    }

    pub fn play(&mut self) {
        if self.playing() {
            return;
        }
        if self.duration > 0.0 && self.from >= self.duration - 0.05 {
            self.from = 0.0;
        }
        self.stop_children();
        self.ended.store(false, Ordering::Relaxed);
        let ss = format!("{:.3}", self.from);
        if let (Some((w, h)), true) = (self.size, have("ffmpeg")) {
            let mut c = tool("ffmpeg");
            c.args(["-nostdin", "-loglevel", "error", "-ss", &ss, "-re", "-i"]).arg(&self.path).args(["-an", "-vf", &format!("scale={w}:{h}"), "-f", "rawvideo", "-pix_fmt", "rgba", "-"]).stdout(Stdio::piped());
            if let Ok(mut child) = c.spawn() {
                if let Some(mut out) = child.stdout.take() {
                    let (slot, ended, ctx) = (self.frame.clone(), self.ended.clone(), self.ctx.clone());
                    let n = (w * h * 4) as usize;
                    std::thread::spawn(move || {
                        let mut buf = vec![0u8; n];
                        while out.read_exact(&mut buf).is_ok() {
                            {
                                let mut s = slot.lock().unwrap_or_else(|e| e.into_inner());
                                s.0 += 1;
                                std::mem::swap(&mut s.1, &mut buf);
                                if buf.len() != n {
                                    buf = vec![0u8; n];
                                }
                            }
                            ctx.request_repaint();
                        }
                        ended.store(true, Ordering::Relaxed);
                        ctx.request_repaint();
                    });
                }
                self.video = Some(child);
            }
        }
        if self.has_audio && !self.muted && have("ffplay") {
            let mut c = tool("ffplay");
            c.args(["-nodisp", "-autoexit", "-vn", "-loglevel", "error", "-ss", &ss]).arg(&self.path).stdout(Stdio::null());
            self.audio = c.spawn().ok();
        }
        self.started = Some(Instant::now());
    }

    pub fn pause(&mut self) {
        if let Some(_) = self.started {
            self.from = self.position();
            self.started = None;
        }
        self.stop_children();
    }

    pub fn toggle(&mut self) {
        if self.playing() { self.pause() } else { self.play() }
    }

    pub fn seek(&mut self, t: f64) {
        let was = self.playing();
        self.pause();
        self.from = t.clamp(0.0, if self.duration > 0.0 { self.duration } else { t.max(0.0) });
        if was {
            self.play();
        }
    }

    pub fn set_muted(&mut self, m: bool) {
        if self.muted != m {
            self.muted = m;
            if self.playing() {
                let t = self.position();
                self.seek(t); // restart without (or with) the sound process
            }
        }
    }

    /// Call every frame: stops at the end. Returns a new frame when one arrived.
    pub fn tick(&mut self, last_seq: &mut u64) -> Option<Vec<u8>> {
        if self.playing() && self.duration > 0.0 && self.position() >= self.duration - 0.01 && (self.video.is_none() || self.ended.load(Ordering::Relaxed)) {
            self.pause();
            self.from = self.duration;
        }
        let s = self.frame.lock().unwrap_or_else(|e| e.into_inner());
        (s.0 != *last_seq && !s.1.is_empty()).then(|| {
            *last_seq = s.0;
            s.1.clone()
        })
    }

    fn stop_children(&mut self) {
        for c in [self.video.take(), self.audio.take()].into_iter().flatten() {
            let mut c = c;
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop_children();
    }
}

// ------------------------------------------------------------------ pdf

pub fn pdf_pages(path: &Path) -> Option<usize> {
    if !have("pdfinfo") {
        return None;
    }
    let mut c = tool("pdfinfo");
    c.arg(path);
    let out = String::from_utf8_lossy(&output(c, Duration::from_secs(10))?).into_owned();
    out.lines().find_map(|l| l.strip_prefix("Pages:")).and_then(|v| v.trim().parse().ok())
}

/// One page (1-based) rendered so its larger side is `px` pixels.
pub fn pdf_page(path: &Path, page: usize, px: u32) -> Option<Rgba> {
    if !have("pdftoppm") {
        return None;
    }
    let p = page.to_string();
    let mut c = tool("pdftoppm");
    c.args(["-f", &p, "-l", &p, "-scale-to", &px.clamp(64, 4096).to_string(), "-png", "-singlefile"]).arg(path); // no output name: PNG on stdout
    crate::imgdec::decode_bytes(&output(c, Duration::from_secs(20))?)
}
