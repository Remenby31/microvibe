use std::path::Path;

/// Quick project scan to inject context about the codebase
pub fn scan_project() -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut sections = Vec::new();

    // Detect languages and frameworks
    let mut languages = Vec::new();
    let mut frameworks = Vec::new();

    if cwd.join("Cargo.toml").exists() {
        languages.push("Rust");
        frameworks.push("Cargo");
    }
    if cwd.join("package.json").exists() {
        languages.push("TypeScript/JavaScript");
        if cwd.join("next.config.js").exists() || cwd.join("next.config.mjs").exists() {
            frameworks.push("Next.js");
        } else if cwd.join("vite.config.ts").exists() || cwd.join("vite.config.js").exists() {
            frameworks.push("Vite");
        }
        if cwd.join("tsconfig.json").exists() {
            frameworks.push("TypeScript");
        }
    }
    if cwd.join("pyproject.toml").exists() || cwd.join("setup.py").exists() {
        languages.push("Python");
        if cwd.join("pyproject.toml").exists() {
            if let Ok(content) = std::fs::read_to_string(cwd.join("pyproject.toml")) {
                if content.contains("fastapi") {
                    frameworks.push("FastAPI");
                }
                if content.contains("django") {
                    frameworks.push("Django");
                }
                if content.contains("flask") {
                    frameworks.push("Flask");
                }
            }
        }
    }
    if cwd.join("go.mod").exists() {
        languages.push("Go");
    }
    if cwd.join("docker-compose.yml").exists() || cwd.join("docker-compose.yaml").exists() {
        frameworks.push("Docker Compose");
    }
    if cwd.join("Dockerfile").exists() {
        frameworks.push("Docker");
    }
    if cwd.join("Makefile").exists() {
        frameworks.push("Make");
    }

    if !languages.is_empty() {
        sections.push(format!("Languages: {}", languages.join(", ")));
    }
    if !frameworks.is_empty() {
        sections.push(format!("Frameworks: {}", frameworks.join(", ")));
    }

    // Key files (top-level, sorted by relevance)
    let key_files: Vec<&str> = [
        "README.md",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "Makefile",
        "Dockerfile",
        "docker-compose.yml",
        ".env.example",
    ]
    .iter()
    .filter(|f| cwd.join(f).exists())
    .copied()
    .collect();

    if !key_files.is_empty() {
        sections.push(format!("Key files: {}", key_files.join(", ")));
    }

    // Top-level directory structure (1 level)
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&cwd) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') && name != ".env.example" {
                continue;
            }
            if ["node_modules", "target", "__pycache__", "dist", "build", ".next"]
                .contains(&name.as_str())
            {
                continue;
            }
            if entry.metadata().map(|m| m.is_dir()).unwrap_or(false) {
                dirs.push(format!("{}/", name));
            } else {
                files.push(name);
            }
        }
    }
    dirs.sort();
    files.sort();

    if !dirs.is_empty() || !files.is_empty() {
        let mut structure = String::from("Structure: ");
        structure.push_str(&dirs.join(" "));
        if !dirs.is_empty() && !files.is_empty() {
            structure.push(' ');
        }
        // Only show first 10 files
        let shown_files: Vec<&String> = files.iter().take(10).collect();
        structure.push_str(
            &shown_files
                .iter()
                .map(|f| f.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        );
        if files.len() > 10 {
            structure.push_str(&format!(" (+{} files)", files.len() - 10));
        }
        sections.push(structure);
    }

    // Count source files
    let src_counts = count_source_files(&cwd);
    if !src_counts.is_empty() {
        sections.push(format!("Source files: {}", src_counts));
    }

    if sections.is_empty() {
        String::new()
    } else {
        format!("\n\nProject:\n{}", sections.join("\n"))
    }
}

fn count_source_files(root: &Path) -> String {
    let extensions = [
        ("rs", "Rust"),
        ("py", "Python"),
        ("ts", "TypeScript"),
        ("tsx", "TSX"),
        ("js", "JavaScript"),
        ("jsx", "JSX"),
        ("go", "Go"),
        ("java", "Java"),
        ("c", "C"),
        ("cpp", "C++"),
        ("rb", "Ruby"),
    ];

    let mut counts: Vec<(String, usize)> = Vec::new();

    for (ext, name) in &extensions {
        let pattern = format!("{}/**/*.{}", root.display(), ext);
        if let Ok(paths) = glob::glob(&pattern) {
            let count = paths
                .filter_map(|p| p.ok())
                .filter(|p| {
                    let s = p.display().to_string();
                    !s.contains("/node_modules/")
                        && !s.contains("/target/")
                        && !s.contains("/__pycache__/")
                        && !s.contains("/.venv/")
                })
                .count();
            if count > 0 {
                counts.push((name.to_string(), count));
            }
        }
    }

    counts
        .iter()
        .map(|(name, count)| format!("{} {}", count, name))
        .collect::<Vec<_>>()
        .join(", ")
}
