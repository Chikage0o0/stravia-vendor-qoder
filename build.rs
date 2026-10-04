#[path = "vendor/message-compiler/build-support/messages.rs"]
mod messages;

fn main() {
    messages::generate().expect("plugin message catalogs must be valid");
}
