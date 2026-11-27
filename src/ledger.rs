//! The Feature Ledger: LyraCore's Behavior files, read as a data format without I/O.

use serde::Deserialize;
use sha1::Sha1;
use sha2::{Digest, Sha256};

pub const LEDGER_DIR: &str = "conformance/features";
pub const LEDGER_BRANCH: &str = "main";
/// A gap whose fix lives in the importer is a Data Gap, which LyraCore closes by importing data.
const DATA_GAP_PREFIX: &str = "importer/";

/// One Behavior file as the git tree names it, relative to `LEDGER_DIR`, with its text.
#[derive(Debug, Clone)]
pub struct LedgerFile {
    pub path: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct VmangosRef {
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub symbol: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KnownGap {
    pub description: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub symbol: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub needs_real_client: bool,
}

impl KnownGap {
    pub fn is_data_gap(&self) -> bool {
        self.path.starts_with(DATA_GAP_PREFIX)
    }

    pub fn is_task(&self) -> bool {
        !self.is_data_gap() && !self.needs_real_client
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Behavior {
    pub id: String,
    pub system: String,
    #[serde(default)]
    pub scenarios: Vec<String>,
    #[serde(default)]
    pub vmangos: Option<VmangosRef>,
    #[serde(default)]
    pub known_gaps: Vec<KnownGap>,
    /// The file's path in the repository.
    #[serde(skip)]
    pub file: String,
}

#[derive(Debug, Clone, Default)]
pub struct Ledger {
    pub behaviors: Vec<Behavior>,
    /// Files that are not a Behavior Workshop can read, by path in the repository.
    pub unparsed: Vec<String>,
}

fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A Behavior file sits at `<system>/<behavior>.toml`. Other entries in the tree are not the Ledger.
pub fn is_ledger_path(path: &str) -> bool {
    match path.split_once('/') {
        Some((system, file)) => is_name(system) && file.strip_suffix(".toml").is_some_and(is_name),
        None => false,
    }
}

pub fn repository_path(path: &str) -> String {
    format!("{LEDGER_DIR}/{path}")
}

/// `<behavior id>-<first 8 hex digits of SHA-256 of the description>`, as LyraCore names a gap.
pub fn gap_id(behavior: &str, description: &str) -> String {
    let digest = Sha256::digest(description.as_bytes());
    let hex: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    format!("{behavior}-{hex}")
}

/// The SHA-1 git gives a blob with these bytes, which the tree names each file by.
pub fn blob_sha(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Parses each file on its own: one that fails drops only itself and is named in `unparsed`.
pub fn parse(files: &[LedgerFile]) -> Ledger {
    let mut ledger = Ledger::default();
    for file in files {
        let path = repository_path(&file.path);
        match toml::from_str::<Behavior>(&file.text) {
            Ok(mut behavior) if is_name(&behavior.id) && is_name(&behavior.system) => {
                behavior.file = path;
                ledger.behaviors.push(behavior);
            }
            _ => ledger.unparsed.push(path),
        }
    }
    ledger
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gap_id_is_the_behavior_and_the_description_hash() {
        // printf %s 'Kick does not interrupt.' | sha256sum
        assert_eq!(
            gap_id("cast_and_interrupt", "Kick does not interrupt."),
            "cast_and_interrupt-44c30d94"
        );
    }

    #[test]
    fn the_blob_sha_matches_git() {
        // printf 'hello\n' | git hash-object --stdin
        assert_eq!(
            blob_sha(b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn only_system_and_behavior_toml_paths_are_the_ledger() {
        assert!(is_ledger_path("spells/cast_and_interrupt.toml"));
        for path in [
            "README.md",
            "spells",
            "spells/notes.md",
            "spells/deep/x.toml",
            "Spells/x.toml",
            "../x.toml",
        ] {
            assert!(!is_ledger_path(path), "{path}");
        }
    }

    #[test]
    fn a_file_that_does_not_parse_drops_only_itself() {
        let files = [
            LedgerFile {
                path: "combat/broken.toml".into(),
                text: "id = \"broken\"\nsystem = ".into(),
            },
            LedgerFile {
                path: "combat/no_id.toml".into(),
                text: "system = \"combat\"".into(),
            },
            LedgerFile {
                path: "combat/swing.toml".into(),
                text: "id = \"swing\"\nsystem = \"combat\"\nfuture_field = 1\n\n[[known_gaps]]\ndescription = \"d\"\nlater = true".into(),
            },
        ];
        let ledger = parse(&files);
        assert_eq!(
            ledger.unparsed,
            vec![
                "conformance/features/combat/broken.toml",
                "conformance/features/combat/no_id.toml"
            ]
        );
        assert_eq!(ledger.behaviors.len(), 1);
        let gap = &ledger.behaviors[0].known_gaps[0];
        assert!(gap.is_task() && !gap.needs_real_client);
        assert_eq!(
            ledger.behaviors[0].file,
            "conformance/features/combat/swing.toml"
        );
    }
}
