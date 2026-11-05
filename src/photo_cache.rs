//! Small LRU cache of decoded full-size photos for the strip export. The strips move from top to
//! bottom, so neighbouring strips need mostly the same photos; keeping them avoids decoding the
//! same JPEG again for every strip. The cache never holds more than `cap` bytes, except for the
//! one photo that was asked for last (it must be usable even if it alone is larger).

use image::RgbImage;
use std::sync::Arc;

pub struct PhotoCache {
    cap: u64,
    used: u64,
    /// least recently used first
    items: Vec<(usize, Arc<RgbImage>)>,
    pub hits: u64,
    pub misses: u64,
}

fn bytes_of(img: &RgbImage) -> u64 {
    img.width() as u64 * img.height() as u64 * 3
}

impl PhotoCache {
    pub fn new(cap: u64) -> Self {
        PhotoCache { cap, used: 0, items: Vec::new(), hits: 0, misses: 0 }
    }

    /// Bytes currently held.
    pub fn used(&self) -> u64 {
        self.used
    }

    /// The photo `i`, from the cache or decoded by `load`.
    pub fn get_or_load(&mut self, i: usize, load: impl FnOnce() -> Result<Arc<RgbImage>, String>) -> Result<Arc<RgbImage>, String> {
        if let Some(pos) = self.items.iter().position(|(k, _)| *k == i) {
            self.hits += 1;
            let it = self.items.remove(pos);
            let img = it.1.clone();
            self.items.push(it);
            return Ok(img);
        }
        self.misses += 1;
        let img = load()?;
        self.used += bytes_of(&img);
        self.items.push((i, img.clone()));
        // drop the least recently used photos until the cache fits again (the new one stays)
        while self.used > self.cap && self.items.len() > 1 {
            let (_, old) = self.items.remove(0);
            self.used -= bytes_of(&old);
        }
        Ok(img)
    }

    /// Changes the size limit; photos beyond it are dropped, least recently used first.
    pub fn set_capacity(&mut self, cap: u64) {
        self.cap = cap;
        while self.used > self.cap && !self.items.is_empty() {
            let (_, old) = self.items.remove(0);
            self.used -= bytes_of(&old);
        }
    }

    /// Frees everything (after the export).
    pub fn clear(&mut self) {
        self.items.clear();
        self.used = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img() -> Arc<RgbImage> {
        Arc::new(RgbImage::new(10, 10)) // 300 bytes
    }

    #[test]
    fn a_smaller_capacity_drops_the_oldest_photos() {
        let mut c = PhotoCache::new(10_000);
        for k in 0..3 {
            c.get_or_load(k, || Ok(img())).unwrap();
        }
        assert_eq!(c.used(), 900);
        c.set_capacity(600);
        assert_eq!(c.used(), 600);
        c.get_or_load(2, || Ok(img())).unwrap();
        assert_eq!(c.hits, 1, "the newest photo is still cached");
    }

    #[test]
    fn a_cached_photo_is_not_loaded_twice() {
        let mut c = PhotoCache::new(10_000);
        let mut loads = 0;
        for _ in 0..3 {
            c.get_or_load(1, || {
                loads += 1;
                Ok(img())
            })
            .unwrap();
        }
        assert_eq!((loads, c.hits, c.misses), (1, 2, 1));
    }

    #[test]
    fn the_least_recently_used_photo_goes_first_and_the_limit_holds() {
        let mut c = PhotoCache::new(700); // two photos fit
        for i in [1, 2] {
            c.get_or_load(i, || Ok(img())).unwrap();
        }
        c.get_or_load(1, || Ok(img())).unwrap(); // 1 is now newer than 2
        c.get_or_load(3, || Ok(img())).unwrap(); // evicts 2
        assert!(c.used() <= 700);
        let mut reloaded = Vec::new();
        for i in [1, 3, 2] {
            c.get_or_load(i, || {
                reloaded.push(i);
                Ok(img())
            })
            .unwrap();
        }
        assert_eq!(reloaded, vec![2]);
    }

    #[test]
    fn a_photo_larger_than_the_cache_is_still_returned() {
        let mut c = PhotoCache::new(10);
        let a = c.get_or_load(0, || Ok(img())).unwrap();
        assert_eq!(a.width(), 10);
        c.get_or_load(1, || Ok(img())).unwrap();
        assert!(c.used() <= 300, "only the newest photo stays");
    }

    #[test]
    fn load_errors_are_passed_on_and_not_cached() {
        let mut c = PhotoCache::new(1000);
        assert_eq!(c.get_or_load(0, || Err("broken".into())).unwrap_err(), "broken");
        assert_eq!(c.used(), 0);
        c.get_or_load(0, || Ok(img())).unwrap();
        c.clear();
        assert_eq!(c.used(), 0);
    }
}
