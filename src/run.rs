use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct RunArgs {
    pub goal: String,

    #[arg(short, long)]
    pub session: Option<String>,

    #[arg(long, default_value_t = 15)]
    pub max_steps: u32,

    #[arg(long, default_value = "execute")]
    pub mode: String,

    #[arg(short, long)]
    pub provider: Option<String>,

    #[arg(short, long)]
    pub model: Option<String>,

    #[arg(short, long)]
    pub yes: bool,
}

pub fn run_agent(args: &RunArgs) {
    let project = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    println!("nib run: {}", args.goal);

    // Create session if needed (use Rust store for id, delegate execution)
    let session_store = nib::session::SessionStore::new(&project);
    let sid = if let Some(s) = &args.session {
        s.clone()
    } else {
        session_store.create_session().id
    };

    println!(
        "session={} mode={} max_steps={}",
        sid, args.mode, args.max_steps
    );

    // We use the Rust agent loop directly
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let loop_cfg = nib::agent::AgentLoopConfig {
        max_steps: args.max_steps,
        mode: args.mode.clone(),
        provider: args.provider.clone(),
        auto_approve: false,
        approval_handler: None,
    };

    let result = rt.block_on(nib::agent::run_agent_loop(
        project.clone(),
        &sid,
        &args.goal,
        loop_cfg,
    ));

    match result {
        Ok(summary) => {
            println!("[green]Agent run completed for session {}[/green]", sid);
            if let Some(msg) = summary.last_message {
                println!("Last: {}", &msg[..msg.len().min(300)]);
            }
        }
        Err(e) => eprintln!("Failed to launch agent: {}", e),
    }
}
