fn main() -> gtk::glib::ExitCode {
    hd_display::start_background();
    hyprdeck_core::ui::preview(
        "io.github.mikkeyboi.Hyprdeck.Preview.Display",
        hd_display::pages(),
    )
}
