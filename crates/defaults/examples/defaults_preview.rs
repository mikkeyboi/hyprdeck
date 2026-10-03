fn main() -> gtk::glib::ExitCode {
    hd_defaults::start_background();
    hyprdeck_core::ui::preview(
        "io.github.mikkeyboi.Hyprdeck.Preview.Defaults",
        hd_defaults::pages(),
    )
}
