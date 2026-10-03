fn main() -> gtk::glib::ExitCode {
    hd_input::start_background();
    hyprdeck_core::ui::preview(
        "io.github.mikkeyboi.Hyprdeck.Preview.Input",
        hd_input::pages(),
    )
}
