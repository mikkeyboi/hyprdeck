fn main() -> gtk::glib::ExitCode {
    hd_updates::start_background();
    hyprdeck_core::ui::preview(
        "io.github.mikkeyboi.Hyprdeck.Preview.Updates",
        hd_updates::pages(),
    )
}
