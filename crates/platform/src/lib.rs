//! Gaming platform: game identity and release metadata (shared, game-neutral).
//!
//! Kept out of `crates/protocol` on purpose: that crate is compiled into the
//! current integrity zome, and adding even unused code to it changed the DNA
//! hash. These types join a DNA only with the planned `gaming_directory`.
//!
//! The record shapes and rules a future Gaming Registry (in the planned shared
//! `gaming_directory` DNA) and the Gaming Hub will use. **Not stored in any DNA
//! yet**: this module only fixes the contract and its validation, so the Hub,
//! the registry zome and release tooling agree on it. See ARCHITECTURE.md
//! "Gaming hApp / Multi-Game Architecture".
//!
//! # Trust model
//!
//! A download source provides bytes; cryptographic verification determines
//! whether those bytes are the expected release.
//!
//! * A [`Game`] names its `developer` (an `AgentPubKeyB64`); the registry entry
//!   must be authored by that key, as every agent-named record in this DNA is.
//! * A [`GameRelease`] is valid only if authored by that same developer key, so
//!   the Holochain action signature *is* the release signature: no second
//!   signing scheme.
//! * Each [`ReleaseArtifact`] pins its content by SHA-256 and size; the Hub
//!   hashes what it downloaded, from wherever, and installs only on a match.
//! * `dna_hash` pins the game's DNA: the Hub installs the game hApp only if the
//!   DNA it packs hashes to exactly this.
//! * [`check_update`] refuses downgrades and same-version substitutions.
//!
//! This module reads no clock, does no I/O and hashes nothing itself.

use holochain_moq_protocol::AgentPubKeyB64;
use serde::{Deserialize, Serialize};
use std::fmt;

/// `[a-z][a-z0-9-]*`, 3–32 bytes, no `--`, no trailing `-`. Stable forever:
/// it names the game's DNA namespace and its install directory.
pub const MAX_GAME_ID_LEN: usize = 32;
pub const MAX_GAME_NAME_CHARS: usize = 48;
pub const MAX_ARTIFACTS: usize = 8;
/// Package-relative artifact path: at most this many bytes and components.
pub const MAX_ARTIFACT_PATH_LEN: usize = 128;
pub const MAX_ARTIFACT_PATH_COMPONENTS: usize = 8;
/// 8 GiB.
pub const MAX_ARTIFACT_SIZE: u64 = 8 << 30;
/// Length of a base64 `DnaHash` (`uhC0k` + 48).
pub const DNA_HASH_B64_LEN: usize = 53;

pub fn valid_game_id(id: &str) -> bool {
    (3..=MAX_GAME_ID_LEN).contains(&id.len())
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.contains("--")
        && !id.ends_with('-')
}

/// Shape of a base64 `DnaHash` (not its checksum: that is `holo_hash`'s job).
pub fn looks_like_dna_hash(s: &str) -> bool {
    s.len() == DNA_HASH_B64_LEN
        && s.starts_with("uhC0k")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A path inside a release package that cannot escape the install directory:
/// relative, `/`-separated, each component `[A-Za-z0-9._-]`, no `.`/`..`
/// components, no hidden-file or drive tricks.
pub fn safe_package_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= MAX_ARTIFACT_PATH_LEN
        && p.split('/').count() <= MAX_ARTIFACT_PATH_COMPONENTS
        && p.split('/').all(|c| {
            !c.is_empty()
                && c != "."
                && c != ".."
                && !c.starts_with('.')
                && c.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Platform {
    LinuxX86_64,
    LinuxAarch64,
    WindowsX86_64,
    MacosX86_64,
    MacosAarch64,
}

/// `major.minor.patch`. Ordered; the Hub never installs a lower one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl Version {
    pub fn parse(s: &str) -> Option<Self> {
        let mut it = s.split('.').map(|p| {
            (!p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()))
                .then(|| p.parse::<u16>().ok())
                .flatten()
        });
        let v = Self {
            major: it.next()??,
            minor: it.next()??,
            patch: it.next()??,
        };
        it.next().is_none().then_some(v)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A game's identity in the registry. Authored by `developer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Game {
    pub game_id: String,
    pub name: String,
    /// The key allowed to publish releases of this game.
    pub developer: AgentPubKeyB64,
}

/// One downloadable file of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseArtifact {
    pub platform: Platform,
    /// Where the Hub places it, relative to the game's install directory.
    pub path: String,
    pub sha256: [u8; 32],
    pub size: u64,
}

impl ReleaseArtifact {
    /// Whether downloaded content (already hashed by the Hub) is this artifact.
    pub fn matches(&self, sha256: &[u8; 32], size: u64) -> bool {
        self.sha256 == *sha256 && self.size == size
    }
}

/// One version of a game. Authored by the game's developer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameRelease {
    pub game_id: String,
    pub version: Version,
    /// The realtime/control protocol generation. Peers play together only on
    /// equal values ([`protocol_compatible`]).
    pub protocol_version: u32,
    /// The game DNA this release runs against (base64 `DnaHash`).
    pub dna_hash: String,
    pub artifacts: Vec<ReleaseArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseError {
    GameId(String),
    Name,
    DnaHash(String),
    NoArtifacts,
    TooManyArtifacts(usize),
    DuplicatePlatform(Platform),
    UnsafePath(String),
    Size(u64),
    OtherGame,
    Downgrade {
        installed: Version,
        candidate: Version,
    },
    Substitution(Version),
}

impl fmt::Display for ReleaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GameId(id) => write!(f, "invalid game id {id:?}"),
            Self::Name => write!(
                f,
                "game name must be 1-{MAX_GAME_NAME_CHARS} characters without control characters"
            ),
            Self::DnaHash(h) => write!(f, "{h:?} is not a DNA hash"),
            Self::NoArtifacts => write!(f, "a release needs at least one artifact"),
            Self::TooManyArtifacts(n) => write!(f, "{n} artifacts, at most {MAX_ARTIFACTS}"),
            Self::DuplicatePlatform(p) => write!(f, "two artifacts for {p:?}"),
            Self::UnsafePath(p) => write!(f, "unsafe package path {p:?}"),
            Self::Size(s) => write!(f, "artifact size {s} not in 1..={MAX_ARTIFACT_SIZE}"),
            Self::OtherGame => write!(f, "release is for a different game"),
            Self::Downgrade {
                installed,
                candidate,
            } => {
                write!(
                    f,
                    "refusing {candidate}: {installed} is installed (downgrade)"
                )
            }
            Self::Substitution(v) => write!(f, "a different release claims installed version {v}"),
        }
    }
}

impl std::error::Error for ReleaseError {}

impl Game {
    pub fn validate(&self) -> Result<(), ReleaseError> {
        if !valid_game_id(&self.game_id) {
            return Err(ReleaseError::GameId(self.game_id.clone()));
        }
        let n = self.name.chars().count();
        if !(1..=MAX_GAME_NAME_CHARS).contains(&n) || self.name.chars().any(char::is_control) {
            return Err(ReleaseError::Name);
        }
        Ok(())
    }
}

impl GameRelease {
    pub fn validate(&self) -> Result<(), ReleaseError> {
        if !valid_game_id(&self.game_id) {
            return Err(ReleaseError::GameId(self.game_id.clone()));
        }
        if !looks_like_dna_hash(&self.dna_hash) {
            return Err(ReleaseError::DnaHash(self.dna_hash.clone()));
        }
        if self.artifacts.is_empty() {
            return Err(ReleaseError::NoArtifacts);
        }
        if self.artifacts.len() > MAX_ARTIFACTS {
            return Err(ReleaseError::TooManyArtifacts(self.artifacts.len()));
        }
        for (i, a) in self.artifacts.iter().enumerate() {
            if self.artifacts[..i].iter().any(|b| b.platform == a.platform) {
                return Err(ReleaseError::DuplicatePlatform(a.platform));
            }
            if !safe_package_path(&a.path) {
                return Err(ReleaseError::UnsafePath(a.path.clone()));
            }
            if a.size == 0 || a.size > MAX_ARTIFACT_SIZE {
                return Err(ReleaseError::Size(a.size));
            }
        }
        Ok(())
    }

    pub fn artifact_for(&self, platform: Platform) -> Option<&ReleaseArtifact> {
        self.artifacts.iter().find(|a| a.platform == platform)
    }
}

/// Whether the Hub may install `candidate` over what is `installed`: same
/// game, strictly newer, or exactly the installed release again (a no-op).
/// A lower version (downgrade, e.g. to a known-vulnerable build) or a
/// different release under the installed version number is refused.
pub fn check_update(
    installed: Option<&GameRelease>,
    candidate: &GameRelease,
) -> Result<(), ReleaseError> {
    candidate.validate()?;
    let Some(installed) = installed else {
        return Ok(());
    };
    if installed.game_id != candidate.game_id {
        return Err(ReleaseError::OtherGame);
    }
    match candidate.version.cmp(&installed.version) {
        std::cmp::Ordering::Greater => Ok(()),
        std::cmp::Ordering::Equal if candidate == installed => Ok(()),
        std::cmp::Ordering::Equal => Err(ReleaseError::Substitution(installed.version)),
        std::cmp::Ordering::Less => Err(ReleaseError::Downgrade {
            installed: installed.version,
            candidate: candidate.version,
        }),
    }
}

/// Peers can share a session only on the same protocol generation.
pub fn protocol_compatible(a: u32, b: u32) -> bool {
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    const DNA: &str = "uhC0kNX8i1bXqIhkNdg0z5__WLM37Bhxi9cKQj3qq8Buxt_uabLUN";

    fn release(v: &str) -> GameRelease {
        GameRelease {
            game_id: "turbo-kart".into(),
            version: Version::parse(v).unwrap(),
            protocol_version: 1,
            dna_hash: DNA.into(),
            artifacts: vec![ReleaseArtifact {
                platform: Platform::LinuxX86_64,
                path: "bin/turbo-kart".into(),
                sha256: [1; 32],
                size: 10_000,
            }],
        }
    }

    #[test]
    fn game_ids() {
        for ok in ["turbo-kart", "cubyz", "saber", "descent-2", "abc"] {
            assert!(valid_game_id(ok), "{ok}");
        }
        for bad in [
            "",
            "ab",
            "Turbo",
            "1kart",
            "kart--x",
            "kart-",
            "kart_x",
            "kart.x",
            "../x",
            &"a".repeat(33),
        ] {
            assert!(!valid_game_id(bad), "{bad:?}");
        }
        let g = |id: &str, name: &str| Game {
            game_id: id.into(),
            name: name.into(),
            developer: "uhCAkdev".into(),
        };
        g("cubyz", "Cubyz").validate().unwrap();
        assert!(g("cubyz", "").validate().is_err());
        assert!(g("cubyz", "bell\u{7}").validate().is_err());
        assert!(g("Cubyz", "Cubyz").validate().is_err());
    }

    #[test]
    fn versions_parse_strictly_and_order() {
        assert_eq!(
            Version::parse("1.2.3"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "-1.2.3",
            "1..3",
            "+1.2.3",
            "99999999.0.0",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
        assert!(Version::parse("1.10.0") > Version::parse("1.9.9"));
        assert_eq!(Version::parse("2.0.1").unwrap().to_string(), "2.0.1");
    }

    #[test]
    fn release_validation() {
        release("1.0.0").validate().unwrap();
        let bad = |f: fn(&mut GameRelease)| {
            let mut r = release("1.0.0");
            f(&mut r);
            r.validate().unwrap_err()
        };
        assert!(matches!(
            bad(|r| r.dna_hash = "uhCkkNotADna".into()),
            ReleaseError::DnaHash(_)
        ));
        assert!(matches!(
            bad(|r| r.dna_hash = DNA.replace("uhC0k", "uhCkk")),
            ReleaseError::DnaHash(_)
        ));
        assert_eq!(bad(|r| r.artifacts.clear()), ReleaseError::NoArtifacts);
        assert!(matches!(
            bad(|r| r.game_id = "Turbo Kart".into()),
            ReleaseError::GameId(_)
        ));
        assert_eq!(
            bad(|r| r.artifacts.push(r.artifacts[0].clone())),
            ReleaseError::DuplicatePlatform(Platform::LinuxX86_64)
        );
        assert_eq!(bad(|r| r.artifacts[0].size = 0), ReleaseError::Size(0));
        assert!(matches!(
            bad(|r| r.artifacts[0].size = MAX_ARTIFACT_SIZE + 1),
            ReleaseError::Size(_)
        ));
    }

    #[test]
    fn package_paths_cannot_escape_the_install_directory() {
        for ok in [
            "turbo-kart",
            "bin/turbo-kart",
            "assets/v1/tracks.pak",
            "Game.exe",
        ] {
            assert!(safe_package_path(ok), "{ok}");
        }
        for bad in [
            "",
            "../evil",
            "bin/../../etc/passwd",
            "/etc/passwd",
            "./x",
            "a//b",
            "C:\\Windows\\x.dll",
            "C:x",
            "bin\\x",
            ".hidden",
            "bin/.config",
            "a b",
            "dir/",
            "\u{202E}exe.txt",
            &"a/".repeat(9).trim_end_matches('/').to_string(),
            &"x".repeat(129),
        ] {
            assert!(!safe_package_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn updates_refuse_downgrades_substitutions_and_other_games() {
        let v1 = release("1.0.0");
        let v2 = release("1.1.0");
        check_update(None, &v1).unwrap();
        check_update(Some(&v1), &v2).unwrap();
        check_update(Some(&v2), &v2).unwrap(); // reinstalling the same release
        assert!(matches!(
            check_update(Some(&v2), &v1),
            Err(ReleaseError::Downgrade { .. })
        ));
        let mut swapped = v2.clone();
        swapped.artifacts[0].sha256 = [2; 32];
        assert_eq!(
            check_update(Some(&v2), &swapped),
            Err(ReleaseError::Substitution(v2.version))
        );
        let mut other = release("9.0.0");
        other.game_id = "cubyz".into();
        assert_eq!(
            check_update(Some(&v1), &other),
            Err(ReleaseError::OtherGame)
        );
        let mut unsafe_path = release("2.0.0");
        unsafe_path.artifacts[0].path = "../../bin/sh".into();
        assert!(matches!(
            check_update(Some(&v1), &unsafe_path),
            Err(ReleaseError::UnsafePath(_))
        ));
    }

    #[test]
    fn artifacts_match_only_their_exact_content() {
        let r = release("1.0.0");
        let a = r.artifact_for(Platform::LinuxX86_64).unwrap();
        assert!(a.matches(&[1; 32], 10_000));
        assert!(!a.matches(&[1; 32], 10_001));
        assert!(!a.matches(&[3; 32], 10_000));
        assert!(r.artifact_for(Platform::WindowsX86_64).is_none());
        assert!(protocol_compatible(1, 1) && !protocol_compatible(1, 2));
    }
}
