fn main() -> gtk::glib::ExitCode {
    hd_system::start_background();
    let mut pages = hd_system::pages();
    // `HD_PAGE=<id>` opens the preview on that page.
    if let Ok(first) = std::env::var("HD_PAGE") {
        pages.sort_by_key(|p| p.id != first);
    }
    hyprdeck_core::ui::preview("io.github.mikkeyboi.Hyprdeck.Preview.System", pages)
}
