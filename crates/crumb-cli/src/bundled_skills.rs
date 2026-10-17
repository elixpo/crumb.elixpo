//! Offline core skills compiled into the Crumb binary.

use std::path::{Path, PathBuf};

pub(crate) const PREFIX: &str = "builtin:";

pub(crate) struct BundledSkill {
    pub id: &'static str,
    pub description: &'static str,
    pub instructions: &'static str,
}

macro_rules! skill {
    ($id:literal, $description:literal) => {
        BundledSkill {
            id: $id,
            description: $description,
            instructions: include_str!(concat!("../../../skills/core/", $id, "/SKILL.md")),
        }
    };
}

pub(crate) static ALL: &[BundledSkill] = &[
    skill!(
        "codebase-map",
        "map an unfamiliar repository before changing it"
    ),
    skill!(
        "failure-autopsy",
        "diagnose failures from evidence before editing"
    ),
    skill!(
        "test-repair",
        "repair failing tests with minimal verified changes"
    ),
    skill!(
        "safe-refactor",
        "preserve behavior while improving structure"
    ),
    skill!(
        "security-review",
        "review trust boundaries, secrets, and unsafe flows"
    ),
    skill!(
        "performance-profile",
        "measure bottlenecks before optimizing"
    ),
    skill!(
        "docs-maintainer",
        "keep user-facing documentation accurate and concise"
    ),
    skill!(
        "release-readiness",
        "audit a local build for release blockers"
    ),
    skill!(
        "plan-execute",
        "split broad work into bounded verified packages"
    ),
];

pub(crate) fn get(id: &str) -> Option<&'static BundledSkill> {
    ALL.iter().find(|skill| skill.id == id)
}

pub(crate) fn path(id: &str) -> PathBuf {
    PathBuf::from(format!("{PREFIX}{id}"))
}

pub(crate) fn id_from_path(path: &Path) -> Option<&str> {
    path.to_str()?.strip_prefix(PREFIX)
}

pub(crate) fn instructions(path: &Path) -> Option<&'static str> {
    get(id_from_path(path)?).map(|skill| skill.instructions)
}

#[cfg(test)]
mod tests {
    use super::{ALL, get, id_from_path, path};

    #[test]
    fn bundled_paths_round_trip_to_nonempty_instructions() {
        for skill in ALL {
            assert_eq!(id_from_path(&path(skill.id)), Some(skill.id));
            assert!(get(skill.id).is_some_and(|found| !found.instructions.is_empty()));
        }
    }
}
