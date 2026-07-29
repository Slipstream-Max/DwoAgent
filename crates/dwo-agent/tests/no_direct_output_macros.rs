use std::path::Path;

#[test]
fn production_sources_do_not_use_direct_output_macros() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("dwo-agent must be inside the workspace");
    let mut violations = Vec::new();
    visit_sources(&workspace.join("crates"), &mut violations);
    assert!(
        violations.is_empty(),
        "production sources use direct output macros:\n{}",
        violations.join("\n")
    );
}

fn visit_sources(path: &Path, violations: &mut Vec<String>) {
    for entry in std::fs::read_dir(path).expect("read source directory") {
        let entry = entry.expect("read source entry");
        let path = entry.path();
        if path.is_dir() {
            visit_sources(&path, violations);
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("rs")
            || !path.components().any(|part| part.as_os_str() == "src")
        {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read Rust source");
        for name in ["print", "println", "eprint", "eprintln", "dbg"] {
            let needle = format!("{name}!");
            if source.contains(&needle) {
                violations.push(format!("{} contains {needle}", path.display()));
            }
        }
    }
}
