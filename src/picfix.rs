//! Quick fixes to pictures through ImageMagick: a quarter turn in place (the original
//! goes to the trash, so Undo brings it back), a smaller copy and a cropped copy beside
//! it. Every step turns the picture upright first, as its EXIF orientation says.

use std::path::{Path, PathBuf};
use std::time::Duration;

use egui::{Color32, CornerRadius, Pos2, Rect, Stroke, StrokeKind, Vec2, pos2, vec2};

use crate::drives::Tools;

/// A big picture can take a while; past this the change is given up.
const TIMEOUT: Duration = Duration::from_secs(120);

/// ImageMagick's command: `magick` (7), else `convert` (6).
pub fn magick(t: &Tools) -> Option<&'static str> {
    ["magick", "convert"].into_iter().find(|n| t.has(n))
}

/// Pictures ImageMagick turns, resizes and crops and writes back in their own format.
pub fn is_picture(ext: &str) -> bool {
    format(ext).is_some()
}

/// Pictures the inspector shows, to draw a crop on.
pub fn can_crop(ext: &str) -> bool {
    matches!(ext, "jpg" | "jpeg" | "png" | "webp" | "bmp" | "gif")
}

fn format(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "jpg" | "jpeg" => "JPEG",
        "png" => "PNG",
        "webp" => "WEBP",
        "bmp" => "BMP",
        "gif" => "GIF",
        "tif" | "tiff" => "TIFF",
        _ => return None,
    })
}

fn ext_of(p: &Path) -> String {
    p.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// ImageMagick reading `src` on its standard input (a name with brackets or a colon
/// can't be misread as one of its own) with `args`, writing to `out` (a file, or
/// `info:` printed).
fn im(t: &Tools, prog: &str, src: &Path, args: &[String], out: Option<(&Path, &str)>) -> Result<Vec<u8>, String> {
    let input = std::fs::File::open(src).map_err(|e| e.to_string())?;
    let mut c = t.cmd(prog);
    c.arg("-").args(args);
    let file = match out {
        Some((p, fmt)) => {
            c.arg(format!("{fmt}:-"));
            Some(std::fs::File::create(p).map_err(|e| e.to_string())?)
        }
        None => {
            c.arg("info:");
            None
        }
    };
    crate::errands::output_io(c, TIMEOUT, Some(input), file)
}

/// Width and height of `src` the right way up.
pub fn dims(t: &Tools, prog: &str, src: &Path) -> Result<[u32; 2], String> {
    let out = im(t, prog, src, &["-auto-orient".into(), "-format".into(), "%w %h\n".into()], None)?;
    let s = String::from_utf8_lossy(&out);
    let mut it = s.lines().next().unwrap_or("").split_whitespace().filter_map(|n| n.parse().ok());
    match (it.next(), it.next()) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Ok([w, h]),
        _ => Err("Can't read this picture".into()),
    }
}

/// Write `src` through ImageMagick's `args` into a new file at `dest`: through a
/// hidden file beside it, renamed into place once whole.
fn write(t: &Tools, prog: &str, src: &Path, args: &[String], dest: &Path) -> Result<(), String> {
    let fmt = format(&ext_of(src)).ok_or("Not a picture ImageMagick writes")?;
    let tmp = dest.with_file_name(format!(".{}.part", name_of(dest)));
    let r = im(t, prog, src, args, Some((&tmp, fmt))).and_then(|_| {
        if let Ok(m) = std::fs::metadata(src) {
            let _ = std::fs::set_permissions(&tmp, m.permissions());
        }
        std::fs::rename(&tmp, dest).map_err(|e| e.to_string())
    });
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Turn `path` a quarter to the right (or left) in place. Unless `again` (a turn of a
/// picture already turned, its original already in the trash), the original goes to
/// the trash first.
pub fn rotate(t: &Tools, prog: &str, path: &Path, right: bool, again: bool) -> Result<(), String> {
    let fmt = format(&ext_of(path)).ok_or("Not a picture ImageMagick writes")?;
    let tmp = path.with_file_name(format!(".{}.part", name_of(path)));
    let args = ["-auto-orient".to_string(), "-rotate".into(), if right { "90" } else { "-90" }.into()];
    if let Err(e) = im(t, prog, path, &args, Some((&tmp, fmt))) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Ok(m) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, m.permissions());
    }
    if !again {
        if let Err(e) = crate::trashcan::delete(path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.to_string());
        }
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        // Put the original back rather than leave nothing there.
        if !again {
            if let Some(it) = newest_trashed(path) {
                let _ = crate::trashcan::restore(&it);
            }
        }
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// The latest copy of `path` in the trash.
pub(crate) fn newest_trashed(path: &Path) -> Option<crate::trashcan::Item> {
    newest_trashed_by(path, None)
}

/// The latest copy of `path` the trash took by `by`, when given. Deletion dates count
/// seconds: two versions trashed in one (an archive changed twice quickly) are told
/// apart by when each was moved there, its inode's change time.
pub(crate) fn newest_trashed_by(path: &Path, by: Option<std::time::SystemTime>) -> Option<crate::trashcan::Item> {
    let moved = |it: &crate::trashcan::Item| {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(&it.file).ok().map(|m| std::time::UNIX_EPOCH + std::time::Duration::new(m.ctime().max(0) as u64, m.ctime_nsec().clamp(0, 999_999_999) as u32))
    };
    crate::trashcan::list()
        .into_iter()
        .filter(|it| it.original == path && by.is_none_or(|by| moved(it).is_none_or(|m| m <= by)))
        .max_by(|a, b| a.deleted.cmp(&b.deleted).then_with(|| moved(a).cmp(&moved(b))).then_with(|| a.file.cmp(&b.file)))
}

/// Undo a turn: the turned picture to the trash, the original back from it.
pub fn undo_replaced(paths: &[PathBuf]) -> usize {
    undo_replaced_by(paths, None)
}

/// `undo_replaced`, with the version the trash took by `by`.
pub fn undo_replaced_by(paths: &[PathBuf], by: Option<std::time::SystemTime>) -> usize {
    let mut errs = 0;
    for p in paths {
        let Some(it) = newest_trashed_by(p, by) else {
            errs += 1;
            continue;
        };
        if crate::trashcan::delete(p).is_err() || crate::trashcan::restore(&it).is_err() {
            errs += 1;
        }
    }
    errs
}

/// A size asked for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Size {
    /// A fraction of the picture.
    Scale(f32),
    /// No larger than this on its longer side; a smaller picture stays as it is.
    Fit(u32),
    /// This long on its longer side.
    Long(u32),
    /// Within this width and height.
    Within(u32, u32),
}

/// What a person types for a size: "1280", "1280 × 720" (or x, *, a space) or "50%".
pub fn parse_size(s: &str) -> Option<Size> {
    let s = s.trim();
    let ok = |n: u32| (1..=30_000).contains(&n).then_some(n);
    if let Some(p) = s.strip_suffix('%') {
        let p: f32 = p.trim().parse().ok()?;
        return (p > 0.0 && p <= 1000.0).then_some(Size::Scale(p / 100.0));
    }
    let nums: Vec<&str> = s.split(|c: char| c == 'x' || c == 'X' || c == '×' || c == '*' || c.is_whitespace()).filter(|w| !w.is_empty()).collect();
    match nums.as_slice() {
        [n] => Some(Size::Long(ok(n.parse().ok()?)?)),
        [w, h] => Some(Size::Within(ok(w.parse().ok()?)?, ok(h.parse().ok()?)?)),
        _ => None,
    }
}

/// The new width and height, or None when the picture would stay as it is.
pub fn target(d: [u32; 2], size: Size) -> Option<[u32; 2]> {
    let (w, h) = (d[0] as f64, d[1] as f64);
    let k = match size {
        Size::Scale(f) => f as f64,
        Size::Fit(n) if w.max(h) <= n as f64 => return None,
        Size::Fit(n) | Size::Long(n) => n as f64 / w.max(h),
        Size::Within(bw, bh) => (bw as f64 / w).min(bh as f64 / h),
    };
    let t = [((w * k).round() as u32).max(1), ((h * k).round() as u32).max(1)];
    (t != d).then_some(t)
}

/// "photo.jpg" at 1920 × 1440 is "photo (1920 px).jpg": named by its longer side.
pub fn resized_name(name: &str, size: [u32; 2]) -> String {
    beside(name, &format!("{} px", size[0].max(size[1])))
}

pub fn cropped_name(name: &str) -> String {
    beside(name, "cropped")
}

fn beside(name: &str, what: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({what}).{ext}"),
        _ => format!("{name} ({what})"),
    }
}

/// A copy of `path` at `size`, beside it.
pub fn resize(t: &Tools, prog: &str, path: &Path, size: Size) -> Result<Option<PathBuf>, String> {
    let d = dims(t, prog, path)?;
    let Some(to) = target(d, size) else { return Ok(None) };
    let dir = path.parent().ok_or("No folder")?;
    let dest = crate::fsx::unique_dest(dir, &resized_name(&name_of(path), to));
    write(t, prog, path, &["-auto-orient".into(), "-resize".into(), format!("{}x{}!", to[0], to[1])], &dest)?;
    Ok(Some(dest))
}

/// A copy of the part of `path` in `sel` (fractions of the upright picture), beside it.
pub fn crop(t: &Tools, prog: &str, path: &Path, sel: Rect) -> Result<PathBuf, String> {
    let d = dims(t, prog, path)?;
    let [x, y, w, h] = crop_px(sel, d).ok_or("Too small to crop")?;
    let dir = path.parent().ok_or("No folder")?;
    let dest = crate::fsx::unique_dest(dir, &cropped_name(&name_of(path)));
    write(t, prog, path, &["-auto-orient".into(), "-crop".into(), format!("{w}x{h}+{x}+{y}"), "+repage".into()], &dest)?;
    Ok(dest)
}

/// `sel` in whole pixels of a `d` picture: x, y, width, height, inside it.
pub fn crop_px(sel: Rect, d: [u32; 2]) -> Option<[u32; 4]> {
    let (w, h) = (d[0] as f32, d[1] as f32);
    let x0 = (sel.min.x.clamp(0.0, 1.0) * w).round() as u32;
    let y0 = (sel.min.y.clamp(0.0, 1.0) * h).round() as u32;
    let x1 = (sel.max.x.clamp(0.0, 1.0) * w).round() as u32;
    let y1 = (sel.max.y.clamp(0.0, 1.0) * h).round() as u32;
    (x1 > x0 && y1 > y0).then(|| [x0, y0, x1 - x0, y1 - y0])
}

// ------------------------------------------------------------------ crop on the preview

#[derive(Clone, Copy, Debug, PartialEq)]
enum Grip {
    New,
    Move,
    /// The edges taken: left, right, top, bottom.
    Edges(bool, bool, bool, bool),
}

/// A crop being drawn on the inspector's picture.
pub struct Crop {
    pub path: PathBuf,
    /// What is kept, in fractions of the picture as shown.
    pub sel: Option<Rect>,
    grip: Option<(Grip, Pos2, Rect)>,
    /// Enter pressed: the app saves the crop.
    pub save: bool,
}

/// How near an edge (in points) a drag takes it.
const EDGE: f32 = 8.0;

impl Crop {
    pub fn new(path: PathBuf) -> Crop {
        Crop { path, sel: None, grip: None, save: false }
    }

    fn grip_at(&self, p: Pos2, img: Rect) -> Grip {
        let Some(sel) = self.sel else { return Grip::New };
        let r = Rect::from_min_max(img.lerp_inside(sel.min.to_vec2()), img.lerp_inside(sel.max.to_vec2()));
        if !r.expand(EDGE).contains(p) {
            return Grip::New;
        }
        let (l, rt, t, b) = ((p.x - r.left()).abs() < EDGE, (p.x - r.right()).abs() < EDGE, (p.y - r.top()).abs() < EDGE, (p.y - r.bottom()).abs() < EDGE);
        if l || rt || t || b {
            Grip::Edges(l, rt && !l, t, b && !t)
        } else {
            Grip::Move
        }
    }

    /// Take the pointer's drags over the picture drawn at `img` and paint the selection
    /// over it, within `clip`.
    pub fn ui(&mut self, ui: &egui::Ui, resp: &egui::Response, img: Rect, clip: Rect, pal: &crate::theme::Palette) {
        let frac = |p: Pos2| pos2(((p.x - img.left()) / img.width()).clamp(0.0, 1.0), ((p.y - img.top()) / img.height()).clamp(0.0, 1.0));
        if let Some(p) = resp.hover_pos().filter(|_| self.grip.is_none()) {
            ui.ctx().set_cursor_icon(match self.grip_at(p, img) {
                Grip::New => egui::CursorIcon::Crosshair,
                Grip::Move => egui::CursorIcon::Grab,
                Grip::Edges(l, r, t, b) if (l && t) || (r && b) => egui::CursorIcon::ResizeNwSe,
                Grip::Edges(l, r, t, b) if (l && b) || (r && t) => egui::CursorIcon::ResizeNeSw,
                Grip::Edges(l, r, _, _) if l || r => egui::CursorIcon::ResizeHorizontal,
                Grip::Edges(..) => egui::CursorIcon::ResizeVertical,
            });
        }
        if resp.drag_started() {
            // Where the button went down: a drag is only seen once the pointer has
            // moved on from there, past an edge it was grabbing.
            if let Some(p) = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos()) {
                let g = self.grip_at(p, img);
                let f = frac(p);
                self.grip = Some((g, f, self.sel.unwrap_or(Rect::from_min_max(f, f))));
            }
        }
        if let (Some((g, start, was)), Some(p)) = (self.grip, resp.interact_pointer_pos().filter(|_| resp.dragged())) {
            let f = frac(p);
            self.sel = Some(match g {
                Grip::New => Rect::from_two_pos(start, f),
                Grip::Move => {
                    let d = f - start;
                    let d = vec2(d.x.clamp(-was.min.x, 1.0 - was.max.x), d.y.clamp(-was.min.y, 1.0 - was.max.y));
                    was.translate(d)
                }
                Grip::Edges(l, r, t, b) => {
                    let (mut a, mut z) = (was.min, was.max);
                    if l {
                        a.x = f.x;
                    }
                    if r {
                        z.x = f.x;
                    }
                    if t {
                        a.y = f.y;
                    }
                    if b {
                        z.y = f.y;
                    }
                    Rect::from_two_pos(a, z)
                }
            });
        }
        if resp.drag_stopped() {
            self.grip = None;
            // A click, or a sliver: nothing chosen.
            if self.sel.is_some_and(|s| s.width() * img.width() < 4.0 || s.height() * img.height() < 4.0) {
                self.sel = None;
            }
        }
        let p = ui.painter().with_clip_rect(clip);
        let shade = Color32::from_black_alpha(140);
        let Some(sel) = self.sel else {
            return;
        };
        let r = Rect::from_min_max(img.lerp_inside(sel.min.to_vec2()), img.lerp_inside(sel.max.to_vec2()));
        // The picture outside the crop, darkened as one mesh with a hole: rectangles
        // side by side leave seams where their soft edges meet.
        let mut m = egui::Mesh::default();
        for q in [img.left_top(), img.right_top(), img.right_bottom(), img.left_bottom(), r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom()] {
            m.colored_vertex(q, shade);
        }
        for k in 0..4u32 {
            let n = (k + 1) % 4;
            m.add_triangle(k, n, 4 + k);
            m.add_triangle(n, 4 + n, 4 + k);
        }
        p.add(m);
        p.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.5, pal.accent), StrokeKind::Middle);
        for c in [r.left_top(), r.right_top(), r.left_bottom(), r.right_bottom()] {
            p.rect_filled(Rect::from_center_size(c, Vec2::splat(7.0)), CornerRadius::same(1), pal.accent);
        }
    }

    /// The footer's line while cropping a `d` picture.
    pub fn hint(&self, d: [u32; 2]) -> String {
        match self.sel.and_then(|s| crop_px(s, d)) {
            Some([_, _, w, h]) => format!("{w} × {h}  ·  Enter to crop"),
            None => "Drag over the part to keep".into(),
        }
    }
}
