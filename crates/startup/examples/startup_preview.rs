fn main() -> gtk::glib::ExitCode {
    hd_startup::start_background();
    hyprdeck_core::ui::preview(
        "io.github.mikkeyboi.Hyprdeck.Preview.Startup",
        hd_startup::pages(),
    )
}
