use super::{cells, field, read, safe_relative, section, specs, Check};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    schema_version: u32,
    historical_specs: Vec<String>,
    releases: Vec<Release>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Release {
    id: String,
    component: String,
    baseline: String,
    target: String,
    impact: String,
    timing: String,
    status: String,
    owner: String,
    version_source: String,
    specs: Vec<String>,
    evidence: String,
}

fn version(value: &str) -> Result<[u64; 3], String> {
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || (part.len() > 1 && part.starts_with('0'))
                || !part.chars().all(|c| c.is_ascii_digit())
        })
    {
        return Err(format!("invalid normal version: {value}"));
    }
    let mut result = [0; 3];
    for (index, part) in parts.iter().enumerate() {
        result[index] = part.parse().map_err(|_| "version overflow")?;
    }
    Ok(result)
}

fn next(baseline: &str, impact: &str) -> Result<String, String> {
    let mut number = version(baseline)?;
    let index = match impact {
        "major" => 0,
        "minor" => 1,
        "patch" => 2,
        _ => return Err("invalid release impact".into()),
    };
    number[index] = number[index].checked_add(1).ok_or("version overflow")?;
    number
        .iter_mut()
        .skip(index + 1)
        .for_each(|value| *value = 0);
    Ok(format!("{}.{}.{}", number[0], number[1], number[2]))
}

fn impact_rows(text: &str) -> Result<Vec<Vec<&str>>, String> {
    let body = section(text, "Version Impact")?;
    let header = "| Component | Impact | Release | Rationale |";
    if !body.contains(header) {
        return Err("missing Version Impact columns".into());
    }
    let mut result = Vec::new();
    let mut components = HashSet::new();
    for line in body
        .lines()
        .filter(|line| line.starts_with('|') && *line != header && !line.starts_with("| ---"))
    {
        let values = cells(line);
        if values.len() != 4
            || values.iter().any(|value| value.is_empty())
            || !matches!(values[1], "major" | "minor" | "patch" | "none")
            || !components.insert(values[0])
        {
            return Err("invalid Version Impact row".into());
        }
        result.push(values);
    }
    if result.is_empty() {
        return Err("empty Version Impact".into());
    }
    Ok(result)
}

pub fn validate(root: &Path) -> Check {
    let ledger: Ledger = serde_json::from_str(&read(&root.join("workspace/releases.json"))?)
        .map_err(|error| error.to_string())?;
    if ledger.schema_version != 1 {
        return Err("unsupported release schema".into());
    }
    let mut historical = HashSet::new();
    for name in &ledger.historical_specs {
        if !safe_relative(name)
            || !name.starts_with("workspace/specs/done/")
            || !root.join(name).is_file()
            || !historical.insert(name.clone())
        {
            return Err("invalid or duplicate historical boundary".into());
        }
    }
    let mut members = HashMap::new();
    let mut ids = HashSet::new();
    let mut claims = HashSet::new();
    let mut open = HashSet::new();
    for release in &ledger.releases {
        release_record(root, release)?;
        if !ids.insert(&release.id)
            || !claims.insert((&release.component, &release.target))
            || (release.status != "released" && !open.insert(&release.component))
        {
            return Err("duplicate release or concurrent component reservation".into());
        }
        for member in &release.specs {
            if !safe_relative(member)
                || !root.join(member).is_file()
                || members
                    .insert((member.clone(), release.component.clone()), release)
                    .is_some()
            {
                return Err("invalid or duplicate release member".into());
            }
        }
    }
    let mut impacts: HashMap<&str, usize> = HashMap::new();
    for (state, path) in specs(root)? {
        let name =
            super::portable_path(path.strip_prefix(root).map_err(|error| error.to_string())?)?;
        let text = read(&path)?;
        for values in impact_rows(&text)? {
            if historical.contains(&name) {
                if values[2] != "historical" {
                    return Err("historical spec needs historical release".into());
                }
            } else if values[1] == "none" {
                if values[2] != "none" {
                    return Err("none impact needs none release".into());
                }
            } else {
                let release = members
                    .remove(&(name.clone(), values[0].to_owned()))
                    .ok_or("unreserved version impact")?;
                if values[2] != release.id {
                    return Err("release membership mismatch".into());
                }
                let rank = ["patch", "minor", "major"]
                    .iter()
                    .position(|impact| *impact == values[1])
                    .ok_or("invalid impact")?;
                impacts
                    .entry(&release.id)
                    .and_modify(|prior| *prior = (*prior).max(rank))
                    .or_insert(rank);
                if matches!(state.as_str(), "test" | "done") && release.status == "planned" {
                    return Err("integration needs an applied version".into());
                }
            }
        }
        if state == "done" && !historical.contains(&name) {
            main_evidence(&text)?;
        }
    }
    if !members.is_empty() {
        return Err("orphan release membership".into());
    }
    for release in &ledger.releases {
        if impacts
            .get(release.id.as_str())
            .map(|rank| ["patch", "minor", "major"][*rank])
            != Some(release.impact.as_str())
        {
            return Err("release impact does not match aggregate membership".into());
        }
    }
    Ok(())
}

fn release_record(root: &Path, release: &Release) -> Check {
    let identifier =
        regex::Regex::new(r"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$").map_err(|error| error.to_string())?;
    if !identifier.is_match(&release.id)
        || !identifier.is_match(&release.component)
        || !identifier.is_match(&release.owner)
        || release.evidence.trim().is_empty()
        || release.specs.is_empty()
        || !safe_relative(&release.version_source)
        || !matches!(release.timing.as_str(), "development-start" | "merge")
        || !matches!(release.status.as_str(), "planned" | "applied" | "released")
    {
        return Err("invalid release record".into());
    }
    if version(&release.target).is_err()
        || next(&release.baseline, &release.impact)? != release.target
    {
        return Err("incorrect release arithmetic".into());
    }
    let source: toml::Value = toml::from_str(&read(&root.join(&release.version_source))?)
        .map_err(|error| error.to_string())?;
    let actual = source
        .get("package")
        .and_then(|package| package.get("version"))
        .and_then(toml::Value::as_str)
        .ok_or("missing native package version")?;
    let expected = if release.status == "planned" {
        &release.baseline
    } else {
        &release.target
    };
    if (release.status != "released" && actual != expected)
        || (release.status == "released" && version(actual)? < version(&release.target)?)
    {
        return Err("native package version disagrees with ledger".into());
    }
    if release.timing == "development-start"
        && release.status == "planned"
        && release
            .specs
            .iter()
            .any(|path| path.contains("/development/"))
    {
        return Err("development-start version was not applied".into());
    }
    Ok(())
}

fn main_evidence(text: &str) -> Check {
    let body = section(text, "Main Merge Evidence")?;
    let revision = field(body, "Revision")?;
    if revision.len() != 40
        || !revision.chars().all(|c| c.is_ascii_hexdigit())
        || field(body, "Outcome")? != "passed"
    {
        return Err("invalid main completion evidence".into());
    }
    Ok(())
}
