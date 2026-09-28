use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};

use crate::error::Result;

enum WatchCmd {
    Watch(PathBuf),
    Unwatch(PathBuf),
    Shutdown,
}

/// File-system watcher that lives on its own thread.
/// Add roots with [`WatchHandle::watch`]; drop the handle to stop.
pub struct WatchHandle {
    cmd_tx: Sender<WatchCmd>,
    events: Arc<Mutex<Receiver<PathBuf>>>,
}

impl WatchHandle {
    pub fn start() -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        thread::Builder::new()
            .name("pam-watch".into())
            .spawn(move || watch_thread(cmd_rx, event_tx))
            .map_err(crate::error::Error::from)?;

        Ok(Self {
            cmd_tx,
            events: Arc::new(Mutex::new(event_rx)),
        })
    }

    pub fn events(&self) -> Arc<Mutex<Receiver<PathBuf>>> {
        self.events.clone()
    }

    pub fn watch(&self, root: PathBuf) {
        let _ = self.cmd_tx.send(WatchCmd::Watch(root));
    }

    pub fn unwatch(&self, root: PathBuf) {
        let _ = self.cmd_tx.send(WatchCmd::Unwatch(root));
    }

    pub fn drain(&self, wait: Duration) -> Vec<PathBuf> {
        let rx = self.events.lock().unwrap();
        drain_debounced(&rx, wait)
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WatchCmd::Shutdown);
    }
}

fn watch_thread(cmd_rx: Receiver<WatchCmd>, event_tx: Sender<PathBuf>) {
    let mut watcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                for path in event.paths {
                    let _ = event_tx.send(path);
                }
            }
        }) {
            Ok(w) => w,
            Err(_) => return,
        };

    let mut watched = std::collections::HashSet::new();
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            WatchCmd::Watch(root) => {
                if !root.exists() || !watched.insert(root.clone()) {
                    continue;
                }
                if watcher.watch(&root, RecursiveMode::Recursive).is_err() {
                    watched.remove(&root);
                }
            }
            WatchCmd::Unwatch(root) => {
                if watched.remove(&root) {
                    let _ = watcher.unwatch(&root);
                }
            }
            WatchCmd::Shutdown => break,
        }
    }
}

pub fn drain_debounced(rx: &Receiver<PathBuf>, wait: Duration) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    match rx.recv_timeout(wait) {
        Ok(first) => paths.push(first),
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return paths,
    }
    while let Ok(p) = rx.try_recv() {
        paths.push(p);
    }
    paths.sort();
    paths.dedup();
    paths
}

pub fn affected_library<'a>(
    path: &Path,
    libraries: &'a [crate::catalog::Library],
) -> Option<&'a crate::catalog::Library> {
    libraries
        .iter()
        .filter(|lib| path.starts_with(&lib.root_path))
        .max_by_key(|lib| lib.root_path.as_os_str().len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    #[test]
    fn watch_sees_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let handle = WatchHandle::start().unwrap();
        handle.watch(dir.path().to_path_buf());
        thread::sleep(Duration::from_millis(80));
        fs::write(dir.path().join("fresh.stl"), b"solid x\nendsolid x\n").unwrap();
        let mut found = false;
        for _ in 0..25 {
            let events = handle.drain(Duration::from_millis(80));
            if events.iter().any(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n == "fresh.stl")
            }) {
                found = true;
                break;
            }
        }
        assert!(found, "watcher should observe the new stl");
    }

    #[test]
    fn drain_debounced_timeout_is_empty() {
        let (_tx, rx) = mpsc::channel();
        let paths = drain_debounced(&rx, Duration::from_millis(5));
        assert!(paths.is_empty());
    }

    #[test]
    fn drain_debounced_sorts_and_dedups() {
        let (tx, rx) = mpsc::channel();
        tx.send(PathBuf::from("/b")).unwrap();
        tx.send(PathBuf::from("/a")).unwrap();
        tx.send(PathBuf::from("/a")).unwrap();
        let paths = drain_debounced(&rx, Duration::from_millis(20));
        assert_eq!(paths, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn affected_library_prefers_longest_root() {
        let libs = vec![
            crate::catalog::Library {
                id: 1,
                root_path: PathBuf::from("/lib"),
            },
            crate::catalog::Library {
                id: 2,
                root_path: PathBuf::from("/lib/nested"),
            },
        ];
        let hit = affected_library(Path::new("/lib/nested/a.stl"), &libs).unwrap();
        assert_eq!(hit.id, 2);
        assert!(affected_library(Path::new("/other/a.stl"), &libs).is_none());
    }
}
