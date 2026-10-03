use crate::server::ipfix::codec::{self, Batch, Template};
use anyhow::{ensure, Context, Result};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
pub const MAX_DOMAINS: usize = 32;
pub const DEFAULT_REFRESH_SECONDS: u64 = 60;
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);
#[derive(Clone, Default)]
pub struct Domain {
    pub templates: BTreeMap<u16, Template>,
    pub sequence: u32,
}
#[derive(Default)]
pub struct Catalog {
    pub domains: BTreeMap<u32, Domain>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExportInfo {
    pub observation_domain_id: u32,
    pub sequence_number: u32,
    pub record_count: usize,
    pub template_count: usize,
    pub byte_count: usize,
    pub local_transport_only: bool,
}
pub struct Prepared {
    pub domain: Domain,
    pub bytes: Vec<u8>,
    pub info: ExportInfo,
}
pub fn epoch() -> Result<u32> {
    u32::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
        .context("IPFIX export time beyond unsigned32 range")
}
impl Catalog {
    pub fn prepare(&self, batch: &Batch) -> Result<Prepared> {
        ensure!(
            self.domains.contains_key(&batch.observation_domain_id)
                || self.domains.len() < MAX_DOMAINS,
            "exporter domain cap32"
        );
        let mut domain = self
            .domains
            .get(&batch.observation_domain_id)
            .cloned()
            .unwrap_or_default();
        for t in &batch.templates {
            if let Some(old) = domain.templates.get(&t.id) {
                ensure!(
                    old.wire()? == t.wire()?,
                    "template ID cannot be redefined within this UDP exporter; choose a new ID"
                );
            } else {
                ensure!(
                    domain.templates.len() < codec::MAX_TEMPLATES_PER_SESSION,
                    "exporter templates/domain cap32"
                );
                domain.templates.insert(t.id, t.clone());
            }
        }
        let sequence = domain.sequence;
        let export_time = match batch.export_time {
            Some(t) => t,
            None => epoch()?,
        };
        let (bytes, count) = codec::encode(batch, sequence, export_time)?;
        domain.sequence = sequence.wrapping_add(count as u32);
        let info = ExportInfo {
            observation_domain_id: batch.observation_domain_id,
            sequence_number: sequence,
            record_count: count,
            template_count: batch.templates.len(),
            byte_count: bytes.len(),
            local_transport_only: true,
        };
        Ok(Prepared {
            domain,
            bytes,
            info,
        })
    }
    pub fn refresh(&self) -> Result<Vec<Vec<u8>>> {
        let now = epoch()?;
        self.domains
            .iter()
            .map(|(id, d)| {
                codec::encode(
                    &Batch {
                        observation_domain_id: *id,
                        export_time: None,
                        templates: d.templates.values().cloned().collect(),
                        data_sets: vec![],
                    },
                    d.sequence,
                    now,
                )
                .map(|(bytes, _)| bytes)
            })
            .collect()
    }
}
pub async fn send(socket: Arc<tokio::net::UdpSocket>, messages: Vec<Vec<u8>>) -> Result<usize> {
    tokio::time::timeout(IO_TIMEOUT, async move {
        let mut sent = 0;
        for bytes in messages {
            ensure!(
                socket.send(&bytes).await? == bytes.len(),
                "incomplete UDP datagram send"
            );
            sent += 1;
        }
        Ok(sent)
    })
    .await
    .context("IPFIX UDP write deadline exceeded")?
}
