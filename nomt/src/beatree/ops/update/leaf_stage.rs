use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

use dashmap::DashMap;
use threadpool::ThreadPool;

use crate::beatree::{
    allocator::{PageNumber, StoreReader, SyncAllocator},
    index::Index,
    leaf::{
        node::{LeafNode, MAX_LEAF_VALUE_SIZE},
        overflow,
    },
    ops::{
        search_branch,
        update::{
            extend_range_protocol::{
                request_range_extension, try_answer_left_neighbor, LeftNeighbor, RightNeighbor,
                SeparatorRange, WorkerParams,
            },
            get_key,
            leaf_updater::{BaseLeaf, DigestResult as LeafDigestResult, LeafUpdater},
        },
    },
    Key,
};
use crate::io::{IoCommand, IoHandle, IoKind, PAGE_SIZE};

/// Tracker of all changes that happen to leaves during an update
pub type LeavesTracker = super::NodesTracker<LeafNode>;

fn indexed_leaf(bbn_index: &Index, key: Key) -> Option<(Key, Option<Key>, PageNumber)> {
    let Some((_, branch)) = bbn_index.lookup(key) else {
        return None;
    };

    let Some((i, leaf_pn)) = search_branch(&branch, key) else {
        return None;
    };
    let separator = get_key(&branch, i);

    let cutoff = if i + 1 < branch.n() as usize {
        Some(get_key(&branch, i + 1))
    } else {
        bbn_index.next_key(key)
    };

    Some((separator, cutoff, leaf_pn))
}

/// Data, including buffers, which may be dropped only once I/O has certainly concluded.
#[derive(Default)]
pub struct PostIoDrop {
    worker_changes: Vec<LeafWorkerOutput>,
}

/// Outputs of the leaf stage.
#[derive(Default)]
pub struct LeafStageOutput {
    /// The changed leaves.
    pub leaf_changeset: Vec<(Key, Option<PageNumber>)>,
    /// The page numbers of all freed pages.
    pub freed_pages: Vec<PageNumber>,
    /// The number of submitted I/Os.
    pub submitted_io: usize,
    /// Data which should be dropped after all submitted I/Os have concluded.
    pub post_io_drop: PostIoDrop,
}

/// Change the btree's leaves in the specified way
pub fn run(
    bbn_index: &Index,
    leaf_cache: DashMap<PageNumber, Arc<LeafNode>>,
    leaf_reader: StoreReader,
    leaf_writer: SyncAllocator,
    io_handle: IoHandle,
    changeset: Arc<BTreeMap<Key, Option<Vec<u8>>>>,
    thread_pool: ThreadPool,
    num_workers: usize,
) -> anyhow::Result<LeafStageOutput> {
    if changeset.is_empty() {
        return Ok(LeafStageOutput::default());
    }

    let mut overflow_io = 0;
    let page_pool = leaf_reader.page_pool().clone();

    let changeset = changeset
        .iter()
        .map(|(k, v)| match v {
            Some(v) if v.len() <= MAX_LEAF_VALUE_SIZE => Ok((*k, Some((v.clone(), false)))),
            Some(large_value) => {
                let (pages, num_writes) =
                    overflow::chunk(&large_value, &leaf_writer, &page_pool, &io_handle)?;
                overflow_io += num_writes;

                let cell = overflow::encode_cell(large_value.len(), &pages);
                Ok((*k, Some((cell, true))))
            }
            None => Ok((*k, None)),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    assert!(num_workers >= 1);
    let workers = prepare_workers(bbn_index, &changeset, num_workers);
    assert!(!workers.is_empty());

    let leaf_cache = Arc::new(leaf_cache);
    let changeset = Arc::new(changeset);

    let num_workers = workers.len();
    let (worker_result_tx, worker_result_rx) = crossbeam_channel::bounded(num_workers);

    for worker_params in workers {
        let bbn_index = bbn_index.clone();
        let leaf_cache = leaf_cache.clone();
        let leaf_reader = leaf_reader.clone();
        let leaf_writer = leaf_writer.clone();
        let io_handle = io_handle.clone();
        let changeset = changeset.clone();

        let worker_result_tx = worker_result_tx.clone();
        thread_pool.execute(move || {
            // passing the large `Arc` values by reference ensures that they are dropped at the
            // end of this scope, not the end of `run_worker`.
            let res = run_worker(
                bbn_index,
                &*leaf_cache,
                leaf_reader,
                leaf_writer,
                io_handle,
                &*changeset,
                worker_params,
            );

            let _ = worker_result_tx.send(res);
        });
    }

    // we don't want to block other sync steps on deallocating these memory regions.
    drop(changeset);
    drop(leaf_cache);

    let mut output = LeafStageOutput::default();
    output.submitted_io += overflow_io;

    for _ in 0..num_workers {
        // UNWRAP: results are always sent unless worker panics.
        let worker_output = worker_result_rx.recv().unwrap();
        apply_worker_changes(&leaf_reader, &mut output, worker_output);
    }

    output.leaf_changeset.sort_by_key(|(k, _)| *k);
    Ok(output)
}

fn apply_worker_changes(
    leaf_reader: &StoreReader,
    output: &mut LeafStageOutput,
    mut worker_output: LeafWorkerOutput,
) {
    for deleted_overflow_cell in worker_output.overflow_deleted.drain(..) {
        overflow::delete(&deleted_overflow_cell, leaf_reader, &mut output.freed_pages);
    }

    for (key, leaf_entry) in &worker_output.leaves_tracker.inner {
        let leaf_pn = leaf_entry.inserted.as_ref().map(|(_leaf, pn)| pn.clone());
        if leaf_pn.is_some() {
            output.submitted_io += 1;
        }

        if let Some(prev_pn) = leaf_entry.deleted {
            output.freed_pages.push(prev_pn);
        }

        output.leaf_changeset.push((*key, leaf_pn));
    }

    output.submitted_io += worker_output.leaves_tracker.extra_freed.len();
    output
        .freed_pages
        .extend(worker_output.leaves_tracker.extra_freed.drain(..));

    output.post_io_drop.worker_changes.push(worker_output);
}

fn prepare_workers(
    bbn_index: &Index,
    changeset: &[(Key, Option<(Vec<u8>, bool)>)],
    worker_count: usize,
) -> Vec<WorkerParams<LeafNode>> {
    let mut remaining_workers = worker_count;
    let mut changeset_remaining = changeset;

    let mut workers = Vec::with_capacity(worker_count);

    // first worker covers everything. it'll be adjusted in the next iteration of the loop,
    // if possible.
    workers.push(WorkerParams {
        left_neighbor: None,
        right_neighbor: None,
        range: SeparatorRange {
            low: None,
            high: None,
        },
        op_range: Range {
            start: 0,
            end: changeset.len(),
        },
    });
    remaining_workers -= 1;

    // iter: find endpoint for previous worker and push next worker.
    // look up the leaf for the (ops/N)th operation, find out how many ops it covers,
    // and set the end range accordingly.
    while remaining_workers > 0 && changeset_remaining.len() > 0 {
        let pivot_idx = changeset_remaining.len() / (remaining_workers + 1);

        // If pivot_idx == 0 the number of changeset_remaining
        // is less than the number of remaining workers.
        // Everything remaining will be covered by the previous worker.
        if pivot_idx == 0 {
            break;
        }

        // UNWRAP: first worker is pushed at the beginning of the range.
        let prev_worker = workers.last_mut().unwrap();

        match indexed_leaf(bbn_index, changeset_remaining[pivot_idx].0) {
            None => break,
            Some((_, None, _)) => break,
            Some((separator, Some(_), _)) => {
                // link this worker with the previous one.
                let (tx, rx) = crossbeam_channel::unbounded();

                // have previous worker cover everything, only up to the separator of this node.
                let prev_worker_ops = pivot_idx
                    - changeset_remaining[..pivot_idx]
                        .iter()
                        .rev()
                        .take_while(|(k, _)| k >= &separator)
                        .count();

                if prev_worker_ops == 0 {
                    changeset_remaining = &changeset_remaining[pivot_idx..];
                    continue;
                }

                let op_partition_index =
                    (changeset.len() - changeset_remaining.len()) + prev_worker_ops;

                // previous worker now owns all nodes up to this one.
                prev_worker.range.high = Some(separator);
                prev_worker.right_neighbor = Some(RightNeighbor { tx });
                prev_worker.op_range.end = op_partition_index;

                workers.push(WorkerParams {
                    left_neighbor: Some(LeftNeighbor { rx }),
                    right_neighbor: None,
                    range: SeparatorRange {
                        low: Some(separator),
                        high: None,
                    },
                    op_range: Range {
                        start: op_partition_index,
                        end: changeset.len(),
                    },
                });
                remaining_workers -= 1;
                changeset_remaining = &changeset_remaining[prev_worker_ops..];
            }
        }
    }

    workers
}

fn reset_leaf_base(
    bbn_index: &Index,
    leaf_cache: &DashMap<PageNumber, Arc<LeafNode>>,
    leaf_reader: &StoreReader,
    leaves_tracker: &mut LeavesTracker,
    leaf_updater: &mut LeafUpdater,
    has_extended_range: bool,
    mut key: Key,
) {
    if !has_extended_range {
        reset_leaf_base_fresh(
            bbn_index,
            leaf_cache,
            leaf_reader,
            leaves_tracker,
            leaf_updater,
            key,
        );
        return;
    }

    if let Some((separator, node, next_separator)) = leaves_tracker.pending_base.take() {
        let base = BaseLeaf::new(node, separator);
        leaf_updater.reset_base(Some(base), next_separator);
    } else {
        if let Some(separator) = leaves_tracker
            .inner
            .last_key_value()
            .and_then(|(_, entry)| entry.next_separator)
        {
            if separator > key {
                key = separator;
            }
            reset_leaf_base_fresh(
                bbn_index,
                leaf_cache,
                leaf_reader,
                leaves_tracker,
                leaf_updater,
                key,
            )
        } else {
            // special case: all rightward workers deleted every last one of their nodes after the last one
            // we received from a range extension. We are now writing the new rightmost node, which
            // is permitted to be underfull.
            leaf_updater.remove_cutoff();
        }
    }
}

fn reset_leaf_base_fresh(
    bbn_index: &Index,
    leaf_cache: &DashMap<PageNumber, Arc<LeafNode>>,
    leaf_reader: &StoreReader,
    leaves_tracker: &mut LeavesTracker,
    leaf_updater: &mut LeafUpdater,
    key: Key,
) {
    let Some((separator, cutoff, leaf_pn)) = indexed_leaf(bbn_index, key) else {
        return;
    };

    // we intend to work on this leaf, therefore, we delete it.
    // any new leaves produced by the updater will replace it.
    leaves_tracker.delete(separator, leaf_pn, cutoff);

    let base = BaseLeaf::new(
        leaf_cache
            .remove(&leaf_pn)
            .map(|(_, l)| l)
            .unwrap_or_else(|| {
                Arc::new(LeafNode {
                    inner: leaf_reader.query(leaf_pn),
                })
            }),
        separator,
    );

    leaf_updater.reset_base(Some(base), cutoff);
}

struct LeafWorkerOutput {
    leaves_tracker: LeavesTracker,
    overflow_deleted: Vec<Vec<u8>>,
}

fn run_worker(
    bbn_index: Index,
    leaf_cache: &DashMap<PageNumber, Arc<LeafNode>>,
    leaf_reader: StoreReader,
    leaf_writer: SyncAllocator,
    io_handle: IoHandle,
    changeset: &[(Key, Option<(Vec<u8>, bool)>)],
    mut worker_params: WorkerParams<LeafNode>,
) -> LeafWorkerOutput {
    let mut leaf_updater = LeafUpdater::new(leaf_reader.page_pool().clone(), None, None);
    let mut overflow_deleted = Vec::new();
    let mut pending_left_request = None;
    let mut has_extended_range = false;
    let mut has_finished_workload = false;

    let mut new_leaf_state = NewLeafHandler {
        leaf_writer,
        leaves_tracker: LeavesTracker::new(),
        io_handle,
    };

    // point leaf updater at first leaf.
    reset_leaf_base(
        &bbn_index,
        &leaf_cache,
        &leaf_reader,
        &mut new_leaf_state.leaves_tracker,
        &mut leaf_updater,
        has_extended_range,
        changeset[worker_params.op_range.start].0,
    );

    for (key, op) in &changeset[worker_params.op_range.clone()] {
        // ensure key is in scope for leaf updater. if not, digest it. merge rightwards until
        //    done _or_ key is in scope.
        while !leaf_updater.is_in_scope(&key) {
            let k = if let LeafDigestResult::NeedsMerge(cutoff) =
                leaf_updater.digest(&mut new_leaf_state)
            {
                cutoff
            } else {
                *key
            };

            try_answer_left_neighbor(
                &mut pending_left_request,
                &mut worker_params,
                &mut new_leaf_state.leaves_tracker,
                has_finished_workload,
            );

            has_extended_range = false;
            if worker_params.range.high.map_or(false, |high| k >= high) {
                has_extended_range = true;
                super::extend_range_protocol::request_range_extension(
                    &mut worker_params,
                    &mut new_leaf_state.leaves_tracker,
                );
            }

            reset_leaf_base(
                &bbn_index,
                leaf_cache,
                &leaf_reader,
                &mut new_leaf_state.leaves_tracker,
                &mut leaf_updater,
                has_extended_range,
                k,
            );
        }

        let (value_change, overflow) = match op {
            None => (None, false),
            Some((v, overflow)) => (Some(v.clone()), *overflow),
        };

        let delete_overflow = |overflow_cell: &[u8]| overflow_deleted.push(overflow_cell.to_vec());
        leaf_updater.ingest(*key, value_change, overflow, delete_overflow);
    }

    while let LeafDigestResult::NeedsMerge(cutoff) = leaf_updater.digest(&mut new_leaf_state) {
        try_answer_left_neighbor(
            &mut pending_left_request,
            &mut worker_params,
            &mut new_leaf_state.leaves_tracker,
            has_finished_workload,
        );

        has_extended_range = false;
        if worker_params
            .range
            .high
            .map_or(false, |high| cutoff >= high)
        {
            has_extended_range = true;
            request_range_extension(&mut worker_params, &mut new_leaf_state.leaves_tracker);
        }

        reset_leaf_base(
            &bbn_index,
            leaf_cache,
            &leaf_reader,
            &mut new_leaf_state.leaves_tracker,
            &mut leaf_updater,
            has_extended_range,
            cutoff,
        );
    }

    // Now we are safe to send over our right neighbor to the left one
    // because we're sure to have finished interacting with it
    has_finished_workload = true;

    // wait until left worker concludes or exhausts our range.
    while worker_params.left_neighbor.is_some() {
        // answer any outstanding pending request
        try_answer_left_neighbor(
            &mut pending_left_request,
            &mut worker_params,
            &mut new_leaf_state.leaves_tracker,
            has_finished_workload,
        );
        // we're done with the right worker so we're sure
        // we are able to respond to each request
        assert!(pending_left_request.is_none());

        match worker_params.left_neighbor.as_ref().map(|l| l.rx.recv()) {
            None => continue,
            Some(Ok(item)) => {
                pending_left_request = Some(item);
            }
            Some(Err(_)) => {
                worker_params.left_neighbor = None;
            }
        }
    }

    LeafWorkerOutput {
        leaves_tracker: new_leaf_state.leaves_tracker,
        overflow_deleted,
    }
}

struct NewLeafHandler {
    leaf_writer: SyncAllocator,
    leaves_tracker: LeavesTracker,
    io_handle: IoHandle,
}

impl super::leaf_updater::HandleNewLeaf for NewLeafHandler {
    fn handle_new_leaf(&mut self, key: Key, leaf: LeafNode, cutoff: Option<Key>) {
        let leaf = Arc::new(leaf);
        let fd = self.leaf_writer.store_fd();

        // TODO: handle error
        let page_number = self.leaf_writer.allocate().unwrap();

        // TODO: handle error
        let ptr = leaf.inner.as_ptr();
        self.io_handle
            .send(IoCommand {
                kind: IoKind::WriteRaw(fd, page_number.0 as u64, ptr, PAGE_SIZE),
                user_data: 0,
            })
            .expect("I/O Pool Down");

        self.leaves_tracker.insert(key, leaf, cutoff, page_number);
    }
}
