mod actions;
mod api;
mod app;
mod code_view;
mod comments;
mod conversation;
mod diff_view;
mod layout;
mod log_view;
mod review_bar;
mod settings;
mod sidebar;
mod util;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(app::App);
}
