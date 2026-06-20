use clap::Args;

#[derive(Args, Debug)]
pub struct ContextArgs {
    #[arg(default_value = ".")]
    pub path: String,

    #[arg(short, long)]
    pub task: Option<String>,
}

pub fn run_context(args: &ContextArgs) {
    println!("nib context for {}", args.path);
    println!("(Rust implementation - assembles AGENTS.md + skills from .nib context loader logic)");
}
