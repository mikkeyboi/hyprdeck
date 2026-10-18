//! Native MangoHUD options. The text editor remains the lossless source of truth.
use crate::overlay;
use adw::prelude::*;
use std::{cell::Cell, collections::HashMap, rc::Rc};

pub(super) struct Controls {
    rows: Vec<(&'static str, Input)>,
    syncing: Rc<Cell<bool>>,
}

enum Input {
    Toggle(adw::SwitchRow, bool),
    Number(adw::SpinRow, f64),
    Text(adw::EntryRow),
    Choice(adw::ComboRow, &'static [&'static str]),
    Color(adw::EntryRow, gtk::ColorDialogButton, &'static str),
}

impl Controls {
    pub(super) fn build(content: &gtk::Box, editor: &gtk::TextBuffer) -> Self {
        let syncing = Rc::new(Cell::new(false));
        let mut rows = Vec::new();
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Find a setting or native option…")
            .build();
        content.append(&search);
        let mut sections = Vec::new();
        for (title, description, switches, entries) in [
            (
                "FPS, GPU usage and temperatures",
                "Sensor availability depends on your hardware and drivers. CPU/GPU temperature requires the corresponding statistics row.",
                "fps|Frames per second|1;gpu_stats|GPU usage|1;gpu_temp|GPU temperature|0;cpu_stats|CPU usage|1;cpu_temp|CPU temperature|0;frametime|Frame time|1;frame_timing|Frame-time graph|1;fps_only|Number only (hides other metrics)|0;hide_engine_names|Use FPS instead of GAMESCOPE|0;hide_fps_superscript|Hide FPS suffix|0;engine_short_names|Short engine names|0;temp_fahrenheit|Temperatures in Fahrenheit|0;fps_color_change|Color FPS by thresholds|0",
                "fps_text|Custom FPS label (overrides engine-name switches);gpu_text|GPU label;cpu_text|CPU label;fps_metrics|FPS statistics (avg,0.01);fps_value|FPS color thresholds (30,60);fps_color|FPS threshold colors (hex list)",
            ),
            (
                "Layout and typography",
                "Reset removes an override. Custom labels are literal text; MangoApp does not automatically substitute the running game's name.",
                "horizontal|Horizontal layout|0;horizontal_stretch|Stretch horizontal layout|1;legacy_layout|Legacy layout|1;hud_compact|Compact layout|0;hud_no_margin|Remove margins|0;no_small_font|Uniform font size|0;text_outline|Text outline|1;no_display|Hide overlay|0;mangoapp_steam|Show above Steam UI|0;full|Enable most metrics|0",
                "font_file|Font file (absolute TTF/OTF path);font_file_text|Secondary font file;font_glyph_ranges|Font glyph ranges;custom_text|Custom text;custom_text_center|Centered heading;preset|Native preset (-1 default, 0–4)",
            ),
            (
                "GPU details",
                "Additional GPU sensors, dynamic colors and device selection.",
                "gpu_name|GPU name|0;gpu_junction_temp|Junction temperature|0;gpu_mem_temp|Memory temperature (requires VRAM)|0;gpu_core_clock|Core clock|0;gpu_mem_clock|Memory clock (requires VRAM)|0;gpu_power|Power usage|0;gpu_power_limit|Power limit|0;gpu_fan|Fan speed|0;gpu_voltage|Voltage (AMD)|0;gpu_efficiency|Efficiency|0;gpu_load_change|Color GPU load by thresholds|0;throttling_status|Throttling status|0;throttling_status_graph|Throttling on graph|0",
                "gpu_list|GPU indices (0,1);pci_dev|GPU PCI bus ID;gpu_load_value|GPU load thresholds;gpu_load_color|GPU threshold colors (hex list)",
            ),
            (
                "CPU, memory and I/O",
                "Per-core load, memory consumption and application I/O.",
                "cpu_mhz|CPU frequency|0;cpu_power|CPU power|0;cpu_efficiency|CPU efficiency|0;cpu_load_change|Color CPU load by thresholds|0;core_load|Per-core load|0;core_load_change|Color core load|0;core_bars|Per-core bars|0;core_type|CPU core type|0;vram|Video memory|0;ram|System memory|0;swap|Swap usage|0;procmem|Process resident memory|0;procmem_shared|Process shared memory|0;procmem_virt|Process virtual memory|0;proc_vram|Process video memory|0;cgroup_mem|Application cgroup memory|0;io_read|Application disk reads|0;io_write|Application disk writes|0",
                "cpu_load_value|CPU load thresholds;cpu_load_color|CPU threshold colors (hex list);cpu_custom_temp_sensor|Custom CPU temperature sensor",
            ),
            (
                "Gamescope and frame reporting",
                "MangoApp already reports Gamescope's focused application frame timing, not Hyprdeck's preview capture rate. Its renderer supplies these Gamescope-specific metrics; there is no separate configurable Gamescope text HUD here.",
                "debug|Gamescope frame and latency graphs|0;hdr|HDR status|0;refresh_rate|Display refresh rate|0;fsr|Upscaler status|0;hide_fsr_sharpness|Hide upscaler sharpness|0;resolution|Game resolution|0;show_fps_limit|Current FPS limit|0;frame_count|Frame count|0;frame_timing_detailed|Detailed frame-time graph|0;dynamic_frame_timing|Dynamic frame-time scale|0;histogram|Histogram graphs|0;engine_version|Graphics engine version|0;dx_api|DirectX API label|0;present_mode|Presentation mode|0;vulkan_driver|Vulkan driver|0;wine|Wine version|0;winesync|Wine synchronization|0;exec_name|Executable name (not a dynamic FPS label)|0;arch|MangoHUD architecture|0;display_server|Display server|0;gamemode|GameMode status|0;vkbasalt|vkBasalt status|0;fex_stats|FEX emulation statistics|0",
                "graphs|Graphs (gpu_load,cpu_load,cpu_temp,gpu_temp…);ftrace|Configured ftrace displays",
            ),
            (
                "Battery, clock, media and network",
                "Native MangoHUD information sources.",
                "battery|Battery charge|0;battery_icon|Battery icon|0;battery_watt|Battery power|0;battery_time|Battery time remaining|0;device_battery_icon|Device battery icons|0;fan|Steam Deck fan|0;time|System clock|0;time_no_label|Hide clock label|0;version|MangoHUD version|0;media_player|Media player information|0;flip_efficiency|Efficiency in joules/frame|0",
                "device_battery|Device batteries (gamepad,mouse);time_format|Clock format (%T);media_player_name|Media player name;media_player_format|Media metadata format;network|Network interfaces (1 for all, or eth0,wlo1)",
            ),
            (
                "Hotkeys and logging",
                "MangoHUD hotkeys depend on renderer/input support. Log uploads are opt-in. The exec option below runs a shell command as your user.",
                "log_versioning|Extended log headers|0;permit_upload|Permit uploading logs|0;upload_logs|Automatically upload logs|0",
                "toggle_hud|Toggle HUD hotkey;toggle_hud_position|Cycle position hotkey;toggle_preset|Cycle preset hotkey;toggle_fps_limit|Cycle FPS limits hotkey;toggle_logging|Toggle logging hotkey;reload_cfg|Reload configuration hotkey;upload_log|Upload log hotkey;reset_fps_metrics|Reset FPS metrics hotkey;output_folder|Log output folder (absolute path);output_file|Log output filename;benchmark_percentiles|Benchmark percentiles (97,AVG,1,0.1);control|Control socket name;exec|Shell command (executes as your user)",
            ),
            (
                "Injection-only game settings",
                "These options are preserved for native MangoHUD profiles, but MangoApp cannot apply a game's Vulkan/OpenGL settings or frame limiter. Use Gaming session controls for Gamescope's frame limit. Changing injection-only settings may require restarting the injected game.",
                "bicubic|Bicubic filtering|0;trilinear|Trilinear filtering|0;retro|Nearest-neighbor filtering|0;gl_bind_framebuffer|Bind framebuffer before HUD|0;gl_dont_flip|Do not flip OpenGL origin|0;fcat|FCAT analysis overlay|0",
                "fps_limit|FPS limits (0 means unlimited);fps_limit_method|FPS limiter method (early/late);vulkan_present_mode|Vulkan present mode;vsync|Vulkan VSync (-1 default, 0–3);gl_vsync|OpenGL VSync (-2 default);picmip|Mip-map LOD bias (-16…16);af|Anisotropic filtering (0…16);gl_size_query|OpenGL size query (viewport/scissorbox);blacklist|Excluded executable names;fcat_overlay_width|FCAT width;fcat_screen_edge|FCAT edge (0…3)",
            ),
        ] {
            let group = adw::PreferencesGroup::builder()
                .title(title)
                .description(description)
                .build();
            let expander = adw::ExpanderRow::builder()
                .title(title)
                .expanded(title.starts_with("FPS,"))
                .build();
            let mut section_rows: Vec<(String, gtk::Widget)> = Vec::new();
            for item in switches.split(';').filter(|v| !v.is_empty()) {
                let mut fields = item.split('|');
                let key = fields.next().unwrap();
                let title = fields.next().unwrap();
                let default = fields.next().unwrap() == "1";
                let row = adw::SwitchRow::builder().title(title).subtitle(key).build();
                add_reset(&row, key, editor, &syncing);
                row.connect_active_notify({
                    let editor = editor.clone();
                    let syncing = syncing.clone();
                    move |row| {
                        update(
                            &editor,
                            &syncing,
                            key,
                            if row.is_active() { "1" } else { "0" },
                        )
                    }
                });
                expander.add_row(&row);
                section_rows.push((
                    format!("{title} {key}").to_lowercase(),
                    row.clone().upcast(),
                ));
                rows.push((key, Input::Toggle(row, default)));
            }
            for item in entries.split(';').filter(|v| !v.is_empty()) {
                let (key, title) = item.split_once('|').unwrap();
                let row = adw::EntryRow::builder().title(title).build();
                row.set_tooltip_text(Some(key));
                add_reset(&row, key, editor, &syncing);
                row.connect_changed({
                    let editor = editor.clone();
                    let syncing = syncing.clone();
                    move |row| update(&editor, &syncing, key, &row.text())
                });
                expander.add_row(&row);
                section_rows.push((
                    format!("{title} {key}").to_lowercase(),
                    row.clone().upcast(),
                ));
                rows.push((key, Input::Text(row)));
            }
            if title == "Layout and typography" {
                let options: &'static [&'static str] = &[
                    "top-left",
                    "top-center",
                    "top-right",
                    "middle-left",
                    "middle-right",
                    "bottom-left",
                    "bottom-center",
                    "bottom-right",
                ];
                let row = adw::ComboRow::builder()
                    .title("Position")
                    .model(&gtk::StringList::new(options))
                    .build();
                add_reset(&row, "position", editor, &syncing);
                row.connect_selected_notify({
                    let editor = editor.clone();
                    let syncing = syncing.clone();
                    move |row| {
                        if let Some(value) = options.get(row.selected() as usize) {
                            update(&editor, &syncing, "position", value);
                        }
                    }
                });
                expander.add_row(&row);
                section_rows.push(("position".into(), row.clone().upcast()));
                rows.push(("position", Input::Choice(row, options)));
                for (key, title, min, max, step, default, digits) in [
                    ("font_size", "Font size (pixels)", 8., 192., 1., 24., 0),
                    (
                        "font_size_text",
                        "Secondary font size",
                        0.,
                        192.,
                        1.,
                        24.,
                        0,
                    ),
                    ("font_scale", "Font scale", 0.1, 4., 0.05, 1., 2),
                    (
                        "font_scale_media_player",
                        "Media font scale",
                        0.1,
                        4.,
                        0.05,
                        0.55,
                        2,
                    ),
                    (
                        "background_alpha",
                        "Background opacity",
                        0.,
                        1.,
                        0.05,
                        0.5,
                        2,
                    ),
                    ("alpha", "Text opacity", 0., 1., 0.05, 1., 2),
                    ("offset_x", "Horizontal offset", -8192., 8192., 1., 0., 0),
                    ("offset_y", "Vertical offset", -8192., 8192., 1., 0., 0),
                    ("width", "HUD width (0 = automatic)", 0., 8192., 1., 0., 0),
                    ("height", "HUD minimum height", 0., 8192., 1., 140., 0),
                    ("table_columns", "Table columns", 1., 64., 1., 3., 0),
                    (
                        "cellpadding_y",
                        "Vertical cell padding",
                        -10.,
                        100.,
                        0.01,
                        -0.085,
                        3,
                    ),
                    ("round_corners", "Corner radius", 0., 100., 1., 0., 0),
                    (
                        "text_outline_thickness",
                        "Text outline thickness",
                        0.,
                        10.,
                        0.1,
                        1.5,
                        1,
                    ),
                ] {
                    add_number(
                        &expander,
                        &mut rows,
                        &mut section_rows,
                        editor,
                        &syncing,
                        key,
                        title,
                        min,
                        max,
                        step,
                        default,
                        digits,
                    );
                }
            }
            if title == "Hotkeys and logging" {
                for (key, title, default) in [
                    (
                        "fps_sampling_period",
                        "Statistics interval (milliseconds)",
                        500.,
                    ),
                    ("autostart_log", "Start logging after (seconds)", 0.),
                    ("log_duration", "Log duration (seconds)", 0.),
                    ("log_interval", "Log interval (milliseconds)", 0.),
                ] {
                    add_number(
                        &expander,
                        &mut rows,
                        &mut section_rows,
                        editor,
                        &syncing,
                        key,
                        title,
                        0.,
                        86400000.,
                        1.,
                        default,
                        0,
                    );
                }
            }
            group.add(&expander);
            content.append(&group);
            sections.push((group, expander, section_rows));
        }
        let colors = adw::PreferencesGroup::builder().title("Colors").description("RGB hexadecimal colors. The picker updates the same native option as the text field.").build();
        let expander = adw::ExpanderRow::builder().title("Colors").build();
        let mut color_rows = Vec::new();
        for (key, title, default) in [
            ("text_color", "Text", "FFFFFF"),
            ("gpu_color", "GPU", "2E9762"),
            ("cpu_color", "CPU", "2E97CB"),
            ("engine_color", "FPS / engine label", "EB5B5B"),
            ("background_color", "Background", "020202"),
            ("frametime_color", "Frame-time graph", "00FF00"),
            ("vram_color", "Video memory", "AD64C1"),
            ("ram_color", "System memory", "C26693"),
            ("io_color", "Disk I/O", "A491D3"),
            ("media_player_color", "Media player", "FFFFFF"),
            ("wine_color", "Wine", "EB5B5B"),
            ("battery_color", "Battery", "FF9078"),
            ("network_color", "Network", "E07B85"),
            (
                "horizontal_separator_color",
                "Horizontal separators",
                "AD64C1",
            ),
            ("text_outline_color", "Text outline", "000000"),
        ] {
            let row = adw::EntryRow::builder().title(title).build();
            row.set_tooltip_text(Some(&format!("{key} · default {default}")));
            let picker = gtk::ColorDialogButton::new(Some(
                gtk::ColorDialog::builder().with_alpha(false).build(),
            ));
            picker.set_valign(gtk::Align::Center);
            row.add_suffix(&picker);
            add_reset(&row, key, editor, &syncing);
            row.connect_changed({
                let editor = editor.clone();
                let syncing = syncing.clone();
                move |row| update(&editor, &syncing, key, &row.text())
            });
            picker.connect_rgba_notify({
                let editor = editor.clone();
                let syncing = syncing.clone();
                move |picker| {
                    let c = picker.rgba();
                    update(
                        &editor,
                        &syncing,
                        key,
                        &format!(
                            "{:02X}{:02X}{:02X}",
                            (c.red() * 255.).round() as u8,
                            (c.green() * 255.).round() as u8,
                            (c.blue() * 255.).round() as u8
                        ),
                    );
                }
            });
            expander.add_row(&row);
            color_rows.push((
                format!("{title} {key}").to_lowercase(),
                row.clone().upcast(),
            ));
            rows.push((key, Input::Color(row, picker, default)));
        }
        colors.add(&expander);
        content.append(&colors);
        sections.push((colors, expander, color_rows));
        search.connect_search_changed(move |search| {
            let query = search.text().to_lowercase();
            for (group, expander, rows) in &sections {
                let mut visible = false;
                for (keywords, row) in rows {
                    let matches = keywords.contains(&query);
                    row.set_visible(matches);
                    visible |= matches;
                }
                group.set_visible(visible);
                if !query.is_empty() {
                    expander.set_expanded(visible);
                }
            }
        });
        Self { rows, syncing }
    }

    pub(super) fn sync(&self, text: &str) {
        if self.syncing.replace(true) {
            return;
        }
        let options: HashMap<_, _> = text.lines().filter_map(overlay::parsed_option).collect();
        for (key, input) in &self.rows {
            let value = options.get(key).copied();
            match input {
                Input::Toggle(row, default) => row.set_active(value.map_or(*default, |v| v != "0")),
                Input::Number(row, default) => {
                    let value = value.and_then(|v| v.parse().ok()).unwrap_or(*default);
                    if row.value() != value {
                        row.set_value(value);
                    }
                }
                Input::Text(row) => {
                    let value = value.unwrap_or("");
                    if row.text() != value {
                        row.set_text(value);
                    }
                }
                Input::Choice(row, options) => row.set_selected(
                    value
                        .and_then(|v| options.iter().position(|p| *p == v))
                        .map_or(gtk::INVALID_LIST_POSITION, |v| v as u32),
                ),
                Input::Color(row, picker, default) => {
                    let value = value.unwrap_or("");
                    if row.text() != value {
                        row.set_text(value);
                    }
                    if let Ok(color) = gtk::gdk::RGBA::parse(format!(
                        "#{}",
                        if value.is_empty() { default } else { value }
                    )) && picker.rgba() != color
                    {
                        picker.set_rgba(&color);
                    }
                }
            }
        }
        self.syncing.set(false);
    }
}

fn update(editor: &gtk::TextBuffer, syncing: &Cell<bool>, key: &str, value: &str) {
    if syncing.get() {
        return;
    }
    let text = editor.text(&editor.start_iter(), &editor.end_iter(), true);
    if overlay::option(&text, key) == Some(value) {
        return;
    }
    let edited = if value.is_empty() {
        overlay::remove_option(&text, key)
    } else {
        overlay::set_option(&text, key, value)
    };
    syncing.set(true);
    editor.set_text(&edited);
    syncing.set(false);
}

fn add_reset(
    row: &impl IsA<adw::PreferencesRow>,
    key: &'static str,
    editor: &gtk::TextBuffer,
    syncing: &Rc<Cell<bool>>,
) {
    let reset = gtk::Button::builder()
        .icon_name("edit-undo-symbolic")
        .tooltip_text("Reset to MangoHUD default (remove override)")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    // All supported rows inherit ActionRow or EntryRow, which have separate suffix APIs.
    if let Some(row) = row.as_ref().downcast_ref::<adw::ActionRow>() {
        row.add_suffix(&reset);
    } else if let Some(row) = row.as_ref().downcast_ref::<adw::EntryRow>() {
        row.add_suffix(&reset);
    }
    reset.connect_clicked({
        let editor = editor.clone();
        let syncing = syncing.clone();
        move |_| {
            if syncing.get() {
                return;
            }
            let text = editor.text(&editor.start_iter(), &editor.end_iter(), true);
            editor.set_text(&overlay::remove_option(&text, key));
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn add_number(
    expander: &adw::ExpanderRow,
    rows: &mut Vec<(&'static str, Input)>,
    section_rows: &mut Vec<(String, gtk::Widget)>,
    editor: &gtk::TextBuffer,
    syncing: &Rc<Cell<bool>>,
    key: &'static str,
    title: &str,
    min: f64,
    max: f64,
    step: f64,
    default: f64,
    digits: u32,
) {
    let row = adw::SpinRow::with_range(min, max, step);
    row.set_title(title);
    row.set_subtitle(key);
    row.set_digits(digits);
    add_reset(&row, key, editor, syncing);
    row.connect_value_notify({
        let editor = editor.clone();
        let syncing = syncing.clone();
        move |row| update(&editor, &syncing, key, &row.value().to_string())
    });
    expander.add_row(&row);
    section_rows.push((
        format!("{title} {key}").to_lowercase(),
        row.clone().upcast(),
    ));
    rows.push((key, Input::Number(row, default)));
}
