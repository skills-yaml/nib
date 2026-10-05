use super::{field, read, section, specs, Check};
use std::path::Path;

pub fn text(text: &str, state: &str) -> Check {
    let memory = section(text, "Memory Impact")?;
    field(memory, "Rationale")?;
    match field(memory, "Status")? {
        "none" => Ok(()),
        "updated" => {
            if !["decisions", "facts", "preferences", "open-questions"]
                .iter()
                .any(|name| memory.contains(&format!("workspace/agents/memory/{name}.md")))
                || !memory.contains("workspace/agents/memory/changelog.md")
            {
                return Err("updated memory needs category and changelog references".into());
            }
            Ok(())
        }
        "pending" if matches!(state, "development" | "test" | "blocked") => Ok(()),
        _ => Err(format!("invalid memory status for {state}")),
    }
}

pub fn validate(root: &Path) -> Check {
    for (state, path) in specs(root)? {
        super::validate_memory_text(&read(&path)?, &state)
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(())
}
