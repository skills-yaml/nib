use clap::Args;
use nib::context::assemble_context;

#[derive(Args, Debug)]
pub struct ContextArgs {
    #[arg(default_value = ".")]
    pub path: String,

    #[arg(short, long)]
    pub task: Option<String>,
}

pub fn run_context(args: &ContextArgs) {
    let path = std::path::PathBuf::from(&args.path);
    let ctx = assemble_context(&path, args.task.as_deref());
    println!("{ctx}");
}
