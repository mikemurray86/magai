use std::{collections::HashMap, path::PathBuf};

pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
}

fn global_skills_dir() -> Option<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok()?;
    Some(base.join("magai").join("skills"))
}

fn load_from_dir(dir: &PathBuf, out: &mut HashMap<String, Skill>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let description = content
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim_start_matches('#').trim().to_string())
            .unwrap_or_default();
        out.insert(
            name.clone(),
            Skill {
                name,
                description,
                content,
            },
        );
    }
}

pub fn discover() -> Vec<Skill> {
    let mut map = HashMap::new();
    if let Some(dir) = global_skills_dir() {
        load_from_dir(&dir, &mut map);
    }
    load_from_dir(&PathBuf::from(".magai/skills"), &mut map);
    let mut skills: Vec<Skill> = map.into_values().collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

pub fn render(content: &str, args: &str) -> String {
    if content.contains("{{args}}") {
        content.replace("{{args}}", args)
    } else if !args.is_empty() {
        format!("{}\n\n{}", content.trim_end(), args)
    } else {
        content.to_string()
    }
}
