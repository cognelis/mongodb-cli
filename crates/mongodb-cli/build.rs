//! Embeds every bundled agent Skill file so `skills install` works without the source tree.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

/// Relative to this package; each visible subdirectory is one Skill.
const SKILLS: &str = "../../plugins/mongodb-cli/skills";

fn main() {
    println!("cargo:rerun-if-changed={SKILLS}");
    let mut files = Vec::new();
    for (skill, path) in visible_entries(Path::new(SKILLS)) {
        if path.is_dir() {
            collect(&skill, &path, "", &mut files);
        }
    }
    files.sort();

    let rows: String = files
        .iter()
        .map(|(skill, relative)| {
            let source = format!("/{SKILLS}/{skill}/{relative}");
            format!(
                "    ({skill:?}, {relative:?}, \
                 include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), {source:?}))),\n"
            )
        })
        .collect();
    let out_dir = env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR for build scripts");
    fs::write(
        Path::new(&out_dir).join("skill_files.rs"),
        format!("static SKILL_FILES: &[(&str, &str, &[u8])] = &[\n{rows}];\n"),
    )
    .expect("failed to write the embedded Skill table");
}

/// Collects `(skill, path within the Skill)` for every visible file below `directory`.
fn collect(skill: &str, directory: &Path, prefix: &str, files: &mut Vec<(String, String)>) {
    for (name, path) in visible_entries(directory) {
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            collect(skill, &path, &relative, files);
        } else {
            files.push((skill.to_owned(), relative));
        }
    }
}

/// Names and paths of the entries in `directory`, skipping hidden ones such as `.DS_Store`.
fn visible_entries(directory: &Path) -> Vec<(String, PathBuf)> {
    fs::read_dir(directory)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", directory.display()))
        .into_iter()
        .map(|entry| {
            let name = entry
                .file_name()
                .into_string()
                .unwrap_or_else(|name| panic!("Skill file names must be UTF-8: {name:?}"));
            (name, entry.path())
        })
        .filter(|(name, _)| !name.starts_with('.'))
        .collect()
}
