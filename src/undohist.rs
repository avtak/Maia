//! Undo History: the changes Undo can take back, newest first, under those Redo can do
//! again. Taking one back takes back everything after it first, in order, the way
//! pressing Undo that many times would: a change undone out of order could put a file
//! back where a later change expects it gone. Doing one again does the ones before it
//! first, for the same reason.

use egui::{CornerRadius, FontId, Sense, pos2, vec2};

use crate::app::{App, UndoKind, UndoStack};
use crate::popups as pp;

pub struct State {
    /// The row chosen, from the top: what Redo would do last, down to what it would do
    /// next, then the newest change down to the oldest.
    pub sel: usize,
    /// The chosen row not yet scrolled to (it opens on the newest change).
    pub reveal: bool,
}

impl State {
    pub fn new(u: &UndoStack) -> State {
        let r = u.redos().len();
        State { sel: if u.is_empty() { r.saturating_sub(1) } else { r }, reveal: true }
    }
}

/// A few words for what a change was, under its own label.
fn what(k: &UndoKind) -> &'static str {
    match k {
        UndoKind::Rename(_) => "Renamed",
        UndoKind::Move(_) => "Moved",
        UndoKind::Created(_) => "Made",
        UndoKind::Trashed(_) => "Moved to the trash",
        UndoKind::Replaced(_) => "Changed in place",
        UndoKind::Sorted(..) => "Sorted into folders",
        UndoKind::Copied(..) => "Copied, replacing",
        UndoKind::MovedOver(..) => "Moved, replacing",
        UndoKind::Synced(..) => "Synced",
        UndoKind::Tagged(..) => "Tags changed",
    }
}

pub fn popup(app: &mut App, ctx: &egui::Context, st: &mut State) -> bool {
    let pal = app.pal;
    let k = pp::take_keys(ctx);
    if k.esc {
        return false;
    }
    let n = app.undo.len();
    // Rows above the newest change: those Redo can do again.
    let rn = app.undo.redos().len();
    let rows = n + rn;
    pp::step(&mut st.sel, rows, &k, |_| false);
    let redoing = st.sel < rn;
    let fs = app.cfg.font_size;
    let small = fs - 1.5;
    let mut take: Option<usize> = (k.enter && rows > 0).then_some(st.sel);
    let mut close = false;
    let list_max = pp::fit_list(ctx, 420.0, 170.0);
    let (_, rect) = pp::centered(ctx, "undo history", 600.0, &pal, |ui| {
        pp::dialog_title(ui, "Undo history", Some("Taking a change back takes back the ones after it first."), &pal);
        ui.add_space(8.0);
        if rows == 0 {
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Nothing to undo yet").color(pal.text_dim));
            ui.add_space(6.0);
        } else {
            let row_h = (fs * 1.35 + small * 1.35 + 14.0).round();
            let mut area = egui::ScrollArea::vertical().id_salt("undo history list").max_height(list_max).min_scrolled_height((rows as f32 * row_h).min(list_max)).auto_shrink([false, true]);
            if k.up || k.down || k.page_up || k.page_down || st.reveal {
                st.reveal = false;
                let most = (rows as f32 * row_h - list_max).max(0.0);
                area = area.vertical_scroll_offset((st.sel as f32 * row_h - list_max / 2.0 + row_h / 2.0).clamp(0.0, most));
            }
            area.show_rows(ui, row_h, rows, |ui, range| {
                for j in range {
                    // Row j < rn is Redo's, rn - j steps away; row j >= rn the change
                    // j - rn steps back from the latest.
                    let (e, when, undone) = match j.checked_sub(rn) {
                        Some(back) => (&app.undo[n - 1 - back], app.undo.when(n - 1 - back), false),
                        None => match app.undo.redos().nth(j) {
                            Some(e) => (e, app.undo.undone_when(j), true),
                            None => continue,
                        },
                    };
                    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                    // Everything between now and the chosen row goes with it: one band.
                    let on = j == st.sel;
                    let going = if redoing { j > st.sel && j < rn } else { j >= rn && j < st.sel };
                    if on {
                        ui.painter().rect_filled(r, CornerRadius::same(6), pal.selected);
                    } else if going {
                        ui.painter().rect_filled(r, CornerRadius::same(6), pal.selected.gamma_multiply(0.45));
                    } else if resp.hovered() {
                        ui.painter().rect_filled(r, CornerRadius::same(6), pal.hover);
                    }
                    // Now: between what can be done again and what can be taken back.
                    if j == rn && rn > 0 {
                        ui.painter().hline(r.x_range().shrink(6.0), r.top(), egui::Stroke::new(1.0, pal.border));
                    }
                    let (tc, dc) = if on { (pal.on_sel(), pal.on_sel_dim()) } else if undone { (pal.text_dim, pal.text_dim) } else { (pal.text, pal.text_dim) };
                    let when = crate::devtools::ago(when);
                    let wg = ui.painter().layout_no_wrap(when, FontId::proportional(small), dc);
                    let ww = wg.size().x;
                    ui.painter().galley(pos2(r.right() - 12.0 - ww, r.top() + 7.0 + (fs * 1.35 - wg.size().y) / 2.0), wg, dc);
                    let x = r.left() + 12.0;
                    let room = r.width() - 36.0 - ww;
                    let g = crate::listing::elided(ui, &e.label, FontId::proportional(fs), room, tc);
                    ui.painter().galley(pos2(x, r.top() + 6.0), g, tc);
                    let c = e.kind.count();
                    let sub = format!("{}{} · {c} {}", if undone { "Undone · " } else { "" }, what(&e.kind), if c == 1 { "item" } else { "items" });
                    let g = crate::listing::elided(ui, &sub, FontId::proportional(small), room, dc);
                    ui.painter().galley(pos2(x, r.top() + 8.0 + fs * 1.35), g, dc);
                    crate::ui::note_control(ui, r.shrink(1.0), "undo row");
                    if resp.clicked() {
                        st.sel = j;
                    }
                    if resp.double_clicked() {
                        take = Some(j);
                    }
                }
            });
        }
        pp::hints(ui, &[("↑↓", "choose"), ("Enter", if redoing { "do again" } else { "take back" }), ("Esc", "close")], &pal);
        pp::dialog_buttons(ui, |ui| {
            let (verb, steps) = if redoing { ("Redo", rn - st.sel) } else { ("Undo", st.sel.saturating_sub(rn) + 1) };
            let label = if steps > 1 && rows > 0 { format!("{verb} {steps} changes") } else { verb.to_string() };
            let b = pp::primary_button(ui, &label, rows > 0, &pal);
            crate::ui::note_control(ui, b.rect, "undo back");
            if b.clicked() {
                take = Some(st.sel);
            }
            let c = pp::plain_button(ui, "Close", &pal);
            crate::ui::note_control(ui, c.rect, "undo close");
            close = c.clicked();
        });
    });
    if let Some(j) = take {
        let redo = j < rn;
        let steps = if redo { rn - j } else { (j - rn + 1).min(n) };
        let (mut whole, mut done) = (true, 0);
        for _ in 0..steps {
            // A move across disks runs as a job: the rest waits for it, in order.
            if !app.job_steps.is_empty() {
                break;
            }
            whole &= if redo { app.redo_last() } else { app.undo_last() };
            done += 1;
        }
        let verb = if redo { "Redid" } else { "Undid" };
        if done < steps {
            app.toast(format!("{verb} {done} of {steps} changes: the next is a move under way"));
        } else if steps > 1 {
            app.toast(if whole { format!("{verb} {steps} changes") } else { format!("{verb} {steps} changes; some files couldn't be moved") });
        }
        return false;
    }
    !(close || pp::clicked_outside(ctx, rect))
}
