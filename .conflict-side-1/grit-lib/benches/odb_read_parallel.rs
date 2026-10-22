//! Parallel packed-object read throughput (Criterion).

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod fixture;

use std::hint::black_box;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::sync::OnceLock;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use fixture::build_parallel_packed_blob_fixture;
use grit_lib::objects::odb::Odb;
use grit_lib::objects::pack::clear_pack_cache;
use grit_lib::objects::ObjectId;
use tempfile::TempDir;

struct ParallelReadFixture {
    _root: TempDir,
    odb: Odb,
    oids: Vec<ObjectId>,
}

static FIXTURE: OnceLock<ParallelReadFixture> = OnceLock::new();

fn fixture() -> &'static ParallelReadFixture {
    FIXTURE.get_or_init(|| {
        clear_pack_cache();
        let (root, odb, oids) = build_parallel_packed_blob_fixture(50_000);
        ParallelReadFixture {
            _root: root,
            odb,
            oids,
        }
    })
}

enum PoolMsg {
    Run { ack: mpsc::Sender<()> },
    Shutdown,
}

struct ReadWorkerPool {
    workers: Vec<JoinHandle<()>>,
    run_txs: Vec<mpsc::Sender<PoolMsg>>,
}

impl ReadWorkerPool {
    fn new(thread_count: usize, odb: Odb, partitions: Arc<[Arc<[ObjectId]>]>) -> Self {
        let mut workers = Vec::with_capacity(thread_count);
        let mut run_txs = Vec::with_capacity(thread_count);
        for t in 0..thread_count {
            let (tx, rx) = mpsc::channel();
            run_txs.push(tx);
            let slice = Arc::clone(&partitions[t]);
            let odb_t = odb.clone();
            workers.push(thread::spawn(move || {
                odb_t.with_pack_read_context(|| loop {
                    match rx.recv() {
                        Ok(PoolMsg::Run { ack }) => {
                            for oid in slice.iter() {
                                black_box(odb_t.read(oid).expect("read").data);
                            }
                            let _ = ack.send(());
                        }
                        Ok(PoolMsg::Shutdown) | Err(_) => break,
                    }
                })
            }));
        }
        Self { workers, run_txs }
    }

    fn run_iteration(&self, thread_count: usize) -> Result<(), RecvTimeoutError> {
        let (ack_tx, ack_rx) = mpsc::channel();
        for tx in &self.run_txs {
            tx.send(PoolMsg::Run {
                ack: ack_tx.clone(),
            })
            .expect("worker alive");
        }
        drop(ack_tx);
        for _ in 0..thread_count {
            ack_rx.recv_timeout(Duration::from_secs(120))?;
        }
        Ok(())
    }
}

impl Drop for ReadWorkerPool {
    fn drop(&mut self) {
        for tx in &self.run_txs {
            let _ = tx.send(PoolMsg::Shutdown);
        }
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

fn bench_parallel_reads(c: &mut Criterion) {
    let fx = fixture();
    let odb = fx.odb.clone();
    let mut group = c.benchmark_group("odb_read_parallel/packed_blobs");
    group.throughput(Throughput::Elements(fx.oids.len() as u64));
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(5));
    group.measurement_time(Duration::from_secs(10));
    for threads in [1_usize, 2, 4, 8] {
        let thread_count = threads;
        let chunk = fx.oids.len().div_ceil(thread_count);
        let partitions: Arc<[Arc<[ObjectId]>]> = (0..thread_count)
            .map(|t| {
                let start = t * chunk;
                if start >= fx.oids.len() {
                    return Arc::<[ObjectId]>::from([]);
                }
                let end = (start + chunk).min(fx.oids.len());
                Arc::from(fx.oids[start..end].to_vec())
            })
            .collect::<Vec<_>>()
            .into();

        let pool = ReadWorkerPool::new(thread_count, odb.clone(), partitions);

        group.bench_with_input(
            BenchmarkId::from_parameter(threads),
            &thread_count,
            |b, &thread_count| {
                b.iter_custom(|iters| {
                    let start = Instant::now();
                    for _ in 0..iters {
                        pool.run_iteration(thread_count).expect("worker iteration");
                    }
                    start.elapsed()
                });
            },
        );
    }
    group.finish();
}

criterion_group!(odb_read_parallel, bench_parallel_reads);
criterion_main!(odb_read_parallel);
