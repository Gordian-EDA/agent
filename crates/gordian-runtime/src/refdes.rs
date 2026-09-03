//! Durable refdes reservations, so parallel callers never mint the same `R12`.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// Largest block of references one call may reserve.
pub const MAX_RESERVATION: u32 = 200;

/// One reserved block of references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    /// Alphabetic reference prefix, such as `R`, `C`, `U` or `TP`.
    pub prefix: String,
    /// First reserved number.
    pub start: u32,
    /// How many consecutive numbers the block covers.
    pub count: u32,
    /// The fully formed references, `start` through `start + count - 1`.
    pub refs: Vec<String>,
}

/// Reference ranges reserved in one project, so parallel callers never mint the
/// same refdes.
///
/// Backed by `<project>/.gordian/reserved_refs.json`. The mutex serialises
/// callers sharing this store within one process only; it is not a cross-process
/// file lock, so two separate processes over the same project can still race.
pub struct Reservations {
    project: PathBuf,
    lock: Mutex<()>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    reservations: Vec<Reservation>,
}

impl Reservations {
    /// Opens the reservation store of one project directory.
    pub fn for_project(project: PathBuf) -> Self {
        Self {
            project,
            lock: Mutex::new(()),
        }
    }

    /// Reserves the next `count` free numbers for `prefix`, recording them durably.
    ///
    /// `taken` is every reference already in use in the design, so a reservation
    /// never collides with parts that exist but were never reserved. The block is
    /// contiguous: the lowest run of `count` numbers free of both `taken` and
    /// every earlier reservation for `prefix`.
    pub fn reserve(
        &self,
        prefix: &str,
        count: u32,
        taken: &BTreeSet<String>,
    ) -> Result<Reservation> {
        validate(prefix, count)?;
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("refdes reservation lock poisoned"))?;
        let mut store = self.read()?;
        let used = used_numbers(prefix, taken_numbers(taken, prefix), &store);
        let start = first_free_run(&used, count);
        let reservation = Reservation {
            prefix: prefix.to_owned(),
            start,
            count,
            refs: (start..start + count)
                .map(|number| format!("{prefix}{number}"))
                .collect(),
        };
        store.reservations.push(reservation.clone());
        let json = serde_json::to_vec_pretty(&store)?;
        let path = self.path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        crate::workspace::atomic_write(&path, &json)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(reservation)
    }

    /// Every reference any reservation holds — what a designator allocator must
    /// step over so a part it mints does not take a name another caller was
    /// promised.
    pub fn reserved(&self) -> Result<BTreeSet<String>> {
        Ok(self
            .all()?
            .into_iter()
            .flat_map(|reservation| reservation.refs)
            .collect())
    }

    /// Every reservation the project has recorded.
    pub fn all(&self) -> Result<Vec<Reservation>> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("refdes reservation lock poisoned"))?;
        Ok(self.read()?.reservations)
    }

    fn path(&self) -> PathBuf {
        self.project.join(".gordian/reserved_refs.json")
    }

    fn read(&self) -> Result<Store> {
        let path = self.path();
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }
}

fn validate(prefix: &str, count: u32) -> Result<()> {
    if prefix.is_empty()
        || !prefix
            .chars()
            .all(|character| character.is_ascii_alphabetic())
    {
        bail!("reference prefix must be alphabetic, such as R, C, U or TP: got {prefix:?}");
    }
    if count == 0 || count > MAX_RESERVATION {
        bail!("reservation count must be between 1 and {MAX_RESERVATION}: got {count}");
    }
    Ok(())
}

/// The numbers of `taken` references that carry exactly this prefix.
fn taken_numbers(taken: &BTreeSet<String>, prefix: &str) -> BTreeSet<u32> {
    taken
        .iter()
        .filter_map(|reference| {
            let digits = reference.strip_prefix(prefix)?;
            (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
                .then(|| digits.parse().ok())
                .flatten()
        })
        .collect()
}

fn used_numbers(prefix: &str, mut used: BTreeSet<u32>, store: &Store) -> BTreeSet<u32> {
    for reservation in store
        .reservations
        .iter()
        .filter(|reservation| reservation.prefix == prefix)
    {
        used.extend(reservation.start..reservation.start + reservation.count);
    }
    used
}

/// The lowest `start >= 1` whose whole `count`-wide window is free.
fn first_free_run(used: &BTreeSet<u32>, count: u32) -> u32 {
    let mut start = 1;
    for &number in used {
        if number < start {
            continue;
        }
        if number - start >= count {
            break;
        }
        start = number + 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn reserved_numbers_skip_taken_references_and_earlier_reservations() {
        let project = tempfile::tempdir().unwrap();
        let store = Reservations::for_project(project.path().to_path_buf());
        let taken = refs(&["R1", "R2", "C1", "RV9", "R"]);

        let first = store.reserve("R", 2, &taken).unwrap();
        assert_eq!(first.start, 3);
        assert_eq!(first.refs, ["R3", "R4"]);

        let second = store.reserve("R", 3, &taken).unwrap();
        assert_eq!(second.refs, ["R5", "R6", "R7"]);

        let other_prefix = store.reserve("C", 1, &taken).unwrap();
        assert_eq!(other_prefix.refs, ["C2"], "C numbering ignores R");

        assert_eq!(store.all().unwrap(), [first, second, other_prefix]);
        assert_eq!(
            store.reserved().unwrap(),
            refs(&["C2", "R3", "R4", "R5", "R6", "R7"])
        );
    }

    #[test]
    fn a_reservation_is_durable_across_a_new_store() {
        let project = tempfile::tempdir().unwrap();
        let taken = BTreeSet::new();
        let first = Reservations::for_project(project.path().to_path_buf())
            .reserve("U", 2, &taken)
            .unwrap();
        assert_eq!(first.refs, ["U1", "U2"]);

        let reopened = Reservations::for_project(project.path().to_path_buf());
        assert_eq!(reopened.all().unwrap(), [first]);
        assert_eq!(reopened.reserve("U", 1, &taken).unwrap().refs, ["U3"]);
    }

    #[test]
    fn a_bad_prefix_or_count_is_rejected() {
        let project = tempfile::tempdir().unwrap();
        let store = Reservations::for_project(project.path().to_path_buf());
        let taken = BTreeSet::new();

        for prefix in ["", "R1", "r-", " U"] {
            let message = store.reserve(prefix, 1, &taken).unwrap_err().to_string();
            assert!(message.contains("alphabetic"), "{message}");
        }
        for count in [0, MAX_RESERVATION + 1] {
            let message = store.reserve("R", count, &taken).unwrap_err().to_string();
            assert!(message.contains("between 1 and"), "{message}");
        }
        assert!(store.all().unwrap().is_empty());
    }
}
