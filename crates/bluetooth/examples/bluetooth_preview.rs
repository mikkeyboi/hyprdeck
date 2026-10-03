fn main() -> gtk::glib::ExitCode {
    hd_bluetooth::start_background();
    hyprdeck_core::ui::preview(
        "io.github.mikkeyboi.Hyprdeck.Preview.Bluetooth",
        hd_bluetooth::pages(),
    )
}
