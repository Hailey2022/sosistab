use std::{collections::BTreeMap, net::SocketAddr, sync::Arc, time::Instant};

use crate::SessionBack;
use bytes::Bytes;
use indexmap::IndexMap;
use parking_lot::RwLock;

pub struct ShardedAddrs {
    map: IndexMap<u8, SocketAddr>,
    index: usize,
    last_time: Instant,
}

impl ShardedAddrs {
    pub fn new(initial_shard: u8, initial_addr: SocketAddr) -> Self {
        let mut map = IndexMap::new();
        map.insert(initial_shard, initial_addr);
        Self {
            map,
            index: 0,
            last_time: Instant::now(),
        }
    }

    pub fn get_addr(&mut self) -> SocketAddr {
        if self.last_time.elapsed().as_millis() > 100 {
            self.last_time = Instant::now();
            *self.map.get_index(self.index).unwrap().1
        } else {
            loop {
                self.last_time = Instant::now();
                self.index = self.index.wrapping_add(1) % self.map.len();
                if let Some(val) = self.map.get_index(self.index) {
                    return *val.1;
                }
            }
        }
    }
}

struct SessEntry {
    session_back: Arc<SessionBack>,
    addrs: Arc<RwLock<ShardedAddrs>>,
}

#[derive(Default)]
pub(crate) struct SessionTable {
    token_to_sess: BTreeMap<Bytes, SessEntry>,
    addr_to_token: BTreeMap<SocketAddr, Bytes>,
}

impl SessionTable {
    #[tracing::instrument(skip(self), level = "trace")]
    pub fn rebind(&mut self, addr: SocketAddr, shard_id: u8, token: Bytes) -> bool {
        if let Some(entry) = self.token_to_sess.get(&token) {
            let old = {
                let mut addrs = entry.addrs.write();
                let old = addrs.map.insert(shard_id, addr);
                addrs.index = addrs.map.get_index_of(&shard_id).unwrap();
                old
            };
            tracing::trace!("binding {}=>{}", shard_id, addr);
            if let Some(old) = old {
                self.addr_to_token.remove(&old);
            }
            self.addr_to_token.insert(addr, token);
            true
        } else {
            false
        }
    }

    #[tracing::instrument(skip(self), level = "trace")]
    pub fn delete(&mut self, token: Bytes) {
        if let Some(entry) = self.token_to_sess.remove(&token) {
            for (_, addr) in entry.addrs.read().map.iter() {
                self.addr_to_token.remove(addr);
            }
        }
    }

    #[tracing::instrument(skip(self), level = "trace")]
    pub fn lookup(&self, addr: SocketAddr) -> Option<&SessionBack> {
        let token = self.addr_to_token.get(&addr)?;
        let entry = self.token_to_sess.get(token)?;
        Some(&entry.session_back)
    }

    #[tracing::instrument(skip(self, session_back, locked_addrs), level = "trace")]
    pub fn new_sess(
        &mut self,
        token: Bytes,
        session_back: Arc<SessionBack>,
        locked_addrs: Arc<RwLock<ShardedAddrs>>,
    ) {
        let entry = SessEntry {
            session_back,
            addrs: locked_addrs,
        };
        self.token_to_sess.insert(token, entry);
    }
}
