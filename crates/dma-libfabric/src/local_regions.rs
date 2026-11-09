//! Per-operand local registrations, cached in [`crate::region_cache`].
//!
//! EFA requires `FI_MR_LOCAL`: every local RMA operand needs a registered descriptor.
//! Freed memory must not keep a registration. An EFA registration is frozen to the
//! physical pages present when it was made.
//!
//! Registered `FI_READ | FI_WRITE`, never remote-accessible, and the key is never exported, so
//! nothing here is reachable by a peer.

use std::os::raw::c_void;
use std::ptr;
use std::sync::Arc;

use crate::sys::{FI_READ, FI_WRITE, fi_mr_desc, fi_mr_reg, fid_mr};
use dma_libfabric_protocol::DmaError;

use crate::domain::Domain;
use crate::error::check;
use crate::operands::CacheableSpan;
use crate::region_cache::{self, Registration};

/// A local RMA operand's descriptor and the lease keeping its registration alive.
#[derive(Debug)]
pub(crate) struct LocalOperand {
    pub(crate) descriptor: *mut c_void,
    /// `None` only on providers that need no local registration at all, where the descriptor is
    /// null and there is nothing to hold open.
    pub(crate) lease: Option<Arc<Registration>>,
}

/// Per-endpoint handle for the operand-registration path, deferring registration lifetime to
/// [`crate::region_cache`].
#[derive(Debug)]
pub(crate) struct LocalRegions {
    domain: Arc<Domain>,
}

impl LocalRegions {
    pub(crate) fn new(domain: Arc<Domain>) -> Self {
        Self { domain }
    }

    /// The local descriptor covering `[pointer, pointer + length)`, reusing a cached registration
    /// that already covers it, else taking a fresh one. A cached registration is deregistered as its
    /// memory is reclaimed, so it stays valid as long as the operand does — which spans the
    /// transfer, since the transfer retains its operand.
    ///
    /// `cacheable` yields the caller's [`crate::CacheableSpan`] for this operand. It is asked only
    /// on a miss, so it pays once per registration rather than once per transfer.
    /// `None` registers the operand's extent and keeps it out of the cache.
    pub(crate) fn operand(
        &mut self,
        pointer: *mut u8,
        length: usize,
        cacheable: impl FnOnce() -> Option<CacheableSpan>,
    ) -> Result<LocalOperand, DmaError> {
        let start = pointer as usize;
        let end = start
            .checked_add(length)
            .ok_or_else(|| DmaError::Fabric("local operand range overflows".into()))?;

        let domain = self.domain.key();
        if let Some(lease) = region_cache::covering(start, end, domain) {
            // SAFETY: the lease holds the region open for as long as this operand lives, so the
            //         descriptor cannot be deregistered underneath the transfer that posts it.
            let descriptor = unsafe { fi_mr_desc(lease.memory_region()) };
            return Ok(LocalOperand {
                descriptor,
                lease: Some(lease),
            });
        }

        // A span not containing the operand would pin the wrong memory and leave the operand itself
        // unregistered, so it is refused here rather than trusted.
        let cacheable = cacheable().filter(|span| span.contains(start, end));
        // Register the caller's whole span when there is one, so the next operand landing inside it
        // reuses this registration instead of taking its own.
        let (base, limit) = cacheable.map_or((start, end), |span| (span.base, span.end));
        // If a widened registration fails, take the operand's extent instead
        let region = match self.register(base as *mut u8, limit - base) {
            Ok(region) => Ok((base, limit, region)),
            Err(_) if (base, limit) != (start, end) => self
                .register(pointer, length)
                .map(|region| (start, end, region)),
            Err(error) => Err(error),
        };
        let (base, limit, region) = region?;
        let lease = Registration::new(region, Arc::clone(&self.domain));
        // SAFETY: freshly registered above, and held by `lease`.
        let descriptor = unsafe { fi_mr_desc(lease.memory_region()) };
        // Only what the caller promised to `invalidate` may outlive this transfer in the registry —
        // including the narrowed fallback above, which lies inside the span that promise covers.
        if cacheable.is_some() {
            // The registry takes its own lease; this operand keeps the one it already holds.
            region_cache::track(base, limit, domain, &lease);
        }
        Ok(LocalOperand {
            descriptor,
            lease: Some(lease),
        })
    }

    /// Register `[base, base + length)` up front and leave it in the registry.
    ///
    /// every later operand inside should be a `covering()` hit, so no
    /// [`crate::CacheableSpan`] is needed for these regions.
    /// The registry is the sole holder. The span's lifetime belongs to the caller's
    /// [`crate::MemoryRegion`], whose drop retires this entry.
    pub(crate) fn pin(&self, base: usize, length: usize) -> Result<(), DmaError> {
        let end = base
            .checked_add(length)
            .ok_or_else(|| DmaError::Fabric("pinned region range overflows".into()))?;
        let lease = Registration::new(
            self.register(base as *mut u8, length)?,
            Arc::clone(&self.domain),
        );
        region_cache::track(base, end, self.domain.key(), &lease);
        Ok(())
    }

    fn register(&self, pointer: *mut u8, length: usize) -> Result<*mut fid_mr, DmaError> {
        let mut memory_region: *mut fid_mr = ptr::null_mut();
        // A root span, so first-touch registration cost is visible.
        let _span = tracing::info_span!(parent: None, "region_register", bytes = length).entered();
        check(
            unsafe {
                fi_mr_reg(
                    self.domain.handle(),
                    pointer as *const c_void,
                    length,
                    u64::from(FI_READ | FI_WRITE),
                    0,
                    0,
                    0,
                    &mut memory_region,
                    ptr::null_mut(),
                )
            },
            "fi_mr_reg(operand)",
        )?;
        Ok(memory_region)
    }

    /// Release the registry's leases on this domain.
    pub(crate) fn clear(&mut self) {
        region_cache::clear_domain(self.domain.key());
    }
}
