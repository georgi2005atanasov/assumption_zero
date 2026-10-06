use crate::{
    domain::Decision,
    runtime::{Ledger, Runtime},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
static TRACK: AtomicBool = AtomicBool::new(false);
static ALLOCATED: AtomicU64 = AtomicU64::new(0);
pub struct CountingAllocator;
// Counts requested allocation bytes, including reallocations; not resident memory.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() && TRACK.load(Ordering::Relaxed) {
            ALLOCATED.fetch_add(l.size() as u64, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, n) };
        if !q.is_null() && TRACK.load(Ordering::Relaxed) {
            ALLOCATED.fetch_add(n as u64, Ordering::Relaxed);
        }
        q
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() && TRACK.load(Ordering::Relaxed) {
            ALLOCATED.fetch_add(l.size() as u64, Ordering::Relaxed);
        }
        p
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;
pub fn track_allocations() {
    TRACK.store(true, Ordering::Relaxed);
}
pub fn allocated() -> u64 {
    ALLOCATED.load(Ordering::Relaxed)
}
fn clock(id: libc::clockid_t) -> i64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(unsafe { libc::clock_gettime(id, &mut t) }, 0);
    t.tv_sec * 1_000_000_000 + t.tv_nsec
}
pub fn ticks() -> i64 {
    clock(libc::CLOCK_MONOTONIC)
}
fn cpu() -> i64 {
    clock(libc::CLOCK_PROCESS_CPUTIME_ID)
}
#[derive(Default, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Frame {
    pub index: usize,
    pub message_type: i32,
    pub bytes: usize,
    pub template_id: u16,
    pub receive_ticks: i64,
    pub processing_end_ticks: i64,
    pub last_decision_ticks: i64,
    pub decisions: usize,
    pub decode_ticks: i64,
    pub book_ticks: i64,
    pub signal_ticks: i64,
    pub quote_ticks: i64,
    pub gc_pause_overlap: bool,
}
#[derive(Default, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Latency {
    pub count: usize,
    pub p50: i64,
    pub p90: i64,
    pub p95: i64,
    pub p99: i64,
    pub p999: i64,
    pub maximum: i64,
}
impl Latency {
    pub fn from(mut v: Vec<i64>) -> Self {
        if v.is_empty() {
            return Self::default();
        }
        v.sort_unstable();
        let at = |p: f64| v[(v.len() as f64 * p).ceil() as usize - 1];
        Self {
            count: v.len(),
            p50: at(0.5),
            p90: at(0.9),
            p95: at(0.95),
            p99: at(0.99),
            p999: at(0.999),
            maximum: *v.last().unwrap(),
        }
    }
}
pub const OFFSET: u64 = 14695981039346656037;
pub fn mix(hash: &mut u64, x: i64) {
    *hash = (*hash ^ x as u64).wrapping_mul(1099511628211);
}
pub struct Benchmark {
    pub frames: Vec<Frame>,
    pub warmup: usize,
    expected: usize,
    start_cpu: i64,
    start_alloc: u64,
    hash: u64,
    total_decisions: usize,
}
impl Benchmark {
    pub fn new(expected: usize, warmup: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            expected > 0 && warmup < expected,
            "invalid benchmark frame counts"
        );
        track_allocations();
        let mut b = Self {
            frames: Vec::with_capacity(expected),
            warmup,
            expected,
            start_cpu: 0,
            start_alloc: 0,
            hash: OFFSET,
            total_decisions: 0,
        };
        if warmup == 0 {
            b.capture();
        }
        Ok(b)
    }
    fn capture(&mut self) {
        self.start_cpu = cpu();
        self.start_alloc = allocated();
    }
    pub fn begin(&mut self, kind: i32, bytes: usize, receive: i64) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.frames.len() < self.expected,
            "more frames than expected"
        );
        self.frames.push(Frame {
            index: self.frames.len() + 1,
            message_type: kind,
            bytes,
            receive_ticks: receive,
            ..Frame::default()
        });
        Ok(())
    }
    pub fn frame(&mut self) -> &mut Frame {
        self.frames.last_mut().unwrap()
    }
    pub fn decision(&mut self, id: i64, d: Decision) {
        let f = self.frame();
        f.decisions += 1;
        f.last_decision_ticks = ticks();
        self.total_decisions += 1;
        for x in d.values(id) {
            mix(&mut self.hash, x);
        }
    }
    pub fn end(&mut self) {
        self.frame().processing_end_ticks = ticks();
        if self.frames.len() == self.warmup {
            self.capture();
        }
    }
    pub fn finish(&self, completed: bool, state_hash: &str) -> Value {
        let cpu_ns = if self.start_cpu == 0 {
            0
        } else {
            cpu() - self.start_cpu
        };
        let allocation = if self.start_cpu == 0 {
            0
        } else {
            allocated() - self.start_alloc
        };
        let measured = &self.frames[self.warmup.min(self.frames.len())..];
        let duration = measured
            .first()
            .map(|f| measured.last().unwrap().processing_end_ticks - f.receive_ticks)
            .unwrap_or(0);
        let latency = |select: fn(&Frame) -> i64, filter: bool| {
            Latency::from(
                measured
                    .iter()
                    .map(select)
                    .filter(|x| !filter || *x > 0)
                    .collect(),
            )
        };
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let memory = |field: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(field))
                .and_then(|l| l.split_whitespace().next()?.parse::<u64>().ok())
                .map(|x| x * 1024)
                .unwrap_or(0)
        };
        json!({"SchemaVersion":3,"BuildConfiguration":if cfg!(debug_assertions){"Debug"}else{"Release"},"ServerGc":null,"GcLatencyMode":null,"LogicalCpus":std::thread::available_parallelism().map(|n|n.get()).unwrap_or(1),"Completed":completed,"ClockFrequency":1_000_000_000i64,"WarmupFrames":self.warmup,"ReceivedFrames":self.frames.len(),"TotalDecisions":self.total_decisions,"DecisionHash":format!("{:016x}",self.hash),"FinalStateHash":state_hash,
            "MeasuredFrames":measured.len(),"MeasuredDecisions":measured.iter().map(|f|f.decisions).sum::<usize>(),"MeasuredBytes":measured.iter().map(|f|f.bytes).sum::<usize>(),"MeasuredDurationNanoseconds":duration,
            "ProcessingNanoseconds":latency(|f|f.processing_end_ticks-f.receive_ticks,false),"ReceiveToDecisionNanoseconds":latency(|f|if f.last_decision_ticks==0{0}else{f.last_decision_ticks-f.receive_ticks},true),
            "DecodeNanoseconds":latency(|f|f.decode_ticks,true),"BookNanoseconds":latency(|f|f.book_ticks,true),"SignalNanoseconds":latency(|f|f.signal_ticks,true),"QuoteNanoseconds":latency(|f|f.quote_ticks,true),
            "CpuNanoseconds":cpu_ns,"CpuCores":if duration==0{0.0}else{cpu_ns as f64/duration as f64},"CpuNanosecondsPerFrame":if measured.is_empty(){0.0}else{cpu_ns as f64/measured.len() as f64},"MemorySnapshotPhase":"after_replay_before_report","ResidentBytes":memory("VmRSS:"),"PeakResidentBytes":memory("VmHWM:"),
            "ManagedHeapBytes":null,"ManagedAllocatedBytes":null,"ManagedAllocatedBytesPerFrame":null,"GcCollections":null,"GcPauseNanoseconds":null,"IndividualGcPauseNanoseconds":Latency::default(),"Diagnostic":false,"GcPauses":[],"Frames":self.frames,
            "AllocatorAllocatedBytes":allocation,"AllocatorAllocatedBytesPerFrame":if measured.is_empty(){0.0}else{allocation as f64/measured.len() as f64},"AllocationScope":"process requested allocator bytes, including reallocations; measured window"})
    }
}
// Canonical discrete state hash, in configured instrument order. Floating intermediates
// are excluded; exact quote output and book/inventory/order state remain mandatory.
pub fn final_state(rs: &[Runtime], a: &Ledger) -> String {
    let mut h = OFFSET;
    mix(&mut h, rs.len() as i64);
    mix(&mut h, a.ready as i64);
    for r in rs {
        for x in [
            r.instrument.id,
            r.book.last_id,
            r.book.synchronized as i64,
            r.memory.warm as i64,
            r.memory.faulted as i64,
            r.inventory.current,
            r.inventory.target,
            r.inventory.max_long,
            r.inventory.max_short,
            r.inventory.reserved_buy,
            r.inventory.reserved_sell,
        ] {
            mix(&mut h, x);
        }
        for side in [&r.book.bids, &r.book.asks] {
            mix(&mut h, side.len() as i64);
            for l in side {
                mix(&mut h, l.price_ticks);
                mix(&mut h, l.quantity);
            }
        }
        for o in r.orders {
            for x in [
                o.id,
                o.price,
                o.quantity,
                o.filled,
                o.status as i64,
                o.active as i64,
            ] {
                mix(&mut h, x);
            }
        }
    }
    format!("{h:016x}")
}
