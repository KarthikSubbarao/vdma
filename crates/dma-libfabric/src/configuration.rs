//! Configuration for the libfabric DMA provider. Aggregated by the `configuration` crate.

use std::time::Duration;

use crate::sys::{FI_CONTEXT2, FI_RX_CQ_DATA};

/// Supported libfabric providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Software RMA over sockets, for development without RDMA hardware.
    Tcp,
    /// The EFA hardware-RDMA path: the `efa` provider's `efa-direct` fabric, which uses device RMA
    /// with no software wire protocol or host progress engine, unlike EFA's rxr path. Requires
    /// `FI_CONTEXT2` — see `server_dma_worker::InFlight` for how that is satisfied.
    #[serde(rename = "efa-direct")]
    EfaDirect,
}

impl Provider {
    /// The `fi_fabric_attr.prov_name`. `EfaDirect` shares the `efa` provider and is distinguished by
    /// [`Self::fabric_name`].
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Tcp => "tcp",
            Provider::EfaDirect => "efa",
        }
    }

    /// The `fi_fabric_attr.name` to pin when the provider alone doesn't disambiguate. One `efa`
    /// provider exposes both the rxr software `efa` fabric and the device-RMA `efa-direct` one, so
    /// selecting hardware RDMA means pinning the fabric name.
    pub fn fabric_name(self) -> Option<&'static str> {
        match self {
            Provider::EfaDirect => Some("efa-direct"),
            _ => None,
        }
    }

    /// The `fi_info.mode` bits the provider mandates; `fi_getinfo` skips a fabric whose required
    /// mode the hints don't accept. efa-direct needs `FI_CONTEXT2`, a provider-owned `fi_context2`
    /// as each op's `op_context`, and to support rdma-core versions before 52, `FI_RX_CQ_DATA`.
    /// Remote CQ data is not applicable to dma-libfabric, but the support declaration is required.
    pub fn required_mode(self) -> u64 {
        match self {
            Provider::Tcp => 0,
            Provider::EfaDirect => FI_CONTEXT2 | FI_RX_CQ_DATA,
        }
    }
}

/// Local configuration section for the libfabric DMA layer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    /// Providers to attempt, in preference order.
    #[serde(default = "default_providers")]
    pub providers: Vec<Provider>,
    /// Interface names to use; empty means every interface discovered on the configured hardware.
    #[serde(default)]
    pub interfaces: Vec<String>,
    /// Source address to bind the local endpoint to. `None` lets the provider choose.
    #[serde(default)]
    pub bind: Option<String>,
    /// Transfers the server keeps in flight at once. `None` derives a default from the provider's
    /// transmit-queue depth, capped to bound pinned memory.
    #[serde(default)]
    pub max_in_flight: Option<usize>,
    /// `f-pool-nn` threads running checksummed transfers' CRC and reply off the fabric worker, so
    /// hashing a payload doesn't stall the post-and-reap loop. More threads hash more in parallel,
    /// though the caller's completion hook still serializes. `None` derives 2 per EFA device.
    #[serde(default)]
    pub crc_pool_threads: Option<usize>,
    /// How long a transfer may sit unpostable at `-FI_EAGAIN`.
    /// A connecting or dead client will loop with this status. Defaults to one second.
    #[serde(default = "default_progress_deadline")]
    pub progress_deadline: Duration,
    /// Follow each `ToPeer` write with a read of the last byte.
    ///
    /// A write completion, at least on EFA, only promises the target NIC has initiated its posted
    /// writes. A subsequent read is ordered after the write on the target's PCIe, so happens-before
    /// is established.
    ///
    /// This can matter when the "done" signal takes another path where relative ordering is not
    /// strictly guaranteed, like a RESP reply over a different NIC. The target must expose write
    /// buffers with `FI_REMOTE_READ`.
    ///
    /// Off by default.
    #[serde(default)]
    pub quiesce_writes: bool,
}

fn default_progress_deadline() -> Duration {
    Duration::from_secs(1)
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            providers: default_providers(),
            interfaces: Vec::new(),
            bind: None,
            max_in_flight: None,
            crc_pool_threads: None,
            progress_deadline: default_progress_deadline(),
            quiesce_writes: false,
        }
    }
}

fn default_providers() -> Vec<Provider> {
    vec![Provider::Tcp]
}
