//! The disk cache behind `GET /api/v1/media`.
//!
//! Article images are immutable in practice — a publisher who edits an article replaces its body,
//! not the bytes behind an old `<img>` — so a copy fetched once can be served forever. What the
//! cache buys is durability rather than speed: without it every cold browser view is a fresh
//! request to a third-party host, and a host that is slow, rate-limiting or briefly down turns
//! into a broken image for a reader who saw the same picture fine last week.
//!
//! Keys are a hash of the URL rather than any part of it: they become file names, so they must not
//! carry slashes, query strings or the URL's own length. MD5 is a bucket address here, not a
//! security property — the worst a collision costs is one image served in another's place.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use md5::{Digest, Md5};

/// One cached image: the bytes, and the content type they were validated and served with.
pub struct Cached {
    pub body: Vec<u8>,
    pub mime: String,
}

/// Walking the directory to enforce the ceiling costs a syscall per image, which is not a price
/// worth paying on every put; every this-many puts is invisible and keeps the bound honest.
const PRUNE_EVERY: u64 = 64;

static PUTS: AtomicU64 = AtomicU64::new(0);

fn paths(root: &Path, url: &str) -> (PathBuf, PathBuf) {
    let key = hex::encode(Md5::digest(url.as_bytes()));
    (root.join(&key), root.join(format!("{key}.mime")))
}

/// The copy of `url` this server fetched successfully before, if any.
pub fn get(root: &Path, url: &str) -> Option<Cached> {
    let (body, mime) = paths(root, url);
    Some(Cached {
        body: fs::read(body).ok()?,
        mime: fs::read_to_string(mime).ok()?,
    })
}

/// Stores a successfully fetched image and trims the cache when it is over budget.
///
/// Every failure here is swallowed: a cache that cannot write is a cache miss, and a miss is
/// served by going upstream, which is what would have happened with no cache at all.
pub fn put(root: &Path, url: &str, body: &[u8], mime: &str, max_bytes: u64) {
    let (body_path, mime_path) = paths(root, url);
    if fs::create_dir_all(root).is_err() {
        return;
    }
    let part = body_path.with_extension("part");
    // Rename is atomic, so a concurrent read sees either the old copy or the whole new one, never
    // a half-written image that a browser would then cache for a week.
    if fs::write(&part, body).is_err() || fs::rename(&part, &body_path).is_err() {
        let _ = fs::remove_file(&part);
        return;
    }
    let _ = fs::write(&mime_path, mime);
    if PUTS.fetch_add(1, Ordering::Relaxed) % PRUNE_EVERY == PRUNE_EVERY - 1 {
        prune(root, max_bytes);
    }
}

/// Deletes the oldest images, sidecars and scraps with them, until the cache is under budget.
fn prune(root: &Path, max_bytes: u64) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut images: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    let mut total = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        // Sidecars and interrupted writes are removed with their image or on the next put; only
        // complete images are weighed.
        if path.extension().is_some_and(|e| e == "mime" || e == "part") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        total += meta.len();
        images.push((
            meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            meta.len(),
            path,
        ));
    }
    if total <= max_bytes {
        return;
    }
    images.sort_by_key(|image| image.0);
    for (_, len, path) in images {
        if total <= max_bytes {
            break;
        }
        let sidecar = path.with_extension("mime");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&sidecar);
        total -= len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 1, 2, 3];

    fn root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("frss-media-cache-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("cache dir");
        dir
    }

    #[test]
    fn a_put_round_trips_through_get() {
        let dir = root("roundtrip");
        assert!(get(&dir, "https://img.example/a.png").is_none());
        put(&dir, "https://img.example/a.png", PNG, "image/png", 1024);
        let cached = get(&dir, "https://img.example/a.png").expect("cached");
        assert_eq!(cached.body, PNG);
        assert_eq!(cached.mime, "image/png");
        // A different URL is a different key, not a collision with the first.
        assert!(get(&dir, "https://img.example/b.png").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cache_that_cannot_be_written_is_a_miss_not_a_failure() {
        // A regular file where the cache directory would go: `create_dir_all` fails, and the put
        // has to degrade to "not cached" rather than propagating an error into a request.
        let blocker = std::env::temp_dir().join("frss-media-cache-blocker");
        let _ = fs::remove_dir_all(&blocker);
        let _ = fs::remove_file(&blocker);
        fs::write(&blocker, b"x").expect("blocker file");
        let under = blocker.join("cache");
        put(&under, "https://img.example/a.png", PNG, "image/png", 1024);
        assert!(get(&under, "https://img.example/a.png").is_none());
        let _ = fs::remove_file(&blocker);
    }

    #[test]
    fn pruning_keeps_the_cache_under_the_ceiling() {
        let dir = root("prune");
        // Three four-byte images and a ceiling of eight: whichever the mtimes pick, at most two
        // may survive and the total must fit.
        for name in ["a", "b", "c"] {
            put(
                &dir,
                &format!("https://img.example/{name}.png"),
                b"aaaa",
                "image/png",
                1024,
            );
        }
        prune(&dir, 8);
        let left: Vec<_> = fs::read_dir(&dir)
            .expect("dir")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_none())
            .collect();
        assert!(left.len() <= 2, "pruned to the ceiling: {left:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}
