//! A libfabric domain its fabric. For sharing and keeping close lifecycle straight.

use std::ptr;
use std::sync::Arc;

use crate::sys::{fi_close, fi_domain, fi_fabric, fi_freeinfo, fi_info, fid_domain, fid_fabric};
use dma_libfabric_protocol::DmaError;

use crate::error::check;

/// An open domain, closed when the last holder drops it.
///
/// Memory regions are domain objects, and the last lease on one can drop on any thread. Each
/// registration holds a clone of this, so its `fi_close` always runs before the domain's.
#[derive(Debug)]
pub(crate) struct Domain {
    domain: *mut fid_domain,
    fabric: *mut fid_fabric,
    info: *mut fi_info,
}

// SAFETY: `query_info` requests FI_THREAD_SAFE, so libfabric permits domain calls from any thread.
// The handles are written only by `open`, before this is shared, and closed only by the last drop.
unsafe impl Send for Domain {}
unsafe impl Sync for Domain {}

impl Domain {
    /// Open the fabric and domain `info` selects.
    ///
    /// # Safety
    /// `info` is a valid, standalone `fi_info`. This takes ownership of it, including on error.
    pub(crate) unsafe fn open(info: *mut fi_info) -> Result<Arc<Self>, DmaError> {
        // All-null, so an early error closes only what opened.
        let mut opened = Self {
            domain: ptr::null_mut(),
            fabric: ptr::null_mut(),
            info,
        };
        unsafe {
            check(
                fi_fabric((*info).fabric_attr, &mut opened.fabric, ptr::null_mut()),
                "fi_fabric",
            )?;
            check(
                fi_domain(opened.fabric, info, &mut opened.domain, ptr::null_mut()),
                "fi_domain",
            )?;
        }
        Ok(Arc::new(opened))
    }

    pub(crate) fn handle(&self) -> *mut fid_domain {
        self.domain
    }

    /// Identifies this domain in the region cache. It can't be reused while a cached entry exists,
    /// because each entry holds a clone.
    pub(crate) fn key(&self) -> usize {
        self.domain as usize
    }

    pub(crate) fn info(&self) -> *mut fi_info {
        self.info
    }
}

impl Drop for Domain {
    fn drop(&mut self) {
        // SAFETY: the last holder is gone, and `open` left any unopened handle null.
        unsafe {
            if !self.domain.is_null() {
                fi_close(&mut (*self.domain).fid);
            }
            if !self.fabric.is_null() {
                fi_close(&mut (*self.fabric).fid);
            }
            fi_freeinfo(self.info);
        }
    }
}
