mod commit_log;

use std::sync::Arc;

use async_trait::async_trait;
use bloom_filter_rs::{BloomFilter, Murmur3};
use rb_tree::RBMap;
use tokio::sync::Mutex;

use super::TransactInstruction::{Delete, Set};
use super::{Store, Transaction};
use crate::Result;

/// A store backed by a [log-structured merge tree](http://www.benstopford.com/2015/02/14/log-structured-merge-trees)
#[derive(Clone)]
pub struct LSMStore {
    data: Arc<Mutex<LSMStoreData>>,
}

struct LSMStoreData {
    memtable: RBMap<String, Option<Vec<u8>>>,
    bloom_filter: BloomFilter<Murmur3>,
}

impl LSMStore {
    pub fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(LSMStoreData {
                memtable: RBMap::new(),
                // TODO what is the optimal number of items for the bloom filter?
                bloom_filter: BloomFilter::optimal(Murmur3, 512, 0.01),
            })),
        }
    }

    // TODO implement memtable flushes to disk
    // TODO implement segment compaction
}

#[async_trait]
impl Store for LSMStore {
    async fn get(&mut self, k: &str) -> Result<Option<Vec<u8>>> {
        let store = self.data.lock().await;
        if !store.bloom_filter.contains(k.as_bytes()) {
            return Ok(None);
        }
        let mem_result = store.memtable.get(&k.to_string());
        // TODO read from disk if value not found in memtable
        Ok(mem_result.unwrap().as_deref().map(|v| v.to_vec()))
    }

    async fn transact<'a>(
        &mut self,
        transaction: Transaction<'a>,
    ) -> Result<()> {
        let mut store = self.data.lock().await;
        // TODO write begin_transaction to WAL
        for instruction in transaction.instructions {
            match instruction {
                Set(key, value) => {
                    store
                        .memtable
                        .insert(key.to_string(), Some(value.to_vec()));
                    store.bloom_filter.insert(key.as_bytes());
                }
                Delete(key) => {
                    store.memtable.insert(key.to_string(), None);
                }
            };
        }
        Ok(())
    }
}