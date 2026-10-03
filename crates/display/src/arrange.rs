//! Arrangement view: outputs drawn to scale in layout coordinates; drag to
//! reposition with edge snapping, refusing overlapping drops.

use std::cell::RefCell;
use std::f64::consts::PI;
use std::rc::Rc;

use adw::prelude::*;
use gtk::cairo;

use crate::logic::{self, Rect};
use crate::page::Page;

const MARGIN: f64 = 20.0;
/// Snap distance in screen pixels.
const SNAP_PX: f64 = 14.0;

/// Layout → widget coordinates.
#[derive(Clone, Copy)]
struct View {
    s: f64,
    ox: f64,
    oy: f64,
    bx: f64,
    by: f64,
}

impl View {
    fn fit(rects: &[Rect], w: f64, h: f64) -> Option<View> {
        let b = logic::bounds(rects)?;
        let s = ((w - 2.0 * MARGIN) / b.w)
            .min((h - 2.0 * MARGIN) / b.h)
            .max(1e-6);
        Some(View {
            s,
            ox: (w - b.w * s) / 2.0,
            oy: (h - b.h * s) / 2.0,
            bx: b.x,
            by: b.y,
        })
    }

    fn to_screen(self, r: &Rect) -> Rect {
        Rect {
            x: (r.x - self.bx) * self.s + self.ox,
            y: (r.y - self.by) * self.s + self.oy,
            w: r.w * self.s,
            h: r.h * self.s,
        }
    }
}

struct Drag {
    idx: usize,
    start: Rect,
    /// Frozen during the drag so the view doesn't rescale under the pointer.
    view: View,
}

fn placed(page: &Page) -> Vec<(usize, Rect)> {
    page.outs
        .borrow()
        .iter()
        .enumerate()
        .filter(|(_, o)| o.placed())
        .map(|(i, o)| (i, o.rect()))
        .collect()
}

pub(crate) fn setup(page: &Rc<Page>) {
    let drag: Rc<RefCell<Option<Drag>>> = Rc::default();
    page.area.set_draw_func({
        let page = Rc::downgrade(page);
        let drag = drag.clone();
        move |area, cr, w, h| {
            if let Some(page) = page.upgrade() {
                let frozen = drag.borrow().as_ref().map(|d| (d.idx, d.view));
                draw(&page, area, cr, f64::from(w), f64::from(h), frozen);
            }
        }
    });

    let gesture = gtk::GestureDrag::new();
    gesture.connect_drag_begin({
        let page = Rc::downgrade(page);
        let drag = drag.clone();
        move |g, x, y| {
            let Some(page) = page.upgrade() else { return };
            let items = placed(&page);
            let rects: Vec<Rect> = items.iter().map(|i| i.1).collect();
            let area = &page.area;
            let view = View::fit(&rects, f64::from(area.width()), f64::from(area.height()));
            let hit = view.and_then(|v| {
                items
                    .iter()
                    .rev()
                    .find(|(_, r)| v.to_screen(r).contains(x, y))
                    .map(|&(i, r)| (i, r, v))
            });
            match hit {
                Some((idx, start, view)) if items.len() > 1 => {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    area.set_cursor_from_name(Some("grabbing"));
                    *drag.borrow_mut() = Some(Drag { idx, start, view });
                }
                _ => {
                    g.set_state(gtk::EventSequenceState::Denied);
                }
            }
        }
    });
    gesture.connect_drag_update({
        let page = Rc::downgrade(page);
        let drag = drag.clone();
        move |_, dx, dy| {
            let Some(page) = page.upgrade() else { return };
            let Some((idx, start, view)) = drag.borrow().as_ref().map(|d| (d.idx, d.start, d.view))
            else {
                return;
            };
            let proposed = Rect {
                x: start.x + dx / view.s,
                y: start.y + dy / view.s,
                ..start
            };
            let others: Vec<Rect> = placed(&page)
                .into_iter()
                .filter(|(i, _)| *i != idx)
                .map(|(_, r)| r)
                .collect();
            if let Some(r) = logic::place(proposed, &others, SNAP_PX / view.s) {
                let pos = format!("{}x{}", r.x as i64, r.y as i64);
                let moved = page.outs.borrow()[idx].draft.position.as_deref() != Some(pos.as_str());
                if moved {
                    page.edit(idx, |d| d.position = Some(pos));
                }
            }
        }
    });
    gesture.connect_drag_end({
        let page = Rc::downgrade(page);
        move |_, _, _| {
            drag.borrow_mut().take();
            if let Some(page) = page.upgrade() {
                page.area.set_cursor_from_name(None);
                page.area.queue_draw();
            }
        }
    });
    page.area.add_controller(gesture);
}

fn rounded_rect(cr: &cairo::Context, r: &Rect, radius: f64) {
    let rad = radius.min(r.w / 2.0).min(r.h / 2.0);
    cr.new_sub_path();
    cr.arc(r.right() - rad, r.y + rad, rad, -PI / 2.0, 0.0);
    cr.arc(r.right() - rad, r.bottom() - rad, rad, 0.0, PI / 2.0);
    cr.arc(r.x + rad, r.bottom() - rad, rad, PI / 2.0, PI);
    cr.arc(r.x + rad, r.y + rad, rad, PI, 1.5 * PI);
    cr.close_path();
}

fn draw(
    page: &Page,
    area: &gtk::DrawingArea,
    cr: &cairo::Context,
    w: f64,
    h: f64,
    frozen: Option<(usize, View)>,
) {
    let items = placed(page);
    let rects: Vec<Rect> = items.iter().map(|i| i.1).collect();
    let Some(view) = frozen.map(|f| f.1).or_else(|| View::fit(&rects, w, h)) else {
        return;
    };
    let accent = adw::StyleManager::default().accent_color_rgba();
    let fg = area.color();
    let (ar, ag, ab) = (
        f64::from(accent.red()),
        f64::from(accent.green()),
        f64::from(accent.blue()),
    );
    let outs = page.outs.borrow();
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    for (i, r) in &items {
        let s = view.to_screen(r);
        let inner = Rect {
            x: s.x + 1.0,
            y: s.y + 1.0,
            w: (s.w - 2.0).max(0.0),
            h: (s.h - 2.0).max(0.0),
        };
        let active = frozen.is_some_and(|f| f.0 == *i);
        rounded_rect(cr, &inner, 8.0);
        cr.set_source_rgba(ar, ag, ab, if active { 0.45 } else { 0.22 });
        let _ = cr.fill_preserve();
        cr.set_source_rgba(ar, ag, ab, 0.9);
        cr.set_line_width(2.0);
        let _ = cr.stroke();

        let o = &outs[*i];
        let m = o.mode();
        let (x, y) = o.position();
        let lines = [
            (o.name().to_owned(), 14.0, true),
            (
                format!("{}×{} @ {:.2} Hz", m.width, m.height, m.refresh),
                11.0,
                false,
            ),
            (
                format!(
                    "scale {} · {}",
                    logic::fmt_scale(o.effective_scale()),
                    logic::TRANSFORMS[o.transform().clamp(0, 7) as usize]
                ),
                11.0,
                false,
            ),
            (format!("at {x}, {y}"), 11.0, false),
        ];
        let total: f64 = lines.iter().map(|l| l.1 + 4.0).sum();
        let mut ty = s.y + (s.h - total) / 2.0;
        cr.set_source_rgba(
            f64::from(fg.red()),
            f64::from(fg.green()),
            f64::from(fg.blue()),
            f64::from(fg.alpha()),
        );
        for (text, size, bold) in &lines {
            cr.select_font_face(
                "Sans",
                cairo::FontSlant::Normal,
                if *bold {
                    cairo::FontWeight::Bold
                } else {
                    cairo::FontWeight::Normal
                },
            );
            cr.set_font_size(*size);
            ty += size + 4.0;
            let Ok(ext) = cr.text_extents(text) else {
                continue;
            };
            if ext.width() > s.w - 8.0 || ty > s.bottom() - 4.0 {
                continue;
            }
            cr.move_to(s.x + (s.w - ext.width()) / 2.0 - ext.x_bearing(), ty - 4.0);
            let _ = cr.show_text(text);
        }
    }
}
