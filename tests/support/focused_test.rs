//! Zero-selection guard shared by the focused Task test runner and its contract
//! tests (T087). A libtest filter that matches nothing exits successfully, so a
//! focused task whose filter went stale after a module move would pass silently.

/// Number of tests one libtest summary line reports as executed (passed or
/// failed). Ignored, measured and filtered-out tests did not run. Any other
/// line yields `None`.
pub fn executed_tests(line: &str) -> Option<u64> {
    let summary = line.trim().strip_prefix("test result: ")?;
    let (_outcome, counts) = summary.split_once(". ")?;
    let mut executed = 0;
    let mut recognized = false;
    for field in counts.split(';') {
        let mut words = field.split_whitespace();
        let (Some(count), Some(kind)) = (words.next(), words.next()) else {
            continue;
        };
        if matches!(kind, "passed" | "failed") {
            executed += count.parse::<u64>().ok()?;
            recognized = true;
        }
    }
    recognized.then_some(executed)
}

/// Final outcome of one focused selection: the test command's own failure wins;
/// a successful command that executed no test is a stale selection.
pub fn selection_outcome(command_succeeded: bool, executed: u64) -> Result<(), String> {
    if !command_succeeded {
        return Err("the test command failed".to_string());
    }
    if executed == 0 {
        return Err(
            "the selection ran zero tests; point its filter at the tests' current path".to_string(),
        );
    }
    Ok(())
}
