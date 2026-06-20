use std::process::Command;

#[allow(dead_code)]
const REPOSITORY: &str = "example/nib"; // update when real repo known
#[allow(dead_code)]
const REPOSITORY_URL: &str = "https://github.com/example/nib.git";

#[allow(dead_code)]
pub fn current_build_commit() -> &'static str {
    env!("NIB_BUILD_COMMIT")
}

#[allow(dead_code)]
pub fn current_build_channel() -> &'static str {
    env!("NIB_BUILD_CHANNEL")
}

#[allow(dead_code)]
pub fn check_for_update(channel: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let current = current_build_commit();
    let latest = latest_release_commit(channel)?;
    if current == "unknown" {
        return Ok(true);
    }
    Ok(current != latest)
}

#[allow(dead_code)]
pub fn install_update(channel: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Delegate to install script logic (simplified)
    println!("To update, run the install script for channel {}", channel);
    // In full: download like install script does
    Ok(())
}

#[allow(dead_code)]
fn latest_release_commit(channel: &str) -> Result<String, Box<dyn std::error::Error>> {
    let tag = match channel {
        "prod" | "production" => "prod-latest",
        _ => "development-latest",
    };
    let ref_name = format!("refs/tags/{}", tag);
    let output = Command::new("git")
        .args(["ls-remote", REPOSITORY_URL, &ref_name])
        .output()?;

    if !output.status.success() {
        return Err("Failed to fetch latest tag".into());
    }

    let stdout = String::from_utf8(output.stdout)?;
    Ok(stdout
        .split_whitespace()
        .next()
        .unwrap_or("unknown")
        .to_string())
}
