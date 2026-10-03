//! Bounded datagram sequence state, committed only after a successful local send.
use crate::server::sflow::codec::{self, Batch};
use anyhow::{ensure, Result};
use std::{collections::BTreeMap, net::IpAddr, time::Duration};

pub const MAX_AGENTS: usize = 32;
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Default)]
pub struct Sequences {
    next: BTreeMap<(IpAddr, u32), u32>,
}
pub struct Prepared {
    pub bytes: Vec<u8>,
    pub records: usize,
    pub sample_count: usize,
    pub sequence: u32,
    pub uptime: u32,
    pub agent: IpAddr,
    pub sub_agent: u32,
    candidate: Sequences,
}
impl Sequences {
    pub fn prepare(&self, batch: &Batch, uptime: u32) -> Result<Prepared> {
        let key = (batch.agent_address, batch.sub_agent_id);
        ensure!(
            self.next.contains_key(&key) || self.next.len() < MAX_AGENTS,
            "sFlow agent/sub-agent cap32"
        );
        let sequence = self.next.get(&key).copied().unwrap_or(0);
        let uptime = batch.uptime_ms.unwrap_or(uptime);
        let (bytes, records) = codec::encode(batch, sequence, uptime)?;
        let mut candidate = self.clone();
        candidate.next.insert(key, sequence.wrapping_add(1));
        Ok(Prepared {
            bytes,
            records,
            sample_count: batch.samples.len(),
            sequence,
            uptime,
            agent: batch.agent_address,
            sub_agent: batch.sub_agent_id,
            candidate,
        })
    }
    pub fn commit(&mut self, prepared: Prepared) {
        *self = prepared.candidate;
    }
}
