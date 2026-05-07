///
/// skills.rs - Local skill (markdown) loader
///
/// Loads Claude Code-style skill files from `<binary_dir>/skills/<name>/SKILL.md`
/// (with `./skills/` as a fallback for `cargo run` development).
///
/// Each SKILL.md file may have YAML-ish frontmatter with `name` and `description`.
/// On startup we register only `name` + `description` into the system prompt and
/// expose a `load_skill` tool — the model fetches the body on demand.
///
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
}

pub struct SkillRegistry {
    skills: Vec<Skill>,
}

impl SkillRegistry {
    pub fn load() -> Self {
        let mut seen: Vec<Skill> = Vec::new();
        for dir in candidate_dirs() {
            if !dir.is_dir() {
                continue;
            }
            scan_dir(&dir, &mut seen);
        }
        SkillRegistry { skills: seen }
    }

    pub fn list(&self) -> &[Skill] {
        &self.skills
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// JSON schema for the `load_skill` tool. Returns None when no skills are loaded.
    pub fn tool_definition(&self) -> Option<Value> {
        if self.skills.is_empty() {
            return None;
        }
        let names: Vec<&str> = self.skills.iter().map(|s| s.name.as_str()).collect();
        Some(serde_json::json!({
            "type": "function",
            "function": {
                "name": "load_skill",
                "description": "Load the full instruction body of a skill by name. Call this when a user request matches one of the skills listed in the system prompt under 'Available skills'. After loading, follow the returned instructions.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Skill name as listed under Available skills.",
                            "enum": names,
                        }
                    },
                    "required": ["name"]
                }
            }
        }))
    }

    /// Markdown snippet to append to the system prompt advertising available skills.
    pub fn system_prompt_section(&self) -> Option<String> {
        if self.skills.is_empty() {
            return None;
        }
        let mut out = String::from("\n\n## Available skills\n");
        out.push_str(
            "You have access to the following skills. Each skill is a saved instruction set you can load on demand by calling the `load_skill` tool with its name. After loading, follow the returned instructions for the rest of the task.\n\n",
        );
        for s in &self.skills {
            out.push_str(&format!("- **{}**: {}\n", s.name, s.description));
        }
        Some(out)
    }
}

/// Resolve a skill body for the `load_skill` tool.
pub fn handle_load_skill(registry: &SkillRegistry, args: &Value) -> String {
    let name = args["name"].as_str().unwrap_or("").trim();
    if name.is_empty() {
        return "Error: load_skill requires a 'name' argument.".to_string();
    }
    match registry.get(name) {
        Some(s) => format!("# Skill: {}\n\n{}", s.name, s.body),
        None => {
            let available: Vec<&str> = registry.skills.iter().map(|s| s.name.as_str()).collect();
            format!(
                "Error: skill '{}' not found. Available skills: {}",
                name,
                available.join(", ")
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Directory resolution
// ---------------------------------------------------------------------------

fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            dirs.push(parent.join("skills"));
        }
    }
    // Fallback for `cargo run` / dev workflows: also accept ./skills under CWD.
    if let Ok(cwd) = std::env::current_dir() {
        let cwd_skills = cwd.join("skills");
        if !dirs.iter().any(|d| d == &cwd_skills) {
            dirs.push(cwd_skills);
        }
    }
    dirs
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

fn scan_dir(dir: &Path, out: &mut Vec<Skill>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("[skills] failed to read {}: {}", dir.display(), err);
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        // Look for SKILL.md (case-insensitive on the stem) in this directory.
        let skill_md = find_skill_md(&path);
        let skill_md = match skill_md {
            Some(p) => p,
            None => continue,
        };

        match load_skill_file(&skill_md, &dir_name) {
            Ok(skill) => {
                if out.iter().any(|s| s.name == skill.name) {
                    eprintln!(
                        "[skills] duplicate skill name '{}' at {} (keeping earlier)",
                        skill.name,
                        skill_md.display()
                    );
                    continue;
                }
                out.push(skill);
            }
            Err(err) => {
                eprintln!("[skills] failed to load {}: {}", skill_md.display(), err);
            }
        }
    }
}

fn find_skill_md(dir: &Path) -> Option<PathBuf> {
    for candidate in ["SKILL.md", "skill.md", "Skill.md"] {
        let p = dir.join(candidate);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Markdown + frontmatter parsing
// ---------------------------------------------------------------------------

fn load_skill_file(path: &Path, dir_name: &str) -> Result<Skill, String> {
    let raw = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let (front, body) = split_frontmatter(&raw);

    let mut name = String::new();
    let mut description = String::new();
    if let Some(front) = front {
        for (k, v) in parse_frontmatter(front) {
            match k.as_str() {
                "name" => name = v,
                "description" => description = v,
                _ => {}
            }
        }
    }

    if name.is_empty() {
        name = dir_name.to_string();
    }
    if description.is_empty() {
        description = first_meaningful_line(body).unwrap_or_else(|| "(no description)".into());
    }

    Ok(Skill {
        name,
        description,
        body: body.to_string(),
    })
}

/// Return (frontmatter_text, body_text). If no frontmatter, frontmatter is None
/// and the whole input is the body.
fn split_frontmatter(input: &str) -> (Option<&str>, &str) {
    let trimmed_start = input.trim_start_matches('\u{feff}');
    if !trimmed_start.starts_with("---") {
        return (None, input);
    }

    // Skip the opening '---' line.
    let after_open = match trimmed_start.find('\n') {
        Some(i) => &trimmed_start[i + 1..],
        None => return (None, input),
    };

    // Find a closing '---' line.
    let mut search_pos = 0usize;
    let bytes = after_open.as_bytes();
    while search_pos < bytes.len() {
        let line_end = after_open[search_pos..]
            .find('\n')
            .map(|i| search_pos + i)
            .unwrap_or(bytes.len());
        let line = &after_open[search_pos..line_end];
        if line.trim_end() == "---" {
            let front = &after_open[..search_pos];
            let body_start = if line_end < bytes.len() {
                line_end + 1
            } else {
                bytes.len()
            };
            return (Some(front), &after_open[body_start..]);
        }
        if line_end == bytes.len() {
            break;
        }
        search_pos = line_end + 1;
    }

    (None, input)
}

fn parse_frontmatter(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(idx) = line.find(':') {
            let (k, rest) = line.split_at(idx);
            let v = rest[1..].trim();
            let v = v
                .trim_start_matches('"')
                .trim_end_matches('"')
                .trim_start_matches('\'')
                .trim_end_matches('\'')
                .to_string();
            out.push((k.trim().to_lowercase(), v));
        }
    }
    out
}

fn first_meaningful_line(body: &str) -> Option<String> {
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let stripped = line.trim_start_matches('#').trim();
        if !stripped.is_empty() {
            return Some(stripped.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_frontmatter() {
        let input = "---\nname: foo\ndescription: bar\n---\nbody here\n";
        let (front, body) = split_frontmatter(input);
        assert_eq!(front, Some("name: foo\ndescription: bar\n"));
        assert_eq!(body, "body here\n");
    }

    #[test]
    fn no_frontmatter() {
        let input = "no front\nbody\n";
        let (front, body) = split_frontmatter(input);
        assert!(front.is_none());
        assert_eq!(body, input);
    }

    #[test]
    fn parses_kv_with_quotes() {
        let kv = parse_frontmatter("name: \"hello\"\ndescription: 'world'\n");
        assert_eq!(kv[0], ("name".into(), "hello".into()));
        assert_eq!(kv[1], ("description".into(), "world".into()));
    }
}
