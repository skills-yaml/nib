pub fn show_version() {
    let version = env!("CARGO_PKG_VERSION");
    let commit = option_env!("NIB_BUILD_COMMIT").unwrap_or("unknown");
    let channel = option_env!("NIB_BUILD_CHANNEL").unwrap_or("local");
    println!("nib {} ({} - {})", version, channel, commit);
}
