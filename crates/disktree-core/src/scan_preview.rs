//! Bounded, non-actionable overview; never a replacement filesystem tree.
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

/// A measured contribution, not the total size of an unfinished directory.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub bytes: u64,
    pub files: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Preview {
    pub entries: Vec<Entry>,
    pub directory: Option<std::path::PathBuf>,
}

/// Keep top-level arrival order, rather than re-ranking the view every tick.
/// Extra names share one overflow bucket; no tree or per-file copy is kept.
#[derive(Debug, Default)]
pub struct Builder {
    index: FxHashMap<String, usize>,
    entries: Vec<Entry>,
    directory: Option<std::path::PathBuf>,
}

impl Builder {
    pub fn add(&mut self, name: Option<&str>, bytes: u64, files: u64) {
        const LIMIT: usize = 128;
        let key = name.unwrap_or("");
        let index = if let Some(index) = self.index.get(key) {
            *index
        } else if self.index.len() < LIMIT {
            let index = self.entries.len();
            self.index.insert(key.into(), index);
            self.entries.push(Entry {
                name: name.unwrap_or("Files in root").into(),
                ..Entry::default()
            });
            index
        } else {
            if self.entries.len() == LIMIT {
                self.entries.push(Entry {
                    name: "Other indexed paths".into(),
                    ..Entry::default()
                });
            }
            LIMIT
        };
        let entry = &mut self.entries[index];
        entry.bytes = entry.bytes.saturating_add(bytes);
        entry.files = entry.files.saturating_add(files);
    }

    pub fn directory(&mut self, path: &std::path::Path) {
        path.clone_into(self.directory.get_or_insert_default());
    }

    #[must_use]
    pub fn snapshot(&self) -> Preview {
        Preview {
            entries: self.entries.clone(),
            directory: self.directory.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_buckets_preserve_all_weight_and_arrival_order() {
        let mut builder = Builder::default();
        for index in 0..1000 {
            builder.add(Some(&format!("dir-{index}")), 2, 1);
        }
        builder.add(Some("dir-0"), 3, 4);
        let preview = builder.snapshot();
        assert_eq!(preview.entries.len(), 129);
        assert_eq!(preview.entries[0].name, "dir-0");
        assert_eq!(preview.entries[0].bytes, 5);
        assert_eq!(preview.entries.iter().map(|e| e.bytes).sum::<u64>(), 2003);
        assert_eq!(preview.entries.iter().map(|e| e.files).sum::<u64>(), 1004);
    }
}
