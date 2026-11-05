//! A tester for the gap between a payload becoming visible in the target's memory and the "done"
//! signal reaching it over tcp, through a different NIC than the data.
//!
//! EFA only promises it has *initiated* the posted writes when it claims an fi_writemsg is complete.
//! So a signal carried by another device, like vdma's RESP reply over the ENA, is ordered relative
//! to the data by timing (when quiesce_writes is off). This measures that timing.
//!
//! Each iteration the initiator writes a fresh generation number into the target's buffer, and its
//! completion hook sends that generation over tcp when the completion is reaped. It's bare, to
//! be more pessimistic about the gap than RESP or a valkey GIL would provide. On the target, one
//! thread spins on the buffer and remembers when it sees each generation. The main thread timestamps
//! the signal's arrival and then rescans the buffer, tail first. Both stamps are on the target's
//! clock, so no clock sync is needed.
//!
//! - `margin = signal arrival - last read of old data`. The watcher stamps before each read that
//!   finds old data, so a stalled watcher makes the margin look larger.
//!   Negative means it read old data after the signal arrived, which would demonstrate the race,
//!   if observed.
//! - `window = first full read of new data - last read of old data`. The uncertainty during when
//!   the new data arrived. Usually small, but if there was a spurious thread reschedule or stall
//!   this helps avoid reporting false passes or false failures.
//! - `stale` counts iterations where the post-signal rescan still found an old number. If observer,
//!   this would demonstrate the race directly with no timing involved.
//!
//! ```text
//! # instance B, the passive target
//! cargo run --release -p dma-libfabric --example delivery_margin -- target 0.0.0.0:7700
//! # instance C, the initiator
//! cargo run --release -p dma-libfabric --example delivery_margin -- initiator <B-ip>:7700
//! ```
//!
//! `--quiesce` turns on `quiesce_writes`, so each signal waits on a read back of the write: the
//! margin then includes that round trip, and the initiator's timing shows its cost. A target run
//! with `--write-only` exposes its buffer without `FI_REMOTE_READ`, so a quiescing initiator's
//! first transfer must fail.
//!
//! `--csv <path> --label <name>` appends each distribution, the target's margins and the
//! initiator's submit-to-completion latencies, as quantile rows for plotting.
//!
//! `--sizes 64,4096,1048576` and `--iterations 10000` tune the initiator's sweep; `--capacity
//! <bytes>` sizes the target's buffer, 64 MiB by default. `--pin <watcher>,<signal>` pins the
//! target's two threads to those cores.
//!
//! Observing zero stale data and only positive margins is not a guarantee that you can never
//! lose the race.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dma_libfabric::{
    Completion, Configuration, Direction, FabricService, MemoryRegion, Operands, Outcome, Pool,
    Provider, TransferRequest,
};

mod passive_target;

/// One payload word per u64, sizes in whole 64-byte lines, and one probe per page.
const WORD: usize = std::mem::size_of::<u64>();
const LINE: usize = 64;
const PAGE: usize = 4096;
/// A write that hasn't landed by then has failed, not raced.
const LANDING_DEADLINE: Duration = Duration::from_secs(5);

fn io(what: &'static str) -> impl Fn(std::io::Error) -> String {
    move |error| format!("{what}: {error}")
}

fn write_u64(stream: &mut TcpStream, value: u64) -> Result<(), String> {
    stream.write_all(&value.to_le_bytes()).map_err(io("write"))
}

fn read_u64(stream: &mut TcpStream) -> Result<u64, String> {
    let mut bytes = [0; WORD];
    stream.read_exact(&mut bytes).map_err(io("read"))?;
    Ok(u64::from_le_bytes(bytes))
}

fn write_bytes(stream: &mut TcpStream, bytes: &[u8]) -> Result<(), String> {
    write_u64(stream, bytes.len() as u64)?;
    stream.write_all(bytes).map_err(io("write"))
}

fn read_bytes(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let length = usize::try_from(read_u64(stream)?).map_err(|error| error.to_string())?;
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).map_err(io("read"))?;
    Ok(bytes)
}

/// The ack each side waits on: the initiator never writes a generation until the target is done
/// measuring the last one, so the watcher is never overrun.
fn ack(stream: &mut TcpStream) -> Result<(), String> {
    stream.write_all(&[1]).map_err(io("ack"))
}

fn await_ack(stream: &mut TcpStream) -> Result<(), String> {
    let mut byte = [0];
    stream.read_exact(&mut byte).map_err(io("await ack"))
}

fn flag<'a>(arguments: &'a [String], name: &str) -> Option<&'a str> {
    arguments
        .iter()
        .position(|argument| name == argument)
        .and_then(|index| arguments.get(index + 1))
        .map(String::as_str)
}

fn main() -> Result<(), String> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match (
        arguments.first().map(String::as_str),
        arguments.get(1).map(String::as_str),
    ) {
        (Some("target"), Some(listen)) => {
            let capacity = match flag(&arguments, "--capacity") {
                Some(capacity) => capacity.parse().map_err(|_| "--capacity <bytes>")?,
                None => 64 << 20,
            };
            let pins = match flag(&arguments, "--pin") {
                Some(cores) => match cores
                    .split_once(',')
                    .map(|(watcher, signal)| (watcher.parse::<usize>(), signal.parse::<usize>()))
                {
                    Some((Ok(watcher), Ok(signal))) => Some((watcher, signal)),
                    _ => return Err("--pin <watcher-core>,<signal-core>".into()),
                },
                None => None,
            };
            let write_only = arguments.iter().any(|argument| "--write-only" == argument);
            target(
                listen,
                capacity,
                pins,
                write_only,
                &Csv::from_arguments(&arguments),
            )
        }
        (Some("initiator"), Some(target)) => {
            let sizes = match flag(&arguments, "--sizes") {
                Some(sizes) => sizes
                    .split(',')
                    .map(str::parse)
                    .collect::<Result<Vec<usize>, _>>()
                    .map_err(|_| "--sizes <bytes>,<bytes>,...")?,
                None => vec![64, 4096, 65536, 1 << 20, 16 << 20],
            };
            let iterations = match flag(&arguments, "--iterations") {
                Some(iterations) => iterations.parse().map_err(|_| "--iterations <count>")?,
                None => 10_000,
            };
            let quiesce_writes = arguments.iter().any(|argument| "--quiesce" == argument);
            initiator(
                target,
                &sizes,
                iterations,
                quiesce_writes,
                &Csv::from_arguments(&arguments),
            )
        }
        _ => Err("usage: delivery_margin target <listen> | initiator <target>".into()),
    }
}

/// The initiator's payload, rewritten in place between transfers. Registered once, as vdma's cached
/// operands are: a per-op registration is closed before the completion hook runs, which would put an
/// `ibv_dereg_mr` between every completion and its signal.
struct Words(Box<[AtomicU64]>);

impl AsRef<[u8]> for Words {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: `AtomicU64` has `u64`'s size and alignment, and the words are stored only between
        // transfers, while no transfer holds this slice.
        unsafe {
            std::slice::from_raw_parts(self.0.as_ptr().cast(), std::mem::size_of_val(&*self.0))
        }
    }
}

/// The per-op context. Comes back at completion so the next iteration reuses it.
struct Generation {
    number: u64,
    payload: MemoryRegion<Words>,
    length: usize,
}

impl Operands for Generation {
    fn source(&self) -> Option<&[u8]> {
        self.payload.storage().as_ref().get(..self.length)
    }
}

fn initiator(
    target: &str,
    sizes: &[usize],
    iterations: u64,
    quiesce_writes: bool,
    csv: &Csv,
) -> Result<(), String> {
    let mut control = TcpStream::connect(target).map_err(io("connect"))?;
    control.set_nodelay(true).map_err(io("nodelay"))?;
    let target_address = read_bytes(&mut control)?;
    let remote_key = read_u64(&mut control)?;
    let remote_address = read_u64(&mut control)?;
    let capacity = read_u64(&mut control)?;

    // The completion hook signals straight from the fabric worker: the tightest a server could
    // follow a completion with a reply.
    let signal = Mutex::new(control.try_clone().map_err(io("clone"))?);
    let (returned, finished) = channel::<(Generation, Outcome)>();
    let complete = move |completions: std::vec::Drain<'_, Completion<Generation>>| {
        for completion in completions {
            if completion.outcome.is_ok()
                && let Ok(mut signal) = signal.lock()
            {
                // A failed signal surfaces as the target's missing ack.
                let _ = signal.write_all(&completion.caller_context.number.to_le_bytes());
            }
            let _ = returned.send((completion.caller_context, completion.outcome));
        }
    };
    let configuration = Configuration {
        providers: vec![Provider::EfaDirect],
        quiesce_writes,
        ..Configuration::default()
    };
    let pool = Arc::new(Pool::new(1).map_err(|error| format!("pool: {error}"))?);
    let server = FabricService::start(&configuration, Box::new(complete), pool)
        .map_err(|error| error.to_string())?;
    write_bytes(&mut control, server.local_address())?;
    await_ack(&mut control)?;

    let largest = sizes.iter().copied().max().unwrap_or_default();
    let payload = server
        .register(Words(
            std::iter::repeat_with(|| AtomicU64::new(0))
                .take(largest / WORD)
                .collect(),
        ))
        .map_err(|error| error.to_string())?;
    let mut next = Generation {
        number: 0,
        payload,
        length: 0,
    };
    for &size in sizes {
        if 0 == size || !size.is_multiple_of(LINE) || capacity < size as u64 {
            return Err(format!(
                "size {size} must be a nonzero multiple of {LINE} within {capacity}"
            ));
        }
        write_u64(&mut control, size as u64)?;
        write_u64(&mut control, iterations)?;
        write_u64(&mut control, next.number + 1)?;
        await_ack(&mut control)?;
        let started = Instant::now();
        // Submit to completion hook: the fabric's share of a GET, quiesce read included.
        let mut latencies = Vec::with_capacity(iterations as usize);
        for _ in 0..iterations {
            next.number += 1;
            next.length = size;
            for word in next.payload.storage().0.iter().take(size / WORD) {
                word.store(next.number, Ordering::Relaxed);
            }
            let request = TransferRequest {
                client_id: 1,
                peer_address: target_address.clone(),
                remote_key,
                remote_address,
                direction: Direction::ToPeer,
                want_checksum: false,
                caller_context: next,
                parent_id: None,
            };
            let submitted = Instant::now();
            if server.submit(request).is_err() {
                return Err("fabric worker is gone".into());
            }
            let (back, outcome) = finished
                .recv()
                .map_err(|_| "fabric worker exited mid-transfer")?;
            latencies.push(submitted.elapsed().as_nanos() as i64);
            outcome.map_err(|error| format!("generation {}: {error}", back.number))?;
            next = back;
            await_ack(&mut control)?;
        }
        let latencies = Distribution::new(latencies);
        println!(
            "{size:>10} bytes  latency µs  {}  ({iterations} writes in {:.2?})",
            latencies.summary(),
            started.elapsed()
        );
        csv.append("latency", size, &latencies)?;
    }
    write_u64(&mut control, 0)
}

/// Pin the calling thread to one core, so a descheduled watcher can't stamp late and read as a race.
#[cfg(target_os = "linux")]
fn pin(core: usize) -> Result<(), String> {
    // SAFETY: a zeroed cpu_set_t is the empty set, and pid 0 is the calling thread.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(core, &mut set);
        if 0 != libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) {
            return Err(format!(
                "pin to core {core}: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn pin(_core: usize) -> Result<(), String> {
    Err("--pin needs linux".into())
}

fn target(
    listen: &str,
    capacity: usize,
    pins: Option<(usize, usize)>,
    write_only: bool,
    csv: &Csv,
) -> Result<(), String> {
    if !capacity.is_multiple_of(LINE) {
        return Err(format!("--capacity must be a multiple of {LINE}"));
    }
    if let Some((_, signal)) = pins {
        pin(signal)?;
    }
    // Atomics so the spinning loads are never hoisted out of the loop; the NIC writes these words
    // outside the compiler's model.
    let buffer: Vec<AtomicU64> = std::iter::repeat_with(|| AtomicU64::new(0))
        .take(capacity / WORD)
        .collect();
    let mut target = passive_target::Target::open(true, None)?;
    // SAFETY: `buffer` is declared before `target`, so it outlives the registration.
    let exposed =
        unsafe { target.expose(buffer.as_ptr().cast_mut().cast(), capacity, !write_only)? };

    let listener = TcpListener::bind(listen).map_err(io("bind"))?;
    println!("listening on {listen} with {capacity} bytes exposed");
    let (mut control, peer) = listener.accept().map_err(io("accept"))?;
    control.set_nodelay(true).map_err(io("nodelay"))?;
    println!("initiator {peer}");
    write_bytes(&mut control, &target.address()?)?;
    write_u64(&mut control, exposed.remote_key)?;
    write_u64(&mut control, exposed.remote_address)?;
    write_u64(&mut control, capacity as u64)?;
    target.insert_initiator(&read_bytes(&mut control)?)?;
    ack(&mut control)?;

    loop {
        let size = usize::try_from(read_u64(&mut control)?).map_err(|error| error.to_string())?;
        if 0 == size {
            return Ok(());
        }
        let iterations = read_u64(&mut control)?;
        let first = read_u64(&mut control)?;
        let words = buffer.get(..size / WORD).ok_or("size exceeds capacity")?;
        let report = std::thread::scope(|scope| {
            let (visible, landed) = channel();
            scope.spawn(move || {
                if let Some((watcher, _)) = pins
                    && let Err(error) = pin(watcher)
                {
                    // Dropping `visible` unwatched fails the batch as "watcher exited".
                    eprintln!("{error}");
                    return;
                }
                watch(words, first, iterations, &visible)
            });
            ack(&mut control)?;
            measure(&mut control, words, first, iterations, &landed)
        })?;
        report.print(size, csv)?;
    }
}

/// When a generation became fully visible lies between these two reads. A stalled watcher only
/// widens the window: a stall can delay `first_new`, never `last_old`.
struct Landing {
    /// Stamped before the last read that still found an old word, so the payload landed after it.
    last_old: Instant,
    /// Stamped after the read that found the last new word, so the payload landed before it.
    first_new: Instant,
}

/// Spin until every page's last word shows each generation in turn, and bracket when the last one
/// did. A word never reverts once it shows the generation, so the cursor only moves forward. `None`
/// when a generation never fully lands.
fn watch(
    words: &[AtomicU64],
    first: u64,
    iterations: u64,
    visible: &Sender<(u64, Option<Landing>)>,
) {
    for generation in first..first + iterations {
        let deadline = Instant::now() + LANDING_DEADLINE;
        let mut probes = words.chunks(PAGE / WORD).filter_map(<[AtomicU64]>::last);
        let mut pending = probes.next();
        // The initiator writes only after the ack, so the first read below always finds old data.
        let mut last_old = Instant::now();
        let landing = loop {
            let Some(word) = pending else {
                break Some(Landing {
                    last_old,
                    first_new: Instant::now(),
                });
            };
            let before = Instant::now();
            if generation == word.load(Ordering::Relaxed) {
                pending = probes.next();
            } else if deadline < before {
                break None;
            } else {
                last_old = before;
                std::hint::spin_loop();
            }
        };
        if visible.send((generation, landing)).is_err() {
            return;
        }
    }
}

#[derive(Default)]
struct Report {
    /// Nanoseconds from the watcher's last sight of old data to the signal arriving. Negative only
    /// when the watcher still saw old data after the signal: a definite race.
    margins: Vec<i64>,
    /// Nanoseconds between the last old read and the first full new one: how far a stalled watcher
    /// widened the window.
    uncertainties: Vec<i64>,
    /// Iterations whose post-signal rescan found an old word.
    stale: u64,
    /// Words found old, across those iterations.
    stale_words: u64,
    /// Generations that never fully landed.
    lost: u64,
}

fn measure(
    control: &mut TcpStream,
    words: &[AtomicU64],
    first: u64,
    iterations: u64,
    landed: &std::sync::mpsc::Receiver<(u64, Option<Landing>)>,
) -> Result<Report, String> {
    let mut report = Report::default();
    for expected in first..first + iterations {
        let generation = read_u64(control)?;
        let arrived = Instant::now();
        if expected != generation {
            return Err(format!("expected generation {expected}, got {generation}"));
        }
        let old = words
            .iter()
            .rev()
            .filter(|word| generation != word.load(Ordering::Relaxed))
            .count() as u64;
        if 0 < old {
            report.stale += 1;
            report.stale_words += old;
        }
        let (watched, landing) = landed
            .recv_timeout(LANDING_DEADLINE + Duration::from_secs(1))
            .map_err(|_| "watcher exited")?;
        if watched != generation {
            return Err(format!("watcher on generation {watched}, not {generation}"));
        }
        match landing {
            Some(landing) => {
                report
                    .margins
                    .push(signed_nanoseconds(landing.last_old, arrived));
                report
                    .uncertainties
                    .push(signed_nanoseconds(landing.last_old, landing.first_new));
            }
            None => report.lost += 1,
        }
        ack(control)?;
    }
    Ok(report)
}

/// `to - from`, negative when `to` came first.
fn signed_nanoseconds(from: Instant, to: Instant) -> i64 {
    if from <= to {
        (to - from).as_nanos() as i64
    } else {
        -((from - to).as_nanos() as i64)
    }
}

impl Report {
    fn print(self, size: usize, csv: &Csv) -> Result<(), String> {
        let raced = self.margins.iter().filter(|margin| **margin < 0).count();
        let margins = Distribution::new(self.margins);
        let uncertainties = Distribution::new(self.uncertainties);
        println!(
            "{size:>10} bytes  margin µs  {}  raced {raced}  stale {} ({} words)  lost {}",
            margins.summary(),
            self.stale,
            self.stale_words,
            self.lost,
        );
        println!("{size:>10} bytes  window µs  {}", uncertainties.summary());
        csv.append("margin", size, &margins)?;
        csv.append("window", size, &uncertainties)
    }
}

/// A sorted sample of nanoseconds.
struct Distribution(Vec<i64>);

impl Distribution {
    fn new(mut samples: Vec<i64>) -> Self {
        samples.sort_unstable();
        Self(samples)
    }

    fn quantile(&self, quantile: f64) -> i64 {
        let last = self.0.len().saturating_sub(1);
        let index = (quantile * last as f64).round() as usize;
        self.0.get(index).copied().unwrap_or_default()
    }

    fn summary(&self) -> String {
        let microseconds = |quantile| self.quantile(quantile) as f64 / 1000.0;
        format!(
            "min {:>8.2}  p1 {:>8.2}  p50 {:>8.2}  p99 {:>8.2}  p99.9 {:>8.2}  max {:>8.2}",
            microseconds(0.0),
            microseconds(0.01),
            microseconds(0.5),
            microseconds(0.99),
            microseconds(0.999),
            microseconds(1.0),
        )
    }
}

/// Where `--csv` sends each distribution, as `label,series,size,quantile,nanoseconds` rows: every
/// thousandth quantile, plus the far tail a million samples can resolve.
struct Csv {
    path: Option<String>,
    label: String,
}

impl Csv {
    fn from_arguments(arguments: &[String]) -> Self {
        Self {
            path: flag(arguments, "--csv").map(str::to_string),
            label: flag(arguments, "--label").unwrap_or("run").to_string(),
        }
    }

    fn append(&self, series: &str, size: usize, distribution: &Distribution) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let quantiles = (0..=1000)
            .map(|step| f64::from(step) / 1000.0)
            .chain([0.9995, 0.9999, 0.99995, 0.99999]);
        let mut rows = String::new();
        for quantile in quantiles {
            rows.push_str(&format!(
                "{},{series},{size},{quantile},{}\n",
                self.label,
                distribution.quantile(quantile)
            ));
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(rows.as_bytes()))
            .map_err(io("csv"))
    }
}
