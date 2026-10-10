//! External merge sort for fixed-size records.
//!
//! Records are collected in a RAM buffer; when it is full it is sorted (on
//! all cores) and written to disk as a sorted *run*. Reading the result
//! merges the runs. All disk access is sequential, so sorting data far
//! larger than RAM stays fast, unlike sorting one huge memory-mapped file
//! in place, which turns into random page faults once it exceeds RAM.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use bytemuck::Pod;
use rayon::slice::ParallelSliceMut;

fn sort_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(rayon::current_num_threads())
            .thread_name(|i| format!("geors-sort-{i}"))
            .build()
            .expect("sort thread pool")
    })
}

/// Upper bound for the RAM buffer of one sorter.
pub const DEFAULT_BUFFER_BYTES: usize = 256 << 20;

/// RAM buffer for one sorter: an eighth of the memory available to the
/// process (container limit or RAM), between 16 and 256 MB. A fixed 256 MB
/// buffer alone would exhaust a small container.
pub fn buffer_bytes() -> usize {
    static BYTES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *BYTES.get_or_init(|| {
        crate::memory_limit()
            .map_or(DEFAULT_BUFFER_BYTES, |m| (m / 8) as usize)
            .clamp(16 << 20, DEFAULT_BUFFER_BYTES)
    })
}

pub struct ExternalSorter<T: Pod + Ord + Send> {
    dir: PathBuf,
    prefix: String,
    buffer: Vec<T>,
    capacity: usize,
    runs: Vec<PathBuf>,
    dedup: bool,
    count: u64,
}

impl<T: Pod + Ord + Send> ExternalSorter<T> {
    /// `dedup`: drop equal records (within and across runs).
    pub fn new(dir: &Path, prefix: &str, buffer_bytes: usize, dedup: bool) -> Self {
        let capacity = (buffer_bytes / size_of::<T>()).max(1024);
        Self {
            dir: dir.to_path_buf(),
            prefix: prefix.to_string(),
            // Grows as needed: an empty or small sorter costs nothing.
            buffer: Vec::new(),
            capacity,
            runs: Vec::new(),
            dedup,
            count: 0,
        }
    }

    pub fn push(&mut self, record: T) -> io::Result<()> {
        self.buffer.push(record);
        self.count += 1;
        if self.buffer.len() >= self.capacity {
            self.spill()?;
        }
        Ok(())
    }

    pub fn extend(&mut self, records: impl IntoIterator<Item = T>) -> io::Result<()> {
        for r in records {
            self.push(r)?;
        }
        Ok(())
    }

    /// Records pushed so far (before deduplication).
    pub fn pushed(&self) -> u64 {
        self.count
    }

    fn sort_buffer(&mut self) {
        // Own pool: the sorter is fed from the consumer of a rayon pipeline
        // whose workers may all be blocked handing it data; sorting on the
        // global pool would then deadlock.
        let buffer = &mut self.buffer;
        sort_pool().install(|| buffer.par_sort_unstable());
        if self.dedup {
            self.buffer.dedup();
        }
    }

    fn spill(&mut self) -> io::Result<()> {
        self.sort_buffer();
        let path = self
            .dir
            .join(format!("{}.run{}", self.prefix, self.runs.len()));
        let mut out = BufWriter::with_capacity(1 << 20, File::create(&path)?);
        out.write_all(bytemuck::cast_slice(&self.buffer))?;
        out.flush()?;
        self.buffer.clear();
        self.runs.push(path);
        Ok(())
    }

    /// The records in sorted order. Data that fit in the buffer is sorted
    /// in RAM without touching the disk.
    pub fn finish(mut self) -> io::Result<Sorted<T>> {
        if self.runs.is_empty() {
            self.sort_buffer();
            return Ok(Sorted {
                source: Source::Memory(std::mem::take(&mut self.buffer).into_iter()),
                last: None,
                dedup: self.dedup,
                files: Vec::new(),
            });
        }
        if !self.buffer.is_empty() {
            self.spill()?;
        }
        self.buffer = Vec::new();
        let per_run = (buffer_bytes() / self.runs.len()).clamp(64 << 10, 4 << 20);
        let mut readers = Vec::with_capacity(self.runs.len());
        let mut heap = BinaryHeap::new();
        for (i, path) in self.runs.iter().enumerate() {
            let mut r = BufReader::with_capacity(per_run, File::open(path)?);
            if let Some(v) = read_one::<T>(&mut r)? {
                heap.push(Reverse((v, i)));
            }
            readers.push(r);
        }
        Ok(Sorted {
            source: Source::Merge { readers, heap },
            last: None,
            dedup: self.dedup,
            files: std::mem::take(&mut self.runs),
        })
    }
}

fn read_one<T: Pod>(r: &mut impl Read) -> io::Result<Option<T>> {
    let mut v = T::zeroed();
    match r.read_exact(bytemuck::bytes_of_mut(&mut v)) {
        Ok(()) => Ok(Some(v)),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

enum Source<T> {
    Memory(std::vec::IntoIter<T>),
    Merge {
        readers: Vec<BufReader<File>>,
        heap: BinaryHeap<Reverse<(T, usize)>>,
    },
}

/// Sorted output of an [`ExternalSorter`]. Run files are removed on drop.
pub struct Sorted<T: Pod + Ord> {
    source: Source<T>,
    last: Option<T>,
    dedup: bool,
    files: Vec<PathBuf>,
}

impl<T: Pod + Ord> Sorted<T> {
    fn next_raw(&mut self) -> io::Result<Option<T>> {
        match &mut self.source {
            Source::Memory(it) => Ok(it.next()),
            Source::Merge { readers, heap } => {
                let Some(Reverse((v, i))) = heap.pop() else {
                    return Ok(None);
                };
                if let Some(next) = read_one::<T>(&mut readers[i])? {
                    heap.push(Reverse((next, i)));
                }
                Ok(Some(v))
            }
        }
    }

    /// Next record in order (`Ok(None)` at the end).
    pub fn next_record(&mut self) -> io::Result<Option<T>> {
        loop {
            let Some(v) = self.next_raw()? else {
                return Ok(None);
            };
            if self.dedup && self.last == Some(v) {
                continue;
            }
            self.last = Some(v);
            return Ok(Some(v));
        }
    }
}

impl<T: Pod + Ord> Drop for Sorted<T> {
    fn drop(&mut self) {
        for f in &self.files {
            let _ = std::fs::remove_file(f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain<T: Pod + Ord>(mut s: Sorted<T>) -> Vec<T> {
        let mut out = Vec::new();
        while let Some(v) = s.next_record().unwrap() {
            out.push(v);
        }
        out
    }

    #[test]
    fn sorts_across_runs_with_dedup() {
        let dir = tempfile::tempdir().unwrap();
        // Tiny buffer (minimum 1024 records) to force many runs.
        let mut s = ExternalSorter::<i64>::new(dir.path(), "t", 8, true);
        let mut seed = 9u64;
        let mut want = Vec::new();
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let v = (seed % 5_000) as i64 - 2_500;
            s.push(v).unwrap();
            want.push(v);
        }
        want.sort();
        want.dedup();
        assert!(s.runs.len() >= 10);
        let sorted = s.finish().unwrap();
        let files = sorted.files.clone();
        assert_eq!(drain(sorted), want);
        assert!(files.iter().all(|f| !f.exists()), "run files removed");
    }

    #[test]
    fn in_memory_and_tuples() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = ExternalSorter::<[i64; 2]>::new(dir.path(), "t", 1 << 20, false);
        s.extend([[3, 1], [1, 9], [3, 0], [1, 9]]).unwrap();
        assert_eq!(
            drain(s.finish().unwrap()),
            vec![[1, 9], [1, 9], [3, 0], [3, 1]]
        );
    }
}

#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use rayon::prelude::*;

    /// Feeding a sorter from the consumer of a saturated rayon pipeline
    /// must not deadlock when the sorter spills (regression test).
    #[test]
    fn spills_while_rayon_workers_are_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = ExternalSorter::<i64>::new(dir.path(), "p", 8, false);
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<i64>>(1);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                (0..64i64).into_par_iter().for_each_with(tx, |tx, i| {
                    tx.send((0..1000).map(|j| i * 1000 + j).collect()).unwrap();
                });
            });
            for batch in rx {
                s.extend(batch).unwrap();
            }
        });
        let mut sorted = s.finish().unwrap();
        let mut n = 0;
        while sorted.next_record().unwrap().is_some() {
            n += 1;
        }
        assert_eq!(n, 64_000);
    }
}
