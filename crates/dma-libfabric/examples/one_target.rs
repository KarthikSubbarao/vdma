//! The passive peer the other examples RMA against: register a buffer, expose it for remote access,
//! print the advertisement, and poll until the bytes land.
//!
//! ```text
//! # terminal 1 — prints: advertisement: <address-hex> <rkey> <remote-addr>
//! cargo run -p dma-libfabric --example one_target -- 127.0.0.1
//! # terminal 2 — paste those three fields
//! cargo run -p dma-libfabric --example one_transfer -- 127.0.0.1 <address-hex> <rkey> <remote-addr>
//! ```
//!
//! `--read` prefills the buffer and serves it to `read_from_peer` instead. This just holds the buffer
//! open and lets the initiator do the verifying.
//!
//! `--efa` opens the `efa-direct` fabric between two EFA instances instead of tcp loopback. There the
//! order reverses, because a target must hold the initiator's address before it can be RMA'd against
//! and an endpoint's address is new every run — so the initiator starts first and waits on stdin:
//!
//! ```text
//! # instance C — prints `local address: <initiator-hex>`, then waits
//! cargo run -p dma-libfabric --example read_from_peer -- --efa
//! # instance B — takes the initiator address as its only positional
//! cargo run -p dma-libfabric --example one_target -- --efa --read <initiator-hex>
//! # paste the target's three fields into C's stdin
//! ```
//!
//! Raw libfabric: a target uses no part of this crate, which is the initiator half. The contract is
//! `design/Client.md`.

use std::time::{Duration, Instant};

use dma_libfabric_protocol::{checksum, decode_hex, encode_hex};

mod passive_target;

const BUFFER_LEN: usize = 4096;

/// That which `one_transfer` writes and `read_from_peer` fetches.
const PATTERN: u8 = 0xab;

fn main() -> Result<(), String> {
    let (flags, positional): (Vec<String>, Vec<String>) = std::env::args()
        .skip(1)
        .partition(|argument| argument.starts_with("--"));
    // Serve the buffer for a remote read rather than wait for a remote write.
    let serve_read = flags.iter().any(|flag| "--read" == flag);
    let use_efa = flags.iter().any(|flag| "--efa" == flag);
    let mut arguments = positional.into_iter();
    // An EFA endpoint binds to the device and advertises its own fabric address, so there is no
    // source to pin — the first positional is the initiator's address instead.
    let bind = if use_efa { None } else { arguments.next() };
    let initiator = arguments.next();

    // Prefilled when the initiator is the one fetching it.
    let buffer = vec![if serve_read { PATTERN } else { 0 }; BUFFER_LEN];
    let pointer = buffer.as_ptr().cast_mut();
    let mut target = passive_target::Target::open(use_efa, bind.as_deref())?;
    // SAFETY: `buffer` is declared before `target`, so it outlives the registration.
    let exposed = unsafe { target.expose(pointer, BUFFER_LEN, true)? };
    let address = target.address()?;
    if let Some(initiator) = &initiator {
        let bytes = decode_hex(initiator.as_bytes()).map_err(|error| error.to_string())?;
        target.insert_initiator(&bytes)?;
    }

    println!(
        "advertisement: {} {} {}",
        encode_hex(&address),
        exposed.remote_key,
        exposed.remote_address
    );
    if serve_read {
        println!(
            "serving {BUFFER_LEN} bytes of {PATTERN:#04x} for a remote read, crc {:#010x} — the initiator verifies",
            checksum(&buffer)
        );
    } else {
        println!("waiting for a transfer into {BUFFER_LEN} bytes...");
    }

    // Under FI_PROGRESS_MANUAL nothing services the inbound RMA unless the queue is polled, and the
    // write raises no completion here — so poll for progress, watch the buffer for arrival.
    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        target.progress();
        // A read leaves nothing behind here, so there is no arrival to watch for: hold the buffer
        // open and keep polling until the window closes.
        if serve_read {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        // Volatile: the NIC writes these bytes outside the compiler's model.
        // SAFETY: in bounds of the live registered buffer.
        let landed =
            (0..BUFFER_LEN).all(|index| PATTERN == unsafe { pointer.add(index).read_volatile() });
        if landed {
            // The CRC the initiator also prints, so the pair cross-checks.
            println!(
                "received {BUFFER_LEN} bytes, crc {:#010x} — payload verified",
                checksum(&buffer)
            );
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    if serve_read {
        return Ok(());
    }
    Err("timed out waiting for a transfer".into())
}
