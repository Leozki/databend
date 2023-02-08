// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::hash::BuildHasher;
use std::sync::Arc;

use common_cache::BytesMeter;
use common_cache::Cache;
use common_cache::Count;
use common_cache::CountableMeter;
use common_cache::DefaultHashBuilder;
use common_cache::LruCache;
use parking_lot::RwLock;

pub type ItemCache<V> = LruCache<String, Arc<V>, DefaultHashBuilder, Count>;
pub type BytesCache = LruCache<String, Arc<Vec<u8>>, DefaultHashBuilder, BytesMeter>;

pub type InMemoryItemCacheHolder<T> = Arc<RwLock<ItemCache<T>>>;
pub type InMemoryBytesCacheHolder = Arc<RwLock<BytesCache>>;

pub struct InMemoryCacheBuilder;
impl InMemoryCacheBuilder {
    pub fn new_item_cache<V>(capacity: u64) -> InMemoryItemCacheHolder<V> {
        let cache = LruCache::new(capacity);
        Arc::new(RwLock::new(cache))
    }

    pub fn new_bytes_cache(capacity: u64) -> InMemoryBytesCacheHolder {
        let cache =
            LruCache::with_meter_and_hasher(capacity, BytesMeter, DefaultHashBuilder::new());
        Arc::new(RwLock::new(cache))
    }
}

// default impls
mod impls {
    use std::sync::Arc;

    use parking_lot::RwLock;

    use super::*;
    use crate::cache::CacheAccessor;

    // Wrap a Cache with RwLock, and impl CacheAccessor for it
    impl<V, C, S, M> CacheAccessor<String, V, S, M> for Arc<RwLock<C>>
    where
        C: Cache<String, Arc<V>, S, M>,
        M: CountableMeter<String, Arc<V>>,
        S: BuildHasher,
    {
        fn get<Q: AsRef<str>>(&self, k: Q) -> Option<Arc<V>> {
            let mut guard = self.write();
            guard.get(k.as_ref()).cloned()
        }

        fn put(&self, k: String, v: Arc<V>) {
            let mut guard = self.write();
            guard.put(k, v);
        }

        fn evict(&self, k: &str) -> bool {
            let mut guard = self.write();
            guard.pop(k).is_some()
        }

        fn contains_key(&self, k: &str) -> bool {
            let guard = self.read();
            guard.contains(k)
        }
    }

    // Wrap an Option<CacheAccessor>, and impl CacheAccessor for it
    impl<V, C, S, M> CacheAccessor<String, V, S, M> for Option<C>
    where
        C: CacheAccessor<String, V, S, M>,
        M: CountableMeter<String, Arc<V>>,
        S: BuildHasher,
    {
        fn get<Q: AsRef<str>>(&self, k: Q) -> Option<Arc<V>> {
            self.as_ref().and_then(|cache| cache.get(k))
        }

        fn put(&self, k: String, v: Arc<V>) {
            if let Some(cache) = self {
                cache.put(k, v);
            }
        }

        fn evict(&self, k: &str) -> bool {
            if let Some(cache) = self {
                cache.evict(k)
            } else {
                false
            }
        }

        fn contains_key(&self, k: &str) -> bool {
            if let Some(cache) = self {
                cache.contains_key(k)
            } else {
                false
            }
        }
    }
}
