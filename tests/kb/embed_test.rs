//! Tests for the KB embedding layer (`src/kb/embed/`).
//!
//! Task 3.1 covers the LRU cache port. Tasks 3.2 / 3.3 will append provider
//! tests (wiremock-backed) to this file rather than splitting per-provider.

use std::thread::sleep;
use std::time::Duration;

use rantaiclaw::kb::embed::cache::LruCache;

#[test]
fn lru_evicts_oldest_at_capacity() {
    let mut c: LruCache<String, u32> = LruCache::new(2, None);
    c.put("a".into(), 1);
    c.put("b".into(), 2);
    c.put("c".into(), 3);
    assert_eq!(c.get(&"a".into()), None, "oldest entry evicted");
    assert_eq!(c.get(&"b".into()), Some(2));
    assert_eq!(c.get(&"c".into()), Some(3));
    assert_eq!(c.len(), 2);
}

#[test]
fn lru_ttl_evicts_expired() {
    let mut c: LruCache<String, u32> = LruCache::new(8, Some(Duration::from_millis(50)));
    c.put("a".into(), 1);
    sleep(Duration::from_millis(80));
    assert_eq!(c.get(&"a".into()), None, "TTL-expired entry returns miss");
    assert!(c.is_empty(), "expired entry lazily evicted on probe");
}

#[test]
fn lru_get_promotes_to_recent() {
    let mut c: LruCache<String, u32> = LruCache::new(2, None);
    c.put("a".into(), 1);
    c.put("b".into(), 2);
    // Touching `a` promotes it; subsequent insert evicts `b` (now oldest).
    assert_eq!(c.get(&"a".into()), Some(1));
    c.put("c".into(), 3);
    assert_eq!(c.get(&"a".into()), Some(1), "promoted entry survives");
    assert_eq!(c.get(&"b".into()), None, "demoted entry evicted");
    assert_eq!(c.get(&"c".into()), Some(3));
}

#[test]
fn lru_put_overwrites_existing_value() {
    let mut c: LruCache<String, u32> = LruCache::new(4, None);
    c.put("a".into(), 1);
    c.put("a".into(), 2);
    assert_eq!(c.get(&"a".into()), Some(2));
    assert_eq!(c.len(), 1, "overwrite does not grow length");
}
