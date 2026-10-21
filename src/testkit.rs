//! Test-only input driver (cargo feature `testkit`): reads commands from the FIFO named
//! by $MAIA_TEST_FIFO and injects them as egui input. Not compiled into release builds.
//!
//! Commands (coordinates in logical points, relative to the window):
//!   move X Y            click X Y [left|right|middle] [ctrl|shift|alt...]
//!   dclick X Y          drag X1 Y1 X2 Y2 [left|right] [ctrl|shift|alt...]
//!   tclick X Y          hold [ctrl|shift|alt...|none]   (held between events too)
//!   key COMBO           e.g. `key ctrl+shift+p`, `key enter`, `key alt+up`
//!   text STRING         scroll X Y DY
//!   wait N              idle N frames
//!   shot PATH           save the next fully-applied frame as PNG

use std::collections::VecDeque;
use std::io::BufRead;
use std::sync::mpsc::{Receiver, channel};

use egui::{Event, Key, Modifiers, PointerButton, Pos2, pos2, vec2};

pub struct Driver {
    rx: Receiver<String>,
    queue: VecDeque<Vec<Event>>,
    shot: Option<String>,
    shot_after: usize,
    /// (dir, frames left, t0): capture consecutive frames named by ms since the burst began.
    burst: Option<(String, usize, std::time::Instant)>,
}

impl Driver {
    pub fn start(wake: impl Fn() + Send + 'static) -> Option<Driver> {
        let path = std::env::var("MAIA_TEST_FIFO").ok()?;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            loop {
                let Ok(f) = std::fs::File::open(&path) else { return };
                for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        return;
                    }
                    wake();
                }
            }
        });
        Some(Driver { rx, queue: VecDeque::new(), shot: None, shot_after: 0, burst: None })
    }

    /// Events to inject this frame (one step per frame so widgets see press, then release).
    pub fn take(&mut self) -> Vec<Event> {
        while let Ok(line) = self.rx.try_recv() {
            if let Some(path) = line.strip_prefix("shot ") {
                // Captured once every queued step has been applied.
                self.shot = Some(path.trim().to_string());
                self.shot_after = 2;
                continue;
            }
            if let Some(rest) = line.strip_prefix("burst ") {
                // burst DIR N: the next N frames (starting with the frame that applies the
                // following input) are saved as DIR/<ms>.png.
                let mut it = rest.split_whitespace();
                if let (Some(d), Some(n)) = (it.next(), it.next().and_then(|n| n.parse().ok())) {
                    let _ = std::fs::create_dir_all(d);
                    self.burst = Some((d.to_string(), n, std::time::Instant::now()));
                }
                continue;
            }
            if line.trim() == "mem" {
                // Allocator statistics: bytes in use (arena + mmapped) and arena size.
                let m = unsafe { libc::mallinfo2() };
                eprintln!("MEM in_use={} KB (arena {} KB, mmapped {} KB) free_in_arena={} KB", (m.uordblks + m.hblkhd) / 1024, m.uordblks / 1024, m.hblkhd / 1024, m.fordblks / 1024);
                unsafe extern "C" {
                    fn malloc_stats();
                }
                unsafe { malloc_stats() };
                continue;
            }
            if let Some(n) = line.strip_prefix("wait ") {
                for _ in 0..n.trim().parse::<usize>().unwrap_or(1) {
                    self.queue.push_back(vec![]);
                }
                continue;
            }
            for step in parse(&line) {
                self.queue.push_back(step);
            }
        }
        self.queue.pop_front().unwrap_or_default()
    }

    pub fn pending(&self) -> bool {
        !self.queue.is_empty() || self.shot.is_some() || self.burst.is_some()
    }

    pub fn take_shot(&mut self) -> Option<String> {
        if let Some((d, n, t0)) = &mut self.burst {
            let path = format!("{d}/{:06}.png", t0.elapsed().as_millis());
            *n -= 1;
            if *n == 0 {
                self.burst = None;
            }
            return Some(path);
        }
        if self.shot.is_none() || !self.queue.is_empty() {
            return None;
        }
        if self.shot_after > 0 {
            self.shot_after -= 1;
            return None;
        }
        self.shot.take()
    }
}

thread_local! {
    static HELD: std::cell::Cell<Modifiers> = const { std::cell::Cell::new(Modifiers::NONE) };
}

/// The modifiers `hold` keeps down while no event says otherwise (a pointer resting with
/// Ctrl held).
pub(crate) fn held() -> Modifiers {
    HELD.with(|h| h.get())
}

fn mods_of(words: &[&str]) -> Modifiers {
    let mut m = Modifiers::NONE;
    for w in words {
        match *w {
            "ctrl" => {
                m.ctrl = true;
                m.command = true;
            }
            "shift" => m.shift = true,
            "alt" => m.alt = true,
            _ => {}
        }
    }
    m
}

fn button(w: Option<&&str>) -> PointerButton {
    match w.copied() {
        Some("right") => PointerButton::Secondary,
        Some("middle") => PointerButton::Middle,
        _ => PointerButton::Primary,
    }
}

fn press(pos: Pos2, b: PointerButton, m: Modifiers, pressed: bool) -> Event {
    Event::PointerButton { pos, button: b, pressed, modifiers: m }
}

pub(crate) fn parse(line: &str) -> Vec<Vec<Event>> {
    let w: Vec<&str> = line.split_whitespace().collect();
    let num = |i: usize| w.get(i).and_then(|s| s.parse::<f32>().ok()).unwrap_or(0.0);
    match w.first().copied() {
        Some("move") => vec![vec![Event::PointerMoved(pos2(num(1), num(2)))]],
        // down / up X Y [button]: press or release without the other half (hold a drag).
        Some("down") | Some("up") => {
            let p = pos2(num(1), num(2));
            vec![vec![Event::PointerMoved(p)], vec![press(p, button(w.get(3)), Modifiers::NONE, w[0] == "down")]]
        }
        Some("hold") => {
            HELD.with(|h| h.set(mods_of(&w[1..])));
            vec![vec![]]
        }
        Some("click") | Some("dclick") | Some("tclick") => {
            let p = pos2(num(1), num(2));
            let b = button(w.get(3));
            let m = mods_of(&w[3.min(w.len())..]);
            let mut steps = vec![vec![Event::PointerMoved(p)], vec![press(p, b, m, true)], vec![press(p, b, m, false)]];
            for _ in 0..match w[0] {
                "dclick" => 1,
                "tclick" => 2,
                _ => 0,
            } {
                steps.push(vec![press(p, b, m, true)]);
                steps.push(vec![press(p, b, m, false)]);
            }
            steps
        }
        Some("drag") => {
            let (a, z) = (pos2(num(1), num(2)), pos2(num(3), num(4)));
            let b = button(w.get(5));
            let m = mods_of(&w[5.min(w.len())..]);
            let mut steps = vec![vec![Event::PointerMoved(a)], vec![press(a, b, m, true)]];
            for k in 1..=12 {
                let t = k as f32 / 12.0;
                steps.push(vec![Event::PointerMoved(a + (z - a) * t)]);
            }
            steps.push(vec![press(z, b, m, false)]);
            steps
        }
        // scroll X Y DY [ctrl]: DY in wheel lines (1 = one notch).
        Some("scroll") => {
            let m = mods_of(&w[4.min(w.len())..]);
            vec![vec![Event::PointerMoved(pos2(num(1), num(2)))], vec![Event::MouseWheel { unit: egui::MouseWheelUnit::Line, delta: vec2(0.0, num(3)), modifiers: m }]]
        }
        Some("text") => vec![vec![Event::Text(line[5..].to_string())]],
        Some("key") => {
            let combo: Vec<&str> = w.get(1).map(|c| c.split('+').collect()).unwrap_or_default();
            let Some((last, m)) = combo.split_last() else { return vec![] };
            let m = mods_of(m);
            let name = match *last {
                "enter" => "Enter",
                "esc" | "escape" => "Escape",
                "tab" => "Tab",
                "space" => "Space",
                "up" => "ArrowUp",
                "down" => "ArrowDown",
                "left" => "ArrowLeft",
                "right" => "ArrowRight",
                "backspace" => "Backspace",
                "delete" => "Delete",
                "home" => "Home",
                "end" => "End",
                "comma" => "Comma",
                other => other,
            };
            let key = Key::from_name(name).or_else(|| Key::from_name(&name.to_uppercase()));
            let Some(key) = key else { return vec![] };
            let mut ev = vec![Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers: m }];
            // Clipboard shortcuts arrive as their own events, like on a real keyboard.
            if m.ctrl && !m.alt {
                // With the keys' release in the same frame: it carries the modifiers held
                // with them (Ctrl+Shift+C is not Ctrl+C), as the window's own state does.
                let up = Event::Key { key, physical_key: Some(key), pressed: false, repeat: false, modifiers: m };
                match key {
                    Key::C => ev = vec![Event::Copy, up],
                    Key::X => ev = vec![Event::Cut, up],
                    _ => {}
                }
            }
            if !m.ctrl && !m.alt && name.len() == 1 {
                let ch = if m.shift { name.to_uppercase() } else { name.to_lowercase() };
                ev.push(Event::Text(ch));
            }
            vec![ev, vec![Event::Key { key, physical_key: Some(key), pressed: false, repeat: false, modifiers: m }]]
        }
        _ => vec![],
    }
}

/// Read back the frame just painted and write it as a PNG (before swap, so it is the back buffer).
pub fn save_frame(gl: &egui_glow::glow::Context, size: [u32; 2], path: &str) {
    use egui_glow::glow::HasContext as _;
    let (w, h) = (size[0] as usize, size[1] as usize);
    let mut px = vec![0u8; w * h * 4];
    unsafe {
        gl.read_pixels(0, 0, w as i32, h as i32, egui_glow::glow::RGBA, egui_glow::glow::UNSIGNED_BYTE, egui_glow::glow::PixelPackData::Slice(Some(&mut px)));
    }
    let mut flipped = Vec::with_capacity(px.len());
    for row in px.chunks_exact(w * 4).rev() {
        flipped.extend_from_slice(row);
    }
    let Ok(f) = std::fs::File::create(path) else { return };
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    if let Ok(mut wr) = enc.write_header() {
        let _ = wr.write_image_data(&flipped);
    }
}
