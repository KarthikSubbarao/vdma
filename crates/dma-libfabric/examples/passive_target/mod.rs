//! A passive RMA target in raw libfabric: the client half, which this crate deliberately is not. The
//! contract it follows is `design/Client.md`.

use std::ffi::CString;
use std::os::raw::c_void;
use std::ptr;

use ofi_libfabric_sys::bindgen::{
    FI_CONTEXT2, FI_MR_ALLOCATED, FI_MR_LOCAL, FI_MR_PROV_KEY, FI_MR_VIRT_ADDR, FI_MSG, FI_READ,
    FI_RECV, FI_REMOTE_READ, FI_REMOTE_WRITE, FI_RMA, FI_SOURCE, FI_TRANSMIT, FI_WRITE, fi_addr_t,
    fi_allocinfo, fi_av_attr, fi_av_insert, fi_av_open, fi_av_type_FI_AV_MAP, fi_close, fi_cq_attr,
    fi_cq_entry, fi_cq_format_FI_CQ_FORMAT_CONTEXT, fi_cq_open, fi_cq_read, fi_domain, fi_enable,
    fi_endpoint, fi_ep_bind, fi_ep_type_FI_EP_RDM, fi_fabric, fi_freeinfo, fi_getinfo, fi_getname,
    fi_info, fi_mr_key, fi_mr_reg, fi_version, fid_av, fid_cq, fid_domain, fid_ep, fid_fabric,
    fid_mr,
};

/// What an initiator needs to RMA into an exposed buffer.
pub struct Exposed {
    pub remote_key: u64,
    pub remote_address: u64,
}

/// Everything opened, closed on drop in reverse construction order.
pub struct Target {
    memory_region: *mut fid_mr,
    endpoint: *mut fid_ep,
    completion_queue: *mut fid_cq,
    address_vector: *mut fid_av,
    domain: *mut fid_domain,
    fabric: *mut fid_fabric,
    info: *mut fi_info,
}

impl Drop for Target {
    fn drop(&mut self) {
        // SAFETY: each fid is closed once, in reverse order, and null when never opened.
        unsafe {
            for fid in [
                self.memory_region.cast::<c_void>(),
                self.endpoint.cast::<c_void>(),
                self.completion_queue.cast::<c_void>(),
                self.address_vector.cast::<c_void>(),
                self.domain.cast::<c_void>(),
                self.fabric.cast::<c_void>(),
            ] {
                if !fid.is_null() {
                    // Every fid_* starts with its `fid`, so the first field is the handle to close.
                    fi_close(fid.cast());
                }
            }
            if !self.info.is_null() {
                fi_freeinfo(self.info);
            }
        }
    }
}

fn check(code: i32, what: &str) -> Result<(), String> {
    if 0 == code {
        return Ok(());
    }
    Err(format!("{what} failed: {code}"))
}

impl Target {
    /// Open an enabled endpoint on `efa-direct`, or on tcp bound to `bind`.
    pub fn open(use_efa: bool, bind: Option<&str>) -> Result<Self, String> {
        let mut target = Target {
            memory_region: ptr::null_mut(),
            endpoint: ptr::null_mut(),
            completion_queue: ptr::null_mut(),
            address_vector: ptr::null_mut(),
            domain: ptr::null_mut(),
            fabric: ptr::null_mut(),
            info: ptr::null_mut(),
        };

        // Must match the initiator's hints or the two pick incompatible providers; mirrors
        // `endpoint::query_info`. On efa the provider name alone is ambiguous — it exposes both rxr
        // `efa` fabric and device-RDMA `efa-direct`.
        let provider = CString::new(if use_efa { "efa" } else { "tcp" })
            .map_err(|_| "provider name".to_string())?;
        let fabric_name = CString::new("efa-direct").map_err(|_| "fabric name".to_string())?;
        let node = match bind {
            Some(bind) => Some(CString::new(bind).map_err(|_| "bind address".to_string())?),
            None => None,
        };
        // SAFETY: hints are freed below; the CStrings outlive the fi_getinfo call and are detached
        // from the hints first, so fi_freeinfo doesn't free Rust-owned memory.
        unsafe {
            let hints = fi_allocinfo();
            if hints.is_null() {
                return Err("fi_allocinfo failed".into());
            }
            (*hints).caps =
                u64::from(FI_MSG | FI_RMA | FI_READ | FI_WRITE | FI_REMOTE_READ | FI_REMOTE_WRITE);
            (*(*hints).ep_attr).type_ = fi_ep_type_FI_EP_RDM;
            (*(*hints).domain_attr).mr_mode =
                (FI_MR_LOCAL | FI_MR_ALLOCATED | FI_MR_PROV_KEY | FI_MR_VIRT_ADDR) as i32;
            (*(*hints).fabric_attr).prov_name = provider.as_ptr().cast_mut();
            if use_efa {
                (*(*hints).fabric_attr).name = fabric_name.as_ptr().cast_mut();
                // efa-direct requires FI_CONTEXT2. This side posts no ops, but the mode has to
                // match for fi_getinfo to hand back the right fabric.
                (*hints).mode |= FI_CONTEXT2;
            }
            let (node_ptr, flags) = match &node {
                Some(node) => (node.as_ptr(), FI_SOURCE),
                None => (ptr::null(), 0),
            };
            let code = fi_getinfo(
                fi_version(),
                node_ptr,
                ptr::null(),
                flags,
                hints,
                &mut target.info,
            );
            (*(*hints).fabric_attr).prov_name = ptr::null_mut();
            (*(*hints).fabric_attr).name = ptr::null_mut();
            fi_freeinfo(hints);
            check(code, "fi_getinfo")?;
            if target.info.is_null() {
                return Err("no provider matched".into());
            }
        }

        // SAFETY: `target.info` is a live fi_info; each handle is stored in `target`, which closes
        // them.
        unsafe {
            check(
                fi_fabric(
                    (*target.info).fabric_attr,
                    &mut target.fabric,
                    ptr::null_mut(),
                ),
                "fi_fabric",
            )?;
            check(
                fi_domain(
                    target.fabric,
                    target.info,
                    &mut target.domain,
                    ptr::null_mut(),
                ),
                "fi_domain",
            )?;
            let mut av_attr: fi_av_attr = std::mem::zeroed();
            av_attr.type_ = fi_av_type_FI_AV_MAP;
            check(
                fi_av_open(
                    target.domain,
                    &mut av_attr,
                    &mut target.address_vector,
                    ptr::null_mut(),
                ),
                "fi_av_open",
            )?;
            let mut cq_attr: fi_cq_attr = std::mem::zeroed();
            cq_attr.format = fi_cq_format_FI_CQ_FORMAT_CONTEXT;
            check(
                fi_cq_open(
                    target.domain,
                    &mut cq_attr,
                    &mut target.completion_queue,
                    ptr::null_mut(),
                ),
                "fi_cq_open",
            )?;
            check(
                fi_endpoint(
                    target.domain,
                    target.info,
                    &mut target.endpoint,
                    ptr::null_mut(),
                ),
                "fi_endpoint",
            )?;
            check(
                fi_ep_bind(target.endpoint, &mut (*target.address_vector).fid, 0),
                "fi_ep_bind(av)",
            )?;
            check(
                fi_ep_bind(
                    target.endpoint,
                    &mut (*target.completion_queue).fid,
                    u64::from(FI_TRANSMIT | FI_RECV),
                ),
                "fi_ep_bind(cq)",
            )?;
            check(fi_enable(target.endpoint), "fi_enable")?;
        }
        Ok(target)
    }

    /// Register `length` bytes at `pointer` for remote access. Remote-accessible, unlike the
    /// initiator's local-only operands. One region per target. `remote_read` false exposes it for
    /// remote writes only, as a GET buffer can be when the server doesn't quiesce.
    ///
    /// # Safety
    /// The memory must stay allocated until this target drops, which closes the registration.
    pub unsafe fn expose(
        &mut self,
        pointer: *mut u8,
        length: usize,
        remote_read: bool,
    ) -> Result<Exposed, String> {
        let remote = if remote_read {
            FI_REMOTE_READ | FI_REMOTE_WRITE
        } else {
            FI_REMOTE_WRITE
        };
        // SAFETY: the caller keeps the memory alive past the registration.
        let remote_key = unsafe {
            check(
                fi_mr_reg(
                    self.domain,
                    pointer.cast::<c_void>(),
                    length,
                    u64::from(remote | FI_READ | FI_WRITE),
                    0,
                    0,
                    0,
                    &mut self.memory_region,
                    ptr::null_mut(),
                ),
                "fi_mr_reg",
            )?;
            fi_mr_key(self.memory_region)
        };
        // SAFETY: `info` is live for the target's lifetime.
        let mr_mode = unsafe { (*(*self.info).domain_attr).mr_mode } as u32;
        // On FI_MR_VIRT_ADDR providers like efa the remote address is the buffer's virtual address;
        // on tcp it is an offset into the region, so 0.
        let remote_address = if 0 != mr_mode & FI_MR_VIRT_ADDR {
            pointer as u64
        } else {
            0
        };
        Ok(Exposed {
            remote_key,
            remote_address,
        })
    }

    /// The endpoint's fabric address, to advertise.
    pub fn address(&self) -> Result<Vec<u8>, String> {
        // SAFETY: writes `length` bytes of address into `address`, after asking for the size.
        unsafe {
            let mut length: usize = 0;
            fi_getname(&mut (*self.endpoint).fid, ptr::null_mut(), &mut length);
            let mut address = vec![0u8; length];
            check(
                fi_getname(
                    &mut (*self.endpoint).fid,
                    address.as_mut_ptr().cast::<c_void>(),
                    &mut length,
                ),
                "fi_getname",
            )?;
            address.truncate(length);
            Ok(address)
        }
    }

    /// efa-direct requires a target to hold the initiator's address before it can be RMA'd
    /// against. tcp does not.
    pub fn insert_initiator(&mut self, address: &[u8]) -> Result<(), String> {
        let mut peer: fi_addr_t = 0;
        // SAFETY: one address of the provider's own format, from the initiator.
        let inserted = unsafe {
            fi_av_insert(
                self.address_vector,
                address.as_ptr().cast::<c_void>(),
                1,
                &mut peer,
                0,
                ptr::null_mut(),
            )
        };
        if 1 != inserted {
            return Err(format!("fi_av_insert inserted {inserted} of 1"));
        }
        Ok(())
    }

    /// Drive progress once. Only tcp's FI_PROGRESS_MANUAL needs it; efa-direct lands an inbound RMA
    /// with no call here, and the write raises no completion on this side either way.
    #[allow(dead_code, reason = "an efa-only target never calls it")]
    pub fn progress(&self) {
        // SAFETY: a one-entry read into an owned value; the queue is live until `self` drops.
        unsafe {
            let mut entry: fi_cq_entry = std::mem::zeroed();
            fi_cq_read(self.completion_queue, (&raw mut entry).cast::<c_void>(), 1);
        }
    }
}
