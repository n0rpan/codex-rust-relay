use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub fn snapshot(root: &Path, exclude: Option<&str>) -> BTreeMap<PathBuf, (SystemTime, u64)> {
    fn visit(
        root: &Path,
        directory: &Path,
        exclude: Option<&str>,
        files: &mut BTreeMap<PathBuf, (SystemTime, u64)>,
    ) {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if exclude.is_some_and(|name| entry.file_name() == name) {
                continue;
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let mut digest = DefaultHasher::new();
            if metadata.is_dir() {
                visit(root, &path, None, files);
            } else if metadata.is_symlink() {
                fs::read_link(&path).unwrap().hash(&mut digest);
            } else if metadata.is_file() {
                let mut file = fs::File::open(&path).unwrap();
                let mut buffer = [0; 8192];
                loop {
                    let count = file.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    digest.write(&buffer[..count]);
                }
            }
            files.insert(
                path.strip_prefix(root).unwrap().to_owned(),
                (metadata.modified().unwrap(), digest.finish()),
            );
        }
    }

    let mut files = BTreeMap::new();
    visit(root, root, exclude, &mut files);
    files
}
