//! Library folders: adding, scanning, watching for changes, and the
//! thumbnail queue that runs after each scan.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_kit::*;
use pam_core::catalog::{Library, ThumbState};
use pam_core::paths::thumb_path;
use pam_core::watch::affected_library;

use crate::i18n::{self, Key};
use crate::jobs;

use super::{UiStatus, Workspace};

/// Minimum gap between grid refreshes while thumbnails are being generated.
const THUMB_RELOAD_INTERVAL: Duration = Duration::from_millis(500);

impl Workspace {
    pub(super) fn scan_all(&mut self, cx: &mut Context<Self>) {
        self.scan_libraries(self.libraries.clone(), cx);
    }

    fn scan_libraries(&mut self, libs: Vec<Library>, cx: &mut Context<Self>) {
        if libs.is_empty() {
            return;
        }
        let catalog = self.catalog.clone();
        self.scanning = true;
        self.scan_done = 0;
        self.scan_total = 0;
        self.status = UiStatus::Scanning;
        cx.notify();

        let progress = Arc::new(Mutex::new((0u32, 0u32)));
        let progress_scan = progress.clone();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            for lib in &libs {
                let _ = catalog.scan_library_progress(lib, |done, total| {
                    *progress_scan.lock().unwrap() = (done, total);
                });
            }
            // The thumb cache can be purged behind our back; regenerate what's gone.
            let _ = catalog.requeue_missing_thumbs(|hex| thumb_path(hex).is_file());
            let _ = done_tx.send(());
        });

        cx.spawn(async move |this, cx| {
            loop {
                match done_rx.try_recv() {
                    Ok(()) | Err(TryRecvError::Disconnected) => break,
                    Err(TryRecvError::Empty) => {
                        let snap = *progress.lock().unwrap();
                        this.update(cx, |this, cx| {
                            this.scan_done = snap.0;
                            this.scan_total = snap.1;
                            cx.notify();
                        })
                        .ok();
                        cx.background_executor()
                            .timer(Duration::from_millis(100))
                            .await;
                    }
                }
            }
            this.update(cx, |this, cx| {
                this.scanning = false;
                this.status = UiStatus::Idle;
                this.reload(cx);
                this.reload_folders();
                this.watch_libraries();
                this.kick_thumbs(cx);
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn add_folder(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some(i18n::t(Key::AddFolder).into()),
        });
        cx.spawn(async move |this, cx| {
            let paths = prompt
                .await
                .ok()
                .and_then(Result::ok)
                .flatten()
                .unwrap_or_default();
            this.update(cx, |this, cx| {
                let mut added = Vec::new();
                for path in paths {
                    if let Ok(lib) = this.catalog.add_library(&path) {
                        if let Some(watch) = &this.watch {
                            watch.watch(lib.root_path.clone());
                        }
                        added.push(lib);
                    }
                }
                this.libraries.extend(added.iter().cloned());
                this.scan_libraries(added, cx);
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn add_folder_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        let root = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| path.to_path_buf())
        };
        if let Ok(lib) = self.catalog.add_library(&root) {
            if let Some(watch) = &self.watch {
                watch.watch(lib.root_path.clone());
            }
            self.libraries.push(lib.clone());
            self.scan_libraries(vec![lib], cx);
        }
    }

    fn kick_thumbs(&mut self, cx: &mut Context<Self>) {
        self.thumbs_wanted = true;
        if self.thumbs_running {
            return;
        }
        self.thumbs_running = true;
        self.thumb_done = 0;
        self.thumb_total = self.catalog.pending_count().unwrap_or(0);
        let catalog = self.catalog.clone();
        let hashed = Arc::new(AtomicU32::new(0));
        let done = Arc::new(AtomicU32::new(0));
        cx.spawn(async move |this, cx| loop {
            this.update(cx, |this, _cx| {
                this.thumbs_wanted = false;
            })
            .ok();
            let (catalog2, hashed2, done2) = (catalog.clone(), hashed.clone(), done.clone());
            let (done_tx, done_rx) = mpsc::channel();
            std::thread::spawn(move || {
                jobs::drain(&catalog2, &hashed2, &done2);
                let _ = done_tx.send(());
            });
            // New hashes change the duplicates count, new thumbnails the grid.
            let progress = || (hashed.load(Ordering::Relaxed), done.load(Ordering::Relaxed));
            let mut reloaded = progress();
            let mut last_reload = Instant::now();
            loop {
                let finished = !matches!(done_rx.try_recv(), Err(TryRecvError::Empty));
                let now = progress();
                let refresh =
                    now != reloaded && (finished || last_reload.elapsed() >= THUMB_RELOAD_INTERVAL);
                this.update(cx, |this, cx| {
                    let n = now.1;
                    let progressed = this.thumb_done != n;
                    this.thumb_done = n;
                    this.thumb_total = this.thumb_total.max(n);
                    if refresh {
                        this.reload(cx);
                    } else if progressed {
                        cx.notify();
                    }
                })
                .ok();
                if finished {
                    break;
                }
                if refresh {
                    reloaded = now;
                    last_reload = Instant::now();
                }
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
            let stop = this
                .update(cx, |this, cx| {
                    if this.thumbs_wanted {
                        this.thumb_total = this
                            .thumb_total
                            .max(this.thumb_done + this.catalog.pending_count().unwrap_or(0));
                        false
                    } else {
                        this.thumbs_running = false;
                        this.reload(cx);
                        true
                    }
                })
                .unwrap_or(true);
            if stop {
                break;
            }
        })
        .detach();
    }

    pub(super) fn regenerate_thumbs(&mut self, ids: &[i64], cx: &mut Context<Self>) {
        for &id in ids {
            let _ = self.catalog.set_thumb_state(id, ThumbState::Pending, None);
        }
        self.reload(cx);
        self.kick_thumbs(cx);
    }

    pub(super) fn watch_libraries(&mut self) {
        let Some(watch) = &self.watch else {
            return;
        };
        for lib in &self.libraries {
            watch.watch(lib.root_path.clone());
        }
    }

    pub(super) fn start_watch_loop(&mut self, cx: &mut Context<Self>) {
        let Some(events) = self.watch.as_ref().map(|w| w.events()) else {
            return;
        };
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            let mut changed = {
                let rx = events.lock().unwrap();
                let mut paths = Vec::new();
                while let Ok(p) = rx.try_recv() {
                    paths.push(p);
                }
                paths
            };
            if changed.is_empty() {
                continue;
            }
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            {
                let rx = events.lock().unwrap();
                while let Ok(p) = rx.try_recv() {
                    changed.push(p);
                }
            }
            this.update(cx, |this, cx| {
                let mut libs = Vec::new();
                for path in &changed {
                    if let Some(lib) = affected_library(path, &this.libraries) {
                        if !libs.iter().any(|l: &Library| l.id == lib.id) {
                            libs.push(lib.clone());
                        }
                    }
                }
                if !libs.is_empty() {
                    this.scan_libraries(libs, cx);
                }
            })
            .ok();
        })
        .detach();
    }
}
