//! Atomic local JSON preferences shared by the desktop and the headless host.
use serde::Serialize;
use std::{
    fs,
    io::{self, Write},
    path::Path,
};
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let dir = path.parent().unwrap();
    fs::create_dir_all(dir)?;
    let mut temp = tempfile::Builder::new()
        .prefix(".desktop-")
        .tempfile_in(dir)?;
    serde_json::to_writer(&mut temp, value)?;
    temp.flush()?;
    temp.persist(path)?;
    Ok(())
}

pub fn update_preference_at<T: serde::de::DeserializeOwned + Serialize>(
    path: &Path,
    initial: impl FnOnce() -> T,
    edit: impl FnOnce(T) -> T,
) -> io::Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    lock.lock()?;
    let current = match fs::File::open(path) {
        Ok(file) => serde_json::from_reader(file)?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => initial(),
        Err(err) => return Err(err),
    };
    write_json(path, &edit(current))
}
