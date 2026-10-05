//! Native governance validator exposed exclusively through repository Task gates.
#[path = "../tests/support/workspace_governance/mod.rs"]
mod governance;

fn main() {
    let module = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    match governance::validate(root, &module) {
        Ok(()) => println!("Workspace {module}: passed"),
        Err(error) => {
            eprintln!("Workspace {module}: {error}");
            std::process::exit(1);
        }
    }
}
