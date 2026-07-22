mod app;
mod profile_store;
mod region_calc;
mod tray_ui;

fn main() {
    if let Err(e) = app::run() {
        eprintln!("gameframe failed to start: {e}");
        std::process::exit(1);
    }
}
