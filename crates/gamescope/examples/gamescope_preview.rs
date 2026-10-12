fn main() -> gtk::glib::ExitCode {
    hyprdeck_core::ui::preview(
        "io.github.mikkeyboi.Hyprdeck.Preview.Gamescope",
        hd_gamescope::pages(),
    )
}
