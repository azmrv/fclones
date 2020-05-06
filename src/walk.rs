use std::env::current_dir;
use std::fs::{DirEntry, FileType, read_link, ReadDir, symlink_metadata};
use std::hash::Hash;
use std::io;
use std::path::PathBuf;

use dashmap::DashSet;
use fasthash::{FastHasher, HasherExt, t1ha2::Hasher128};
use rayon::Scope;
use crate::glob::PathSelector;

#[derive(Debug)]
enum EntryType {
    File, Dir, SymLink, Other
}

/// Provides an abstraction over `PathBuf` and `DirEntry`
#[derive(Debug)]
struct Entry {
    tpe: EntryType,
    path: PathBuf,
}

impl Entry {

    pub fn new(file_type: FileType, path: PathBuf) -> Entry {
        if file_type.is_symlink() { Entry { tpe: EntryType::SymLink, path }}
        else if file_type.is_file() { Entry { tpe: EntryType::File, path }}
        else if file_type.is_dir() { Entry { tpe: EntryType::Dir, path }}
        else { Entry { tpe: EntryType::Other, path }}
    }

    pub fn from_path(path: PathBuf) -> io::Result<Entry> {
        symlink_metadata(&path).map(|meta| Entry::new(meta.file_type(), path))
    }

    pub fn from_dir_entry(dir_entry: DirEntry) -> io::Result<Entry> {
        let path = dir_entry.path();
        dir_entry.file_type().map(|ft| Entry::new(ft, path))
    }
}

/// Describes walk configuration.
/// Many walks can be initiated from the same instance.
pub struct Walk<'a> {
    pub base_dir: PathBuf,
    pub recursive: bool,
    pub skip_hidden: bool,
    pub follow_links: bool,
    pub path_selector: PathSelector,
    pub logger: &'a (dyn Fn(String) + Sync + Send),
}

/// Private shared state scoped to a single `run` invocation.
struct WalkState<F> {
    pub consumer: F,
    pub visited: DashSet<u128>
}

impl<'a> Walk<'a> {

    /// Creates a default walk with empty root dirs, no link following and logger set to sdterr
    pub fn new() -> Walk<'a> {
        Walk {
            base_dir: current_dir().unwrap_or_default(),
            recursive: true,
            skip_hidden: false,
            follow_links: false,
            path_selector: PathSelector::match_all(),
            logger: &|s| eprintln!("{}", s),
        }
    }

    /// Walks multiple directories recursively in parallel and sends found files to `consumer`.
    /// The `consumer` must be able to receive items from many threads.
    /// Inaccessible files are skipped, but errors are printed to stderr.
    /// The order of files is not specified and may be different every time.
    ///
    /// # Example
    /// ```
    /// use dff::walk::*;
    /// use std::fs::{File, create_dir_all, remove_dir_all, create_dir};
    /// use std::path::PathBuf;
    /// use std::sync::Mutex;
    ///
    /// let test_root = PathBuf::from("target/test/walk/");
    /// let dir1 = test_root.join("dir1");
    /// let dir2 = test_root.join("dir2");
    /// let dir3 = dir2.join("dir3");
    /// let file11 = dir1.join("file11.txt");
    /// let file12 = dir1.join("file12.txt");
    /// let file2 = dir2.join("file2.txt");
    /// let file3 = dir3.join("file3.txt");
    ///
    /// remove_dir_all(&test_root).ok();
    /// create_dir_all(&test_root).unwrap();
    /// create_dir(&dir1).unwrap();
    /// create_dir(&dir2).unwrap();
    /// create_dir(&dir3).unwrap();
    /// File::create(&file11).unwrap();
    /// File::create(&file12).unwrap();
    /// File::create(&file2).unwrap();
    /// File::create(&file3).unwrap();
    ///
    /// let receiver = Mutex::new(Vec::<PathBuf>::new());
    /// let mut walk = Walk::new();
    /// walk.run(vec![test_root.clone()], |path| {
    ///     receiver.lock().unwrap().push(path)
    /// });
    ///
    /// let mut results = receiver.lock().unwrap();
    /// results.sort();
    /// assert_eq!(*results, vec![file11, file12, file3, file2]);
    ///
    /// remove_dir_all(&test_root).ok();
    /// ```
    pub fn run<F>(&self, roots: Vec<PathBuf>, consumer: F)
        where F: Fn(PathBuf) + Sync + Send {

        let state = WalkState { consumer, visited: DashSet::new() };
        rayon::scope( |scope| {
            for p in roots {
                let p = self.absolute(p);
                scope.spawn(|scope| self.visit_path(p, scope, &state))
            }
        });
    }

    /// Visits path of any type (can be a symlink target, file or dir)
    fn visit_path<'s, 'w, F>(&'s self, path: PathBuf, scope: &Scope<'w>, state: &'w WalkState<F>)
        where F: Fn(PathBuf) + Sync + Send, 's: 'w
    {
        if self.path_selector.matches_dir(&path) {
            Entry::from_path(path.clone())
                .map_err(|e|
                    (self.logger)(format!("Failed to stat {}: {}", path.display(), e)))
                .into_iter()
                .for_each(|entry| self.visit_entry(entry, scope, state))
        }
    }

    /// Computes a 128-bit hash of a full path.
    /// We need 128-bits so that collisions are not a problem.
    /// Thanks to using a long hash we can be sure that if a path hash has been recorded
    /// we can
    fn path_hash(path: &PathBuf) -> u128 {
        let mut h = Hasher128::new();
        path.hash(&mut h);
        h.finish_ext()
    }

    /// Visits a path that was already converted to an `Entry` so the entry type is known.
    /// Faster than `visit_path` because it doesn't need to call `stat` internally.
    fn visit_entry<'s, 'w, F>(&'s self, entry: Entry, scope: &Scope<'w>, state: &'w WalkState<F>)
        where F: Fn(PathBuf) + Sync + Send, 's: 'w
    {
        // Skip hidden files
        if self.skip_hidden {
            if let Some(name) = entry.path.file_name() {
                if name.to_string_lossy().starts_with(".") {
                    return
                }
            }
        }

        // Skip already visited paths. We're checking only when follow_links is true,
        // because inserting into a shared hash set is costly.
        if self.follow_links && !state.visited.insert(Self::path_hash(&entry.path)) {
            return
        }

        match entry.tpe {
            EntryType::File =>
                self.visit_file(entry.path, state),
            EntryType::Dir =>
                self.visit_dir(&entry.path, scope, state),
            EntryType::SymLink =>
                self.visit_link(&entry.path, scope, state),
            EntryType::Other => {}
        }
    }

    /// If file matches selection criteria, sends it to the consumer
    fn visit_file<F>(&self, path: PathBuf, state: &WalkState<F>)
        where F: Fn(PathBuf) + Sync + Send
    {
        if self.path_selector.matches_full_path(&path) {
            (state.consumer)(self.relative(path))
        }
    }

    /// Resolves a symbolic link.
    /// If `follow_links` is set to false, does nothing.
    fn visit_link<'s, 'w, F>(&'s self, path: &PathBuf, scope: &Scope<'w>, state: &'w WalkState<F>)
        where F: Fn(PathBuf) + Sync + Send, 's: 'w
    {
        if self.follow_links {
            match self.resolve_link(&path) {
                Ok(target) =>
                    self.visit_path(target, scope, state),
                Err(e) =>
                    (self.logger)(format!("Failed to read link {}: {}", path.display(), e))
            }
        }
    }

    /// Reads the contents of the directory pointed to by `path`
    /// and recursively visits each child entry
    fn visit_dir<'s, 'w, F>(&'s self, path: &PathBuf, scope: &Scope<'w>, state: &'w WalkState<F>)
        where F: Fn(PathBuf) + Sync + Send, 's: 'w
    {
        if self.recursive && self.path_selector.matches_dir(path) {
            match std::fs::read_dir(&path) {
                Ok(rd) => {
                    for entry in Self::sorted_entries(rd) {
                        scope.spawn(move |s| self.visit_entry(entry, s, state))
                    }
                },
                Err(e) =>
                    (self.logger)(format!("Failed to read dir {}: {}", path.display(), e))
            }
        }
    }

    /// Sorts dir entries so that regular files are at the end.
    /// Because each worker's queue is a LIFO, the files would be picked up first and the
    /// dirs would be on the other side, amenable for stealing by other workers.
    fn sorted_entries(rd: ReadDir) -> impl Iterator<Item = Entry> {
        let mut files = vec![];
        let mut links = vec![];
        let mut dirs = vec![];
        rd.into_iter()
            .filter_map(|e| e.ok())
            .filter_map(|e| Entry::from_dir_entry(e).ok())
            .for_each(|e| match e.tpe {
                EntryType::File => files.push(e),
                EntryType::SymLink => links.push(e),
                EntryType::Dir => dirs.push(e),
                EntryType::Other => {}
            });
        dirs.into_iter().chain(links).chain(files)
    }

    /// Returns the absolute target path of a symbolic link
    fn resolve_link(&self, link: &PathBuf) -> io::Result<PathBuf> {
        let target = read_link(link)?;
        let resolved =
            if target.is_relative() {
                link.parent().unwrap().join(target)
            } else {
                target
            };
        Ok(self.absolute(resolved))
    }

    /// Returns absolute canonical path. Relative paths are resolved against `self.base_dir`.
    fn absolute(&self, path: PathBuf) -> PathBuf {
        if path.is_relative() {
            let abs_path = self.base_dir.join(path);
            abs_path.canonicalize().unwrap_or(abs_path)
        } else {
            path.canonicalize().unwrap_or(path)
        }
    }

    /// Returns a path relative to the base dir.
    fn relative(&self, path: PathBuf) -> PathBuf {
        path.strip_prefix(self.base_dir.clone())
            .map(|p| p.to_path_buf())
            .unwrap_or(path)
    }
}


#[cfg(test)]
mod test {
    use std::fs::{create_dir, create_dir_all, File, remove_dir_all};
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn list_files() {
        with_dir("target/test/walk/1/", |test_root| {
            let file1 = test_root.join("file1.txt");
            let file2 = test_root.join("file2.txt");
            File::create(&file1).unwrap();
            File::create(&file2).unwrap();
            let walk = Walk::new();
            assert_eq!(run_walk(walk, test_root.clone()), vec![file1, file2]);
        });
    }

    #[test]
    fn descend_into_nested_dirs() {
        with_dir("target/test/walk/2/", |test_root| {
            let dir = test_root.join("dir");
            create_dir(&dir).unwrap();
            let file = dir.join("file.txt");
            File::create(&file).unwrap();
            let walk = Walk::new();
            assert_eq!(run_walk(walk, test_root.clone()), vec![file]);
        });
    }

    #[test]
    #[cfg(unix)]
    fn follow_rel_file_sym_links() {
        with_dir("target/test/walk/3/", |test_root| {
            use std::os::unix::fs::symlink;
            let file = test_root.join("file.txt");
            let link = test_root.join("link");
            File::create(&file).unwrap();
            symlink(PathBuf::from("file.txt"), &link).unwrap(); // link -> file.txt
            let mut walk = Walk::new();
            walk.follow_links = true;
            assert_eq!(run_walk(walk, link), vec![file]);
        });
    }

    #[test]
    #[cfg(unix)]
    fn follow_rel_dir_sym_links() {
        with_dir("target/test/walk/4/", |test_root| {
            use std::os::unix::fs::symlink;
            let dir = test_root.join("dir");
            let link = test_root.join("link");
            let file = dir.join("file.txt");

            create_dir(&dir).unwrap();
            File::create(&file).unwrap();
            symlink(PathBuf::from("dir"), &link).unwrap(); // link -> dir

            let mut walk = Walk::new();
            walk.follow_links = true;
            assert_eq!(run_walk(walk, link), vec![file]);
        });
    }

    #[test]
    #[cfg(unix)]
    fn follow_abs_dir_sym_links() {
        with_dir("target/test/walk/5/", |test_root| {
            use std::os::unix::fs::symlink;
            let dir = test_root.join("dir");
            let link = test_root.join("link");
            let file = dir.join("file.txt");

            create_dir(&dir).unwrap();
            File::create(&file).unwrap();
            symlink(dir.canonicalize().unwrap(), &link).unwrap(); // link -> absolute path to dir

            let mut walk = Walk::new();
            walk.follow_links = true;
            assert_eq!(run_walk(walk, link), vec![file]);
        });
    }

    #[test]
    #[cfg(unix)]
    fn sym_link_cycles() {
        with_dir("target/test/walk/6/", |test_root| {
            use std::os::unix::fs::symlink;
            let dir = test_root.join("dir");
            let link = dir.join("link");
            let file = dir.join("file.txt");

            create_dir(&dir).unwrap();
            File::create(&file).unwrap();
            // create a link back to the top level, so a cycle is formed
            symlink(test_root.canonicalize().unwrap(), &link).unwrap();

            let mut walk = Walk::new();
            walk.follow_links = true;
            assert_eq!(run_walk(walk, test_root.clone()), vec![file]);
        });
    }

    #[test]
    fn skip_hidden() {
        with_dir("target/test/walk/7/", |test_root| {
            let hidden_dir = test_root.join(".dir");
            create_dir(&hidden_dir).unwrap();
            let hidden_file_1 = hidden_dir.join("file.txt");
            let hidden_file_2 = test_root.join(".file.txt");
            File::create(&hidden_file_1).unwrap();
            File::create(&hidden_file_2).unwrap();
            let mut walk = Walk::new();
            walk.skip_hidden = true;
            assert_eq!(run_walk(walk, test_root.clone()), Vec::<PathBuf>::new());
        });
    }

    fn with_dir<F>(test_root: &str, test_code: F)
        where F: FnOnce(&PathBuf)
    {
        let test_root = PathBuf::from(test_root);
        remove_dir_all(&test_root).ok();
        create_dir_all(&test_root).unwrap();
        (test_code)(&test_root);
        remove_dir_all(&test_root).unwrap();
    }

    fn run_walk(walk: Walk, root: PathBuf) -> Vec<PathBuf> {
        let results = Mutex::new(Vec::new());
        walk.run(vec![root], |path|
            results.lock().unwrap().push(path));

        let mut results = results.into_inner().unwrap();
        results.sort();
        results
    }
}