# VDMA: Low-latency bulk Valkey transfers with EFA

EFAs (Elastic Fabric Adapters) are available on AWS's most potent GPU instances, and
offer terabits of throughput per instance. Currently the popular way to get that kind
of throughput is by gpu-to-gpu transfers. GPU memory is expensive. Herein we propose
the design for Valkey to provide economical bulk storage for kvcache, and generally
for large items that demand high throughput with low latency.

# VDMA module requirements
* Lock-free, gil-free DMA into and out of Valkey's keyspace
* Zero-copy on both read and write
* Optionally DMA GPU memory without traversing host memory bus
* Use Valkey's fundamental scalar type, ValkeyString (set/get to same keyspace as dma.get/set)
  * Cluster and replication are non-goals for launch, but must be supportable by the module
    implementation for later addition.
* VDMA initiates all DMA - client exposes memory to server
* Use open source Valkey module apis
* Valkey RESP commands initiate and complete rpcs
* Optional payload checksums

# Safety
Blast radius of a central Valkey or cluster is greater than a client leaf.
Therefore, VDMA initiates all DMA. No client can refer to server memory directly.

Clients can choose any strategy for DMA memory eligibility registration to satisfy
their own security and latency requirements.

# Valkey module api requirements
https://github.com/valkey-io/valkey/pull/4050

* Reference a ValkeyString without holding the GIL
* Adopt an allocated ValkeyString's memory directly into the keyspace
* Allocate a ValkeyString without zeroing (tantamount to copy)

# Communication model
![vdma design](vdma.png)
A worker thread per EFA manages libfabric resources. EFA requires the remote's address on
the receiving side, so the server advertises every worker's address up front and the client
holds them all. That lets the server pick a worker per transfer — the least loaded one —
rather than pinning a client to a device and inheriting whatever imbalance the assignment
happened to produce. A client that wants N EFA's worth of throughput on its own side still
binds N local interfaces; that choice stays with the client.

# RESP: control channel
Standard RESP initiates and completes every transfer. It carries the client's buffer
advertisement and the result per RPC. The client advertises memory, and the server uses
it according to the control channel's instructions.

* `DMA.HELLO`: Server returns an array of (hex) fabric addresses, one per efa worker — the
  set of source addresses it may initiate from. The client inserts all of them into its
  address vector before any RMA, since any of them may be the initiator for a given transfer.
* `DMA.SET <address> <rkey> <remote-address> <length> <key> [<crc>]`: Client advertises
  a registered buffer (endpoint address, remote key, buffer virtual address) with a value.
  Server sizes the ValkeyString and dma-reads the payload straight into it. If requested,
  it verifies `<crc>` (mismatch aborts the write), and replies `<n_bytes>` over RESP.
* `DMA.GET <address> <rkey> <remote-address> <capacity> <key> [<crc-flag>]`: Client
  advertises a registered buffer and capacity. Server dma-writes the value into it (nil
  if absent, error if the value is larger than `capacity`). Replies `<n_bytes> [<crc>]`
  over RESP. Client verifies checksum if requested.
* `DMA.INFO` reports provider attributes (diagnostics).

# EFA-direct: data path
Server-initiated one-sided RMA via the `efa-direct` provider, zero-copy in/out of
ValkeyString memory. Valkey server is always the DMA initiator. Clients are passive
targets.

### Direction
* `DMA.GET`: `fi_writemsg` local -> peer, flagged `FI_DELIVERY_COMPLETE`. On EFA
  that only means the target device has initiated its posted writes. `efa-direct` ignores
  the flag. The RESP reply travels over a different NIC, so without `quiesce_writes` it is
  ordered behind the payload by timing. With `quiesce_writes`, a one-byte `fi_read` follows
  the write, and the write response waits for the read to complete. The target NIC can't
  return the read ahead of its own posted writes.
  Without `quiesce_writes`, the race is between local PCIe and 2 remote servers + kernels +
  serialization + tcp + 1.5 wire RTT's + application time.
* `DMA.SET`: `fi_read` peer -> local.

### Async / in-flight
Posted DMA operations return immediately. Completions land on the completion queue keyed
by per-op context (`FI_CONTEXT2`). One worker per endpoint busy-polls that queue
(`fi_cq_read`) while any op is in flight. It drains new requests, posting up to an
in-flight cap, and reaps completions in batches. It hands completion batches back under
a single GIL acquisition. When nothing is outstanding it parks on the request channel (a
blocking `recv`) to release the thread.

### Addressing
The advertised `remote-address` is the client buffer's virtual address. Each worker inserts
a client's address into its own address vector on first use, and every worker `fi_av_remove`s
it on disconnect, deferring until that worker's in-flight transfers for the client drain.

### GPUDirect (optional)
The client buffer may be GPU device memory exposed via dmabuf (`FI_HMEM` / `FI_MR_DMABUF`).
The server RMAs directly into/out of VRAM. Requires a p2p-capable instance (eligible
selection is fairly sparse).
