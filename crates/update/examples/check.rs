//! `cargo run -p update --example check -- 0.6.0 [macos|windows]`: what the apps would find for that current version.
fn main() {
    let mut a = std::env::args().skip(1);
    let current = a.next().unwrap_or_else(|| "0.0.0".into());
    let platform = if a.next().as_deref() == Some("windows") { update::Platform::Windows } else { update::Platform::Macos };
    println!("{:?}", update::check(&current, platform));
}
