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
}

pub fn run_agent(args: &RunArgs) {
    let project = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    println!("nib run: {}", args.goal);

    // Create session if needed (use Rust store for id, delegate execution)
    let session_store = crate::session::SessionStore::new(&project);
    let sid = if let Some(s) = &args.session {
        s.clone()
    } else {
        session_store.create_session().id
    };

    println!(
        "session={} mode={} max_steps={}",
        sid, args.mode, args.max_steps
    );

    // For run we also delegate the heavy lifting to Python core (consistent hybrid)
    // We set extra envs and reuse similar snippet logic.
    // To avoid code dupe a helper could be shared, but for now inline minimal.

    let py_code = r#"
import os, sys, asyncio, json
from pathlib import Path
src = os.environ.get("NIB_PYTHONPATH") or "src"
if src and src not in sys.path: sys.path.insert(0, src)
project = Path(os.environ.get("NIB_PROJECT", ".")).resolve()
sid = os.environ.get("NIB_SESSION_ID", "")
goal = os.environ.get("NIB_GOAL", "")
from nib.config import LLMConfig
from nib.core.workload import SessionStore
from nib.agent.loop import run_agent_loop
from nib.tools.executor import default_executor
ss = SessionStore(project_root=project)
llm_cfg = LLMConfig(project_root=project)
llm = llm_cfg.get_current_client()
executor = default_executor
executor.session_store = ss
executor.project_root = project
summary = asyncio.run(run_agent_loop(
    session_store=ss, session_id=sid, goal=goal, llm=llm, executor=executor,
    max_steps=int(os.environ.get("NIB_MAX_STEPS","15")),
    mode=os.environ.get("NIB_MODE","execute"),
    project_root=project,
))
print("RUN_DONE")
print(json.dumps(summary)[:800])
"#;

    let pp = project.join("src").to_string_lossy().to_string();

    let mut cmd = if std::process::Command::new("uv")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        let mut c = std::process::Command::new("uv");
        c.arg("run").arg("python").arg("-c").arg(py_code);
        c
    } else {
        let mut c = std::process::Command::new("python3");
        c.arg("-c").arg(py_code);
        c
    };

    cmd.env("NIB_PROJECT", &project)
        .env("NIB_SESSION_ID", &sid)
        .env("NIB_GOAL", &args.goal)
        .env("NIB_MAX_STEPS", args.max_steps.to_string())
        .env("NIB_MODE", &args.mode)
        .env("NIB_PYTHONPATH", pp)
        .current_dir(&project);

    match cmd.status() {
        Ok(st) if st.success() => {
            println!("[green]Agent run completed for session {}[/green]", sid);
            // Optionally dump last messages
            if let Some(sess) = session_store.load(&sid) {
                if let Some(last) = sess.messages.last() {
                    println!(
                        "Last: [{}] {}",
                        last.role,
                        &last.content[..last.content.len().min(300)]
                    );
                }
            }
        }
        Ok(st) => eprintln!("Run exited non-zero: {:?}", st.code()),
        Err(e) => eprintln!("Failed to launch agent: {}", e),
    }
}
