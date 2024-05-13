use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};

use std::time::SystemTime;
use tokio::sync::{Mutex, MutexGuard, RwLock};

use backtrace::Frame;
use pprof::protos::Message;
use thiserror::Error;

use crate::collector;

const MAX_DEPTH: usize = 32;

static HEAP_PROFILER_ENABLED: AtomicBool = AtomicBool::new(false);

lazy_static::lazy_static! {
    static ref HEAP_PROFILER_STATE: RwLock<ProfilerState<MAX_DEPTH>> = RwLock::new(Default::default());
    static ref HEAP_PROFILER_ENTER: Mutex<()> = Mutex::new(());
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("attempting to run a heap profiler while the another heap profiler is being run")]
    ConcurrentHeapProfiler,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// RAII structure used to stop profiling when dropped. It is the only interface to access the heap profiler.
pub struct HeapProfilerGuard {
    _guard: MutexGuard<'static, ()>,
}

impl HeapProfilerGuard {
    pub async fn new(period: usize) -> Result<Self> {
        let guard = HEAP_PROFILER_ENTER.lock().await;
        Profiler::start(period).await;
        Ok(Self { _guard: guard })
    }

    pub async fn report(self) -> HeapReport {
        Profiler::stop();
        HeapReport::new().await
    }
}

impl Drop for HeapProfilerGuard {
    fn drop(&mut self) {
        Profiler::stop();
    }
}

#[derive(Clone)]
struct ProfilerBuffer {
    allocated_objects: isize,
    allocated_bytes: isize,
    freed_objects: isize,
    freed_bytes: isize,
    next_sample: isize,
    period: usize,
}

impl ProfilerBuffer {
    fn new() -> Self {
        Self {
            allocated_objects: 0,
            allocated_bytes: 0,
            freed_objects: 0,
            freed_bytes: 0,
            next_sample: 1024 * 1024,
            period: 1024 * 1024,
        }
    }

    fn track(&mut self, size: isize) {
        if size > 0 {
            self.allocated_objects += 1;
            self.allocated_bytes += size;
        } else if size < 0 {
            self.freed_objects += 1;
            self.freed_bytes += -size;
        }

        // Check if either allocated bytes or freed bytes cross the threshold
        if self.allocated_bytes >= self.next_sample || self.freed_bytes >= self.next_sample {
            self.next_sample += self.period as isize;
        }
    }

    fn should_flush(&self) -> bool {
        self.allocated_bytes >= self.next_sample || self.freed_bytes >= self.next_sample
    }

    unsafe fn flush(&mut self, profiler: &mut ProfilerState<MAX_DEPTH>) {
        let current_net_change = self.allocated_bytes - self.freed_bytes;

        // Update profiler state
        profiler.allocated_objects += self.allocated_objects;
        profiler.allocated_bytes += self.allocated_bytes;
        profiler.freed_objects += self.freed_objects;
        profiler.freed_bytes += self.freed_bytes;

        // Check if the net change since the last sample is significant
        if current_net_change.abs() >= self.next_sample {
            // Adjust the next_sample based on current activity
            self.next_sample = current_net_change.abs() + self.period as isize;
        }

        // Record backtrace and other data if there was a significant net change
        if current_net_change != 0 {
            let mut bt = Frames::new();
            backtrace::trace_unsynchronized(|frame| bt.push(frame));
            profiler.collector.record(bt, current_net_change);
        }
    }

    fn reset_buffer(&mut self) {
        // Reset local buffer
        self.allocated_objects = 0;
        self.allocated_bytes = 0;
        self.freed_objects = 0;
        self.freed_bytes = 0;
    }
}

// Called by malloc hooks to record a memory allocation event.
pub struct Profiler;

impl Profiler {
    fn enabled() -> bool {
        HEAP_PROFILER_ENABLED.load(Ordering::SeqCst)
    }

    fn set_enabled(value: bool) {
        HEAP_PROFILER_ENABLED.store(value, Ordering::SeqCst)
    }

    async fn start(period: usize) {
        let mut profiler = HEAP_PROFILER_STATE.write().await;
        *profiler = ProfilerState::new(period);
        std::mem::drop(profiler);

        Self::set_enabled(true);
    }

    fn stop() {
        Self::set_enabled(false);
    }

    pub(crate) unsafe fn track_allocated(size: isize) {
        thread_local!(static ENTERED: Cell<bool> = Cell::new(false));
        thread_local!(static BUFFER: std::sync::Mutex<ProfilerBuffer> = std::sync::Mutex::new(ProfilerBuffer::new()));

        struct ResetOnDrop;

        impl Drop for ResetOnDrop {
            fn drop(&mut self) {
                ENTERED.with(|b| b.set(false));
            }
        }

        ENTERED.with(|entered| {
            let mut entered_guard = entered.get();
            if !entered_guard {
                entered_guard = true;
                let _reset_on_drop = ResetOnDrop;
                if Self::enabled() {
                    BUFFER.with(|buffer| {
                        let mut buffer = buffer.lock().unwrap();
                        buffer.track(size);

                        if buffer.should_flush() {
                            // Flush asynchronously only when needed
                            let mut passed_buffer: ProfilerBuffer = buffer.clone();
                            buffer.reset_buffer();
                            tokio::spawn(async move {
                                let mut profiler = HEAP_PROFILER_STATE.write().await;
                                passed_buffer.flush(&mut profiler);
                            });
                        }
                    });
                }
            }
        });
    }

    // Called by malloc hooks to record a memory allocation event.
}

#[derive(Debug)]
pub struct HeapReport {
    data: HashMap<pprof::Frames, collector::MemProfileRecord>,
    period: usize,
}

impl HeapReport {
    async fn new() -> Self {
        let mut profiler = HEAP_PROFILER_STATE.write().await;
        let collector = std::mem::take(&mut profiler.collector);

        let data = collector
            .into_iter()
            .map(|(frames, rec)| (frames.into(), rec))
            .collect();
        Self {
            data,
            period: profiler.period,
        }
    }

    /// flamegraph will write an svg flamegraph into writer.
    pub fn flamegraph<W>(&self, writer: W)
    where
        W: Write,
    {
        // the pprof crate already has all the necessary plumbing for the embedded flamegraph library, let's just render
        // the alloc_bytes stat with it.
        let data = self
            .data
            .iter()
            .map(|(frames, rec)| (frames.clone(), rec.alloc_bytes))
            .collect();

        let timing = Default::default();

        let report = pprof::Report { data, timing };

        let mut options: pprof::flamegraph::Options = Default::default();

        options.count_name = "bytes".to_string();
        options.colors =
            pprof::flamegraph::color::Palette::Basic(pprof::flamegraph::color::BasicPalette::Mem);

        report
            .flamegraph_with_options(writer, &mut options)
            .unwrap();
    }

    fn inner_pprof(&self) -> pprof::protos::Profile {
        use pprof::protos;
        let data = self.data.clone();

        let mut dudup_str = HashSet::new();
        for key in data.iter().map(|(key, _)| key) {
            for frame in key.frames.iter() {
                for symbol in frame {
                    dudup_str.insert(symbol.name());
                    dudup_str.insert(symbol.sys_name().into_owned());
                    dudup_str.insert(symbol.filename().into_owned());
                }
            }
        }
        // string table's first element must be an empty string
        let mut string_table = vec!["".to_owned()];
        string_table.extend(dudup_str.into_iter());

        let mut strings = HashMap::new();
        for (index, name) in string_table.iter().enumerate() {
            strings.insert(name.as_str(), index);
        }

        let mut samples = vec![];
        let mut loc_tbl = vec![];
        let mut fn_tbl = vec![];
        let mut functions = HashMap::new();
        for (key, rec) in data.iter() {
            let mut locs = vec![];
            for frame in key.frames.iter() {
                for symbol in frame {
                    let name = symbol.name();
                    if let Some(loc_idx) = functions.get(&name) {
                        locs.push(*loc_idx);
                        continue;
                    }
                    let sys_name = symbol.sys_name();
                    let filename = symbol.filename();
                    let lineno = symbol.lineno();
                    let function_id = fn_tbl.len() as u64 + 1;
                    let function = protos::Function {
                        id: function_id,
                        name: *strings.get(name.as_str()).unwrap() as i64,
                        system_name: *strings.get(sys_name.as_ref()).unwrap() as i64,
                        filename: *strings.get(filename.as_ref()).unwrap() as i64,
                        ..protos::Function::default()
                    };
                    functions.insert(name, function_id);
                    let line = protos::Line {
                        function_id,
                        line: lineno as i64,
                    };
                    let loc = protos::Location {
                        id: function_id,
                        line: vec![line],
                        ..protos::Location::default()
                    };
                    // the fn_tbl has the same length with loc_tbl
                    fn_tbl.push(function);
                    loc_tbl.push(loc);
                    // current frame locations
                    locs.push(function_id);
                }
            }
            let sample = protos::Sample {
                location_id: locs,
                #[cfg(feature = "measure_free")]
                value: vec![
                    rec.alloc_objects as i64,
                    rec.alloc_bytes as i64,
                    rec.free_objects as i64,
                    rec.free_bytes as i64,
                    rec.in_use_objects() as i64,
                    rec.in_use_bytes() as i64,
                ],
                #[cfg(not(feature = "measure_free"))]
                value: vec![rec.alloc_objects as i64, rec.alloc_bytes as i64],
                ..protos::Sample::default()
            };
            samples.push(sample);
        }

        let mut push_string = |s: &str| {
            let idx = string_table.len();
            string_table.push(s.to_string());
            idx as i64
        };

        let alloc_objects_idx = push_string("alloc_objects");
        let count_idx = push_string("count");
        let alloc_space_idx = push_string("alloc_space");
        let bytes_idx = push_string("bytes");
        #[cfg(feature = "measure_free")]
        let free_objects_idx = push_string("free_objects");
        #[cfg(feature = "measure_free")]
        let free_space_idx = push_string("free_space");
        #[cfg(feature = "measure_free")]
        let inuse_objects_idx = push_string("inuse_objects");
        #[cfg(feature = "measure_free")]
        let inuse_space_idx = push_string("inuse_space");
        let space_idx = push_string("space");

        let sample_type = vec![
            protos::ValueType {
                ty: alloc_objects_idx,
                unit: count_idx,
            },
            protos::ValueType {
                ty: alloc_space_idx,
                unit: bytes_idx,
            },
            #[cfg(feature = "measure_free")]
            protos::ValueType {
                ty: free_objects_idx,
                unit: count_idx,
            },
            #[cfg(feature = "measure_free")]
            protos::ValueType {
                ty: free_space_idx,
                unit: count_idx,
            },
            #[cfg(feature = "measure_free")]
            protos::ValueType {
                ty: inuse_objects_idx,
                unit: count_idx,
            },
            #[cfg(feature = "measure_free")]
            protos::ValueType {
                ty: inuse_space_idx,
                unit: bytes_idx,
            },
        ];

        let period_type = Some(pprof::protos::ValueType {
            ty: space_idx,
            unit: bytes_idx,
        });

        protos::Profile {
            sample_type,
            default_sample_type: alloc_space_idx,
            sample: samples,
            string_table,
            period_type,
            period: self.period as i64,
            function: fn_tbl,
            location: loc_tbl,
            ..protos::Profile::default()
        }
    }

    /// produce a pprof proto (for use with go tool pprof and compatible visualizers)
    pub fn pprof(&self) -> pprof::protos::Profile {
        let mut proto = self.inner_pprof();

        let drop_frames_idx = proto.string_table.len();
        proto
            .string_table
            .push(".*::Profiler::track_allocated".to_string());
        proto.drop_frames = drop_frames_idx as i64;

        proto
    }

    pub fn write_pprof<W: Write>(&self, writer: &mut W) -> std::io::Result<()> {
        let mut buf = vec![];
        self.pprof().encode(&mut buf)?;
        writer.write_all(&buf)
    }
}

// Current profiler state, collection of sampled frames.
struct ProfilerState<const N: usize> {
    collector: collector::Collector<Frames<N>>,
    allocated_objects: isize,
    allocated_bytes: isize,
    #[cfg(feature = "measure_free")]
    freed_objects: isize,
    #[cfg(feature = "measure_free")]
    freed_bytes: isize,
    // take a sample when allocated crosses this threshold
    next_sample: isize,
    // take a sample when free crosses this threshold
    #[cfg(feature = "measure_free")]
    next_free_sample: isize,
    // take a sample every period bytes.
    period: usize,
}

impl<const N: usize> ProfilerState<N> {
    fn new(period: usize) -> Self {
        Self {
            collector: collector::Collector::new(),
            period,
            allocated_objects: 0,
            allocated_bytes: 0,
            #[cfg(feature = "measure_free")]
            freed_objects: 0,
            #[cfg(feature = "measure_free")]
            freed_bytes: 0,
            next_sample: period as isize,
            #[cfg(feature = "measure_free")]
            next_free_sample: period as isize,
        }
    }
}

impl<const N: usize> Default for ProfilerState<N> {
    fn default() -> Self {
        Self::new(1)
    }
}

struct Frames<const N: usize> {
    frames: [MaybeUninit<Frame>; N],
    size: usize,
    ts: SystemTime,
}

impl<const N: usize> Clone for Frames<N> {
    fn clone(&self) -> Self {
        let mut n = Self::new();
        for i in 0..self.size {
            n.frames[i].write(unsafe { self.frames[i].assume_init_ref().clone() });
        }
        n.size = self.size;
        n.ts = self.ts;
        n
    }
}

impl<const N: usize> Frames<N> {
    fn new() -> Self {
        Self {
            frames: std::array::from_fn(|_| MaybeUninit::uninit()),
            size: 0,
            ts: SystemTime::now(),
        }
    }

    /// Push will push up to N frames in the frames array.
    fn push(&mut self, frame: &Frame) -> bool {
        assert!(self.size < N);
        self.frames[self.size].write(frame.clone());
        self.size += 1;
        self.size < N
    }

    fn iter(&self) -> FramesIterator<N> {
        FramesIterator(self, 0)
    }
}

impl<const N: usize> Hash for Frames<N> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.iter()
            .for_each(|frame| frame.symbol_address().hash(state));
    }
}

impl<const N: usize> PartialEq for Frames<N> {
    fn eq(&self, other: &Self) -> bool {
        Iterator::zip(self.iter(), other.iter())
            .map(|(s1, s2)| s1.symbol_address() == s2.symbol_address())
            .all(|equal| equal)
    }
}

impl<const N: usize> Eq for Frames<N> {}

struct FramesIterator<'a, const N: usize>(&'a Frames<N>, usize);

impl<'a, const N: usize> Iterator for FramesIterator<'a, N> {
    type Item = &'a Frame;

    fn next(&mut self) -> Option<Self::Item> {
        if self.1 < self.0.size {
            let res = Some(unsafe { self.0.frames[self.1].assume_init_ref() });
            self.1 += 1;
            res
        } else {
            None
        }
    }
}

impl<const N: usize> From<Frames<N>> for pprof::Frames {
    fn from(bt: Frames<N>) -> Self {
        let frames = bt
            .iter()
            .map(|frame| {
                let mut symbols = Vec::new();
                backtrace::resolve_frame(frame, |symbol| {
                    if let Some(name) = symbol.name() {
                        let name = format!("{:#}", name);
                        if !name.starts_with("alloc::alloc::")
                            && name != "<alloc::alloc::Global as core::alloc::Allocator>::allocate"
                        {
                            symbols.push(symbol.into());
                        }
                    }
                });
                symbols
            })
            .collect();
        Self {
            frames,
            thread_name: "".to_string(),
            thread_id: 0,
            sample_timestamp: bt.ts,
        }
    }
}

// #[cfg(test)]
// mod test {
//     use super::*;

//     #[test]
//     fn test_reentrant() {
//         let _guard = HeapProfilerGuard::new(1).unwrap();

//         assert!(matches!(
//             HeapProfilerGuard::new(1),
//             Err(Error::ConcurrentHeapProfiler)
//         ));
//     }
// }
