//! Host-owned vector artwork: no plugin paths, markup or executable content.
use gtk::cairo;
use gtk::prelude::*;

pub(crate) fn product(kind: &str, large: bool) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_width(if large { 480 } else { 250 })
        .content_height(if large { 260 } else { 150 })
        .hexpand(true)
        .build();
    let kind = kind.to_owned();
    area.update_property(&[gtk::accessible::Property::Label(match kind.as_str() {
        "mouse" => "Mouse and charging dock illustration",
        "controller" => "Game controller illustration",
        "dock" => "Charging dock illustration",
        _ => "Peripheral illustration",
    })]);
    area.set_draw_func(move |_, cr, width, height| {
        let scale = (width as f64 / 480.0).min(height as f64 / 260.0);
        let _ = cr.save();
        cr.translate(
            (width as f64 - 480.0 * scale) / 2.0,
            (height as f64 - 260.0 * scale) / 2.0,
        );
        cr.scale(scale, scale);
        match kind.as_str() {
            "mouse" => mouse(cr),
            "controller" => controller(cr),
            "dock" => dock(cr, 130.0, 65.0, 1.6),
            _ => {
                rounded(cr, 120.0, 65.0, 240.0, 135.0, 22.0);
                fill_body(cr);
                cr.set_source_rgb(0.36, 0.80, 0.62);
                rounded(cr, 148.0, 94.0, 184.0, 8.0, 4.0);
                let _ = cr.fill();
                for x in [160.0, 200.0, 240.0, 280.0, 320.0] {
                    rounded(cr, x, 130.0, 22.0, 34.0, 5.0);
                    cr.set_source_rgb(0.30, 0.34, 0.37);
                    let _ = cr.fill();
                }
            }
        }
        let _ = cr.restore();
    });
    area
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(
        x + r,
        y + h - r,
        r,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    cr.arc(
        x + r,
        y + r,
        r,
        std::f64::consts::PI,
        3.0 * std::f64::consts::FRAC_PI_2,
    );
    cr.close_path();
}

fn fill_body(cr: &cairo::Context) {
    cr.set_source_rgb(0.16, 0.19, 0.22);
    let _ = cr.fill_preserve();
    cr.set_source_rgb(0.39, 0.44, 0.47);
    cr.set_line_width(2.0);
    let _ = cr.stroke();
}

fn mouse(cr: &cairo::Context) {
    cr.move_to(207.0, 26.0);
    cr.curve_to(263.0, 22.0, 290.0, 51.0, 294.0, 109.0);
    cr.curve_to(300.0, 169.0, 298.0, 213.0, 264.0, 232.0);
    cr.curve_to(228.0, 249.0, 165.0, 233.0, 145.0, 212.0);
    cr.curve_to(123.0, 188.0, 161.0, 165.0, 164.0, 130.0);
    cr.curve_to(157.0, 65.0, 169.0, 32.0, 207.0, 26.0);
    cr.close_path();
    fill_body(cr);
    cr.set_source_rgb(0.36, 0.80, 0.62);
    cr.set_line_width(3.0);
    cr.move_to(169.0, 164.0);
    cr.curve_to(176.0, 198.0, 253.0, 231.0, 280.0, 203.0);
    let _ = cr.stroke();
    cr.set_source_rgb(0.43, 0.48, 0.51);
    cr.set_line_width(2.0);
    cr.move_to(223.0, 30.0);
    cr.line_to(223.0, 124.0);
    cr.move_to(173.0, 112.0);
    cr.curve_to(210.0, 126.0, 251.0, 127.0, 290.0, 110.0);
    let _ = cr.stroke();
    rounded(cr, 214.0, 56.0, 18.0, 40.0, 8.0);
    cr.set_source_rgb(0.36, 0.80, 0.62);
    let _ = cr.fill();
    for y in [137.0, 155.0] {
        rounded(cr, 159.0, y, 34.0, 10.0, 4.0);
        cr.set_source_rgb(0.34, 0.39, 0.42);
        let _ = cr.fill();
    }
    dock(cr, 326.0, 147.0, 0.65);
}

fn dock(cr: &cairo::Context, x: f64, y: f64, scale: f64) {
    let _ = cr.save();
    cr.translate(x, y);
    cr.scale(scale, scale);
    cr.move_to(32.0, 15.0);
    cr.line_to(110.0, 15.0);
    cr.line_to(130.0, 88.0);
    cr.line_to(15.0, 88.0);
    cr.close_path();
    fill_body(cr);
    rounded(cr, 2.0, 84.0, 143.0, 24.0, 8.0);
    fill_body(cr);
    cr.set_source_rgb(0.36, 0.80, 0.62);
    rounded(cr, 8.0, 100.0, 130.0, 4.0, 2.0);
    let _ = cr.fill();
    for x in [58.0, 84.0] {
        cr.arc(x, 35.0, 4.0, 0.0, std::f64::consts::TAU);
        cr.set_source_rgb(0.78, 0.72, 0.46);
        let _ = cr.fill();
    }
    let _ = cr.restore();
}

fn controller(cr: &cairo::Context) {
    cr.move_to(146.0, 67.0);
    cr.curve_to(175.0, 50.0, 192.0, 68.0, 240.0, 68.0);
    cr.curve_to(288.0, 68.0, 307.0, 50.0, 334.0, 67.0);
    cr.curve_to(355.0, 83.0, 381.0, 151.0, 392.0, 193.0);
    cr.curve_to(397.0, 222.0, 372.0, 235.0, 350.0, 213.0);
    cr.line_to(302.0, 166.0);
    cr.curve_to(269.0, 181.0, 213.0, 181.0, 179.0, 166.0);
    cr.line_to(132.0, 214.0);
    cr.curve_to(109.0, 235.0, 83.0, 221.0, 90.0, 192.0);
    cr.curve_to(100.0, 149.0, 125.0, 84.0, 146.0, 67.0);
    cr.close_path();
    fill_body(cr);
    for (x, y) in [(160.0, 109.0), (278.0, 154.0)] {
        cr.arc(x, y, 24.0, 0.0, std::f64::consts::TAU);
        cr.set_source_rgb(0.34, 0.39, 0.42);
        let _ = cr.fill();
        cr.arc(x, y, 17.0, 0.0, std::f64::consts::TAU);
        cr.set_source_rgb(0.11, 0.14, 0.16);
        let _ = cr.fill();
    }
    cr.set_source_rgb(0.36, 0.80, 0.62);
    for (x, y) in [
        (326.0, 92.0),
        (344.0, 111.0),
        (326.0, 130.0),
        (308.0, 111.0),
    ] {
        cr.arc(x, y, 8.0, 0.0, std::f64::consts::TAU);
        let _ = cr.fill();
    }
    rounded(cr, 187.0, 146.0, 36.0, 12.0, 3.0);
    rounded(cr, 199.0, 134.0, 12.0, 36.0, 3.0);
    cr.set_source_rgb(0.40, 0.45, 0.48);
    let _ = cr.fill();
    cr.arc(240.0, 101.0, 8.0, 0.0, std::f64::consts::TAU);
    cr.set_source_rgb(0.36, 0.80, 0.62);
    let _ = cr.fill();
}
