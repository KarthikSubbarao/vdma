//! An established RMA connection: one-sided reads and writes against a peer's exposed buffer.

use std::marker::PhantomData;
use std::os::raw::c_void;

use libc::iovec;

use crate::sys::{
    FI_DELIVERY_COMPLETE, fi_addr_t, fi_msg_rma, fi_read, fi_rma_iov, fi_writemsg, fid_ep,
};

/// A connection to a peer's exposed buffer, borrowing the endpoint that owns the fabric resources.
///
/// Both posts return at once with the raw libfabric code: `0` for posted, `-FI_EAGAIN` for a full
/// transmit queue, else an error. The worker supplies `descriptor` from the endpoint's
/// registrations, owns `context`, and reaps the completion. `remote_address` is the peer buffer's
/// virtual address on `FI_MR_VIRT_ADDR` providers like efa, or 0 where addressing is by offset, like
/// tcp.
#[derive(Debug)]
pub struct LibfabricConnection<'endpoint> {
    endpoint: *mut fid_ep,
    peer: fi_addr_t,
    remote_key: u64,
    remote_address: u64,
    /// Ties this connection's lifetime to the endpoint owning the raw fabric resources.
    endpoint_lifetime: PhantomData<&'endpoint fid_ep>,
}

impl LibfabricConnection<'_> {
    pub(crate) fn new(
        endpoint: *mut fid_ep,
        peer: fi_addr_t,
        remote_key: u64,
        remote_address: u64,
    ) -> Self {
        Self {
            endpoint,
            peer,
            remote_key,
            remote_address,
            endpoint_lifetime: PhantomData,
        }
    }

    /// Post an `fi_writemsg`, the `dma.get` direction. `FI_DELIVERY_COMPLETE` asks for a completion
    /// once the peer has applied the write, but an EFA NIC only promises it has initiated its
    /// posted writes. `efa-direct` ignores the flag. A reply sent by another device can technically beat
    /// the payload; `quiesce_writes` guarantees writes are there with a sentinel read back.
    pub(crate) fn post_out(
        &self,
        bytes: &[u8],
        descriptor: *mut c_void,
        context: *mut c_void,
    ) -> isize {
        unsafe {
            let buffer = iovec {
                iov_base: bytes.as_ptr().cast_mut().cast(),
                iov_len: bytes.len(),
            };
            let mut descriptors = [descriptor];
            let remote = fi_rma_iov {
                addr: self.remote_address,
                len: bytes.len(),
                key: self.remote_key,
            };
            let message = fi_msg_rma {
                msg_iov: &buffer,
                desc: descriptors.as_mut_ptr(),
                iov_count: 1,
                addr: self.peer,
                rma_iov: &remote,
                rma_iov_count: 1,
                context,
                data: 0,
            };
            let _span = tracing::info_span!("fi_writemsg", len = bytes.len()).entered();
            fi_writemsg(self.endpoint, &message, u64::from(FI_DELIVERY_COMPLETE))
        }
    }

    /// Post an `fi_read`, the `dma.set` direction. See [`Self::post_out`].
    pub(crate) fn post_in(
        &self,
        bytes: &mut [u8],
        descriptor: *mut c_void,
        context: *mut c_void,
    ) -> isize {
        let length = bytes.len();
        unsafe {
            let _span = tracing::info_span!("fi_read", len = length).entered();
            fi_read(
                self.endpoint,
                bytes.as_mut_ptr().cast(),
                length,
                descriptor,
                self.peer,
                self.remote_address,
                self.remote_key,
                context,
            )
        }
    }
}
