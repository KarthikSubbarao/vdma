# Vdma clients
A vdma client is essentially a RESP client. Many RESP clients exist today,
in a multitude of languages. Each ecosystem has its popular approach to
building, reusing, or extending libraries. However that may be for each
individual ecosystem, all vdma clients will need to consider some common
concerns.

# Host support probe
Not all clients have hardware that works with vdma. Currently, only AWS
EFA is supported, with `efa-direct`. `libfabric` is used, but not all
providers are currently wired or supported.

Clients need to, before expecting `DMA.*` commands to work, inspect their
environment for the needful hardware through `libfabric`. Some of the relevant
client libfabric configuration includes:

* `caps = FI_MSG | FI_RMA | FI_READ | FI_WRITE | FI_REMOTE_READ | FI_REMOTE_WRITE`
  * `| FI_HMEM` if you want to also use ***GpuDirect***
* `fabric_attr.name = "efa-direct"`: Must choose efa-direct provider rather
  than efa, which is software rma.
* `ep_attr.type_ = FI_EP_RDM`: Connectionless reliable datagram messages is
  how vdma is currently set up.
* `domain_attr.mr_mode = FI_MR_LOCAL | FI_MR_ALLOCATED | FI_MR_PROV_KEY | FI_MR_VIRT_ADDR`:
  Advertise the memory region modes explicitly.
  * `| FI_MR_HMEM` if you want to also use ***GpuDirect***
* `fi_info.mode |= FI_CONTEXT2`: Required for `efa-direct` provider.

Clients are passive RDMA targets, and have to set themselves up to be such.

# Registering memory
Clients need to choose how to register memory. The most general, safest, and
most portable approach is to register a bounce buffer per client instance.
It's not the fastest thing possible, but it's the easiest functional bar to
clear.

Register `DMA.SET` buffers with `FI_REMOTE_READ`, since the server reads them. Register
`DMA.GET` buffers with `FI_REMOTE_WRITE`, and with `FI_REMOTE_READ` if the server sets
`quiesce_writes`. Registering all buffers with both works fine.

Once a bounce buffer works, consider the target use cases. If someone wants
to DMA to a video card, there may be some large memory region that can be
registered, filled, and reused.

Memory region reuse is important, because addresses are retained in the server's
address vector. Only 1 DMA can be happening per client instance at a time, so
at most 1 address at a time per client is registered with vdma. Reusing the
address allows vdma to serve responses more quickly than if you use a different
rkey for every RPC. Strive to reuse memory regions!

# Interface multiplicity
A client host may have more than 1 EFA card. It may have many. There may also
be non-uniform affinity between some EFA cards and some GpuDirect targets. Those
details are beyond generalizations, but generally if they're in scope for a client
then they need to be considered as part of a client's implementation.

`DMA.HELLO` returns every address the server may initiate from. A passive
client EFA requires its remote's address to be registered, so insert all of them into your
address vector before the first RMA — any of them may serve a given transfer, and which one
does is the server's choice, made per operation. When the Valkey connection is dropped,
clear the related client EFA state (the vdma server clears its side as well).

The client keeps the reciprocal choice of which local interface exposes the buffer it
advertises.

# Device support
GpuDirect is a fantastic low-overhead way to get data from Valkey into a gpu. This
flow works, but it depends on client hardware supporting GpuDirect. The pickings
are fairly slim in that regard at this time. Instances like g7e.12xlarge and greater
work with GpuDirect, and possibly some of the more exotic tier selection.

Due to the poor compatibility, host memory buffers should be available for usability
even if the desired state for a workflow is GpuDirect. Practically, one can set up
most of a workflow with `efa-direct` to host memory before committing to specialty
hardware with top-tier capabilities.
