fn main() -> gtk::glib::ExitCode {
    hd_audio::start_background();
    hyprdeck_core::ui::preview(
        "io.github.mikkeyboi.Hyprdeck.Preview.Audio",
        hd_audio::pages(),
    )
}
