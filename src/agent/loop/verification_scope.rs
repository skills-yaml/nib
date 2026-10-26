//! Conservative applicability derived from human scope, never model plan labels.
use super::*;

pub(crate) fn report_only_request(goal: &str) -> bool {
    matches!(
        normalize_plan_goal(goal).trim_end_matches(['.', '?', '!']),
        "repo status"
            | "repository status"
            | "project status"
            | "spec status"
            | "spec and repo status"
            | "list open specs"
            | "report repository status"
            | "show repository status"
    )
}

fn clauses(text: &str) -> Vec<String> {
    let mut fenced = false;
    let mut result = Vec::new();
    for line in text.lines() {
        let line = line.trim().to_ascii_lowercase();
        if line.starts_with("```") || line.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        // Preserve provenance before splitting conjunctions into directives.
        if fenced
            || line.starts_with(['>', '"', '\''])
            || line.starts_with("example:")
            || line.starts_with("for example")
        {
            continue;
        }
        result.extend(
            line.split([';', '.'])
                .flat_map(|part| part.split(" and "))
                .flat_map(|part| part.split(" but "))
                .map(|part| part.trim().to_string()),
        );
    }
    result
}

fn non_directive(clause: &str) -> bool {
    clause.contains("not required")
        || clause.contains("without running")
        || clause.starts_with("no ")
        || ["run", "execute", "use", "verify with", "check with"]
            .iter()
            .any(|verb| {
                [
                    "do not",
                    "don't",
                    "must not",
                    "never",
                    "should not",
                    "shouldn't",
                ]
                .iter()
                .any(|negative| clause.contains(&format!("{negative} {verb}")))
            })
}

fn has_command(text: &str, command: &str) -> bool {
    text.match_indices(command).any(|(index, _)| {
        let before = &text[..index];
        let after = &text[index + command.len()..];
        (before.is_empty() || before.ends_with([' ', '`']))
            && (after.is_empty() || after.starts_with([' ', '`', ',', ':']))
    })
}

pub(crate) fn human_verification_commands(goal: &str) -> Vec<String> {
    const COMMANDS: &[&str] = &[
        "task verify",
        "task check",
        "task test",
        "cargo test",
        "cargo check",
        "cargo clippy",
    ];
    let mut commands = Vec::new();
    for clause in clauses(goal) {
        if non_directive(&clause) {
            continue;
        }
        for command in COMMANDS {
            let plain = clause.replace('`', "");
            let exact = plain.trim_start_matches("$ ") == *command;
            let directed = ["", "please ", "you must ", "must ", "you should "]
                .iter()
                .any(|lead| {
                    ["run ", "execute ", "verify with ", "check with ", "use "]
                        .iter()
                        .any(|verb| plain.starts_with(&format!("{lead}{verb}{command}")))
                })
                || plain.starts_with(&format!("do not skip the required {command}"));
            if (exact || directed) && has_command(&plain, command) {
                commands.push((*command).to_string());
            }
        }
    }
    commands.sort();
    commands.dedup();
    commands
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum VerificationApplicability {
    Required,
    NotApplicable,
    Ambiguous,
}

pub(crate) fn project_verification_applicability(
    goal: &str,
    instructions: &str,
) -> VerificationApplicability {
    let report = report_only_request(goal);
    let development_document = instructions.contains("Spec-Driven Development")
        || instructions.contains("Software Development Lifecycle");
    let mut implementation_section = false;
    let mut example_section = false;
    let mut fenced = false;
    let mut applicable = false;
    let mut ambiguous = false;
    let mut prohibited = false;
    for line in instructions.lines() {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        if line.trim_start().starts_with('#') {
            let heading = line.to_ascii_lowercase();
            example_section = heading.contains("example");
            implementation_section = [
                "implementation",
                "development",
                "quality gates",
                "testing",
                "workflow",
            ]
            .iter()
            .any(|word| heading.contains(word))
                || (development_document && heading.contains("completion criteria"));
        }
        if example_section {
            continue;
        }
        for clause in clauses(line) {
            if !has_command(&clause, "task verify") {
                continue;
            }
            if non_directive(&clause) {
                prohibited = true;
                continue;
            }
            let directive = [
                "must",
                "required",
                "require ",
                "run ",
                "use ",
                "gate",
                "before completion",
            ]
            .iter()
            .any(|word| clause.contains(word));
            if !directive {
                continue;
            }
            let reporting = ["report", "status", "every task", "all tasks", "always"]
                .iter()
                .any(|word| clause.contains(word));
            let implementation = implementation_section
                || [
                    "implementation",
                    "code changes",
                    "after edits",
                    "before merging",
                ]
                .iter()
                .any(|word| clause.contains(word));
            if !report || reporting {
                applicable = true;
            } else if !implementation {
                ambiguous = true;
            }
        }
    }
    if applicable && prohibited {
        VerificationApplicability::Ambiguous
    } else if applicable {
        VerificationApplicability::Required
    } else if ambiguous {
        VerificationApplicability::Ambiguous
    } else {
        VerificationApplicability::NotApplicable
    }
}

pub(crate) fn project_verify_applies(goal: &str, instructions: &str) -> bool {
    project_verification_applicability(goal, instructions) == VerificationApplicability::Required
}

pub(crate) fn report_tool_allowed(goal: &str, instructions: &str, call: &ToolCallRequest) -> bool {
    if !report_only_request(goal) {
        return true;
    }
    if matches!(call.name.as_str(), "ask_question" | "write_plan") {
        return true;
    }
    if call.name == "run_terminal" {
        let command = call.arguments["command"].as_str().unwrap_or_default();
        // A local executable named `ls`/`git` is not the trusted utility that
        // the risk classifier describes. Check every compound segment.
        if command.split([';', '|', '&', '\n']).any(|segment| {
            segment
                .split_whitespace()
                .next()
                .is_some_and(|program| program.contains(['/', '\\']))
        }) {
            return false;
        }
    }
    if call.name == "run_terminal"
        && call.arguments["command"].as_str() == Some("task verify")
        && project_verify_applies(goal, instructions)
    {
        return true;
    }
    let tool = ToolCall {
        invocation_id: call.invocation_id,
        tool_name: call.name.clone(),
        arguments: call.arguments.clone(),
        session_id: None,
        project_root: None,
    };
    crate::tools::classifier::classify_tool_call(&tool)
        == crate::tools::classifier::ToolRisk::ReadOnly
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn report_scope_is_exact_and_cannot_be_claimed_by_a_model_label() {
        assert!(report_only_request("repo status"));
        assert!(!report_only_request("repo status and commit changes"));
        assert!(!report_tool_allowed(
            "repo status",
            "Run task verify before completion",
            &ToolCallRequest::new("apply_patch", json!({"read_only":true}))
        ));
        assert!(report_tool_allowed(
            "repo status",
            "",
            &ToolCallRequest::new("git_status", json!({}))
        ));
        for command in ["./ls", "./git status", "git status && ./ls", "ls | bin/wc"] {
            assert!(!report_tool_allowed(
                "repo status",
                "",
                &ToolCallRequest::new(
                    "run_terminal",
                    json!({"command":command, "affected_paths":["."]})
                )
            ));
        }
    }
    #[test]
    fn implementation_gates_do_not_apply_to_reports_but_reporting_gates_do() {
        assert!(!project_verify_applies(
            "repo status",
            "# Development workflow\nRun `task verify` before completion."
        ));
        assert!(project_verify_applies(
            "implement fixes",
            "# Development workflow\nRun `task verify` before completion."
        ));
        assert!(project_verify_applies(
            "repo status",
            "Reports must run `task verify`."
        ));
        for policy in [
            "Reports must not execute task verify",
            "~~~sh\nReports must run task verify\n~~~",
        ] {
            assert!(!project_verify_applies("repo status", policy));
        }
        assert!(!project_verify_applies(
            "fix bug",
            "Do not run `task verify`.\nExample: run `task verify`."
        ));
    }
    #[test]
    fn human_commands_require_affirmative_authority() {
        assert_eq!(
            human_verification_commands("Run `task verify`"),
            ["task verify"]
        );
        for text in [
            "Do not run `task verify`",
            "Example: run `task verify`",
            "> run `task verify`",
            "Explain `task verify`",
            "> inspect files and run task verify",
            "Example: inspect files and run task verify",
            "Never execute task verify",
            "He said run task verify",
            "Run task verify-extra",
            "```sh\nrun task verify\n```",
            "\"run task verify\"",
        ] {
            assert!(human_verification_commands(text).is_empty());
        }
    }
    #[test]
    fn ambiguity_is_explicit_and_negating_skip_does_not_waive_a_gate() {
        assert_eq!(
            project_verification_applicability(
                "repo status",
                "Reports must run task verify. Reports must not execute task verify."
            ),
            VerificationApplicability::Ambiguous
        );
        assert_eq!(
            project_verification_applicability("repo status", "Run task verify before completion"),
            VerificationApplicability::Ambiguous
        );
        assert!(project_verify_applies(
            "fix bug",
            "Do not skip the required `task verify` gate"
        ));
        for text in [
            "Run `task verify` and do not edit files",
            "Run task verify before finishing",
            "Run task verify after updating examples",
        ] {
            assert_eq!(human_verification_commands(text), ["task verify"]);
        }
    }
}
