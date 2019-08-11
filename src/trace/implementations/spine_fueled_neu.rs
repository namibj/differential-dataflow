//! An append-only collection of update batches.
//!
//! The `Spine` is a general-purpose trace implementation based on collection and merging
//! immutable batches of updates. It is generic with respect to the batch type, and can be
//! instantiated for any implementor of `trace::Batch`.

use std::fmt::Debug;

use ::difference::Semigroup;
use lattice::Lattice;
use trace::{Batch, BatchReader, Trace, TraceReader};
// use trace::cursor::cursor_list::CursorList;
use trace::cursor::{Cursor, CursorList};
use trace::Merger;

use ::timely::dataflow::operators::generic::OperatorInfo;

/// An append-only collection of update tuples.
///
/// A spine maintains a small number of immutable collections of update tuples, merging the collections when
/// two have similar sizes. In this way, it allows the addition of more tuples, which may then be merged with
/// other immutable collections.
pub struct Spine<K, V, T: Lattice+Ord, R: Semigroup, B: Batch<K, V, T, R>> {
    operator: OperatorInfo,
    logger: Option<::logging::Logger>,
    phantom: ::std::marker::PhantomData<(K, V, R)>,
    advance_frontier: Vec<T>,                   // Times after which the trace must accumulate correctly.
    through_frontier: Vec<T>,                   // Times after which the trace must be able to subset its inputs.
    merging: Vec<MergeState<K,V,T,R,B>>,// Several possibly shared collections of updates.
    pending: Vec<B>,                       // Batches at times in advance of `frontier`.
    upper: Vec<T>,
    effort: usize,
    activator: Option<timely::scheduling::activate::Activator>,
}

impl<K, V, T, R, B> TraceReader for Spine<K, V, T, R, B>
where
    K: Ord+Clone,           // Clone is required by `batch::advance_*` (in-place could remove).
    V: Ord+Clone,           // Clone is required by `batch::advance_*` (in-place could remove).
    T: Lattice+Ord+Clone+Debug+Default,
    R: Semigroup,
    B: Batch<K, V, T, R>+Clone+'static,
{
    type Key = K;
    type Val = V;
    type Time = T;
    type R = R;

    type Batch = B;
    type Cursor = CursorList<K, V, T, R, <B as BatchReader<K, V, T, R>>::Cursor>;

    fn cursor_through(&mut self, upper: &[T]) -> Option<(Self::Cursor, <Self::Cursor as Cursor<K, V, T, R>>::Storage)> {

        // The supplied `upper` should have the property that for each of our
        // batch `lower` and `upper` frontiers, the supplied upper is comparable
        // to the frontier; it should not be incomparable, because the frontiers
        // that we created form a total order. If it is, there is a bug.
        //
        // We should acquire a cursor including all batches whose upper is less
        // or equal to the supplied upper, excluding all batches whose lower is
        // greater or equal to the supplied upper, and if a batch straddles the
        // supplied upper it had better be empty.

        // We shouldn't grab a cursor into a closed trace, right?
        assert!(self.advance_frontier.len() > 0);

        // Check that `upper` is greater or equal to `self.through_frontier`.
        // Otherwise, the cut could be in `self.merging` and it is user error anyhow.
        assert!(upper.iter().all(|t1| self.through_frontier.iter().any(|t2| t2.less_equal(t1))));

        let mut cursors = Vec::new();
        let mut storage = Vec::new();

        for merge_state in self.merging.iter().rev() {
            match merge_state {
                MergeState::Double(ref batch1, ref batch2, _, _) => {
                    if !batch1.is_empty() {
                        cursors.push(batch1.cursor());
                        storage.push(batch1.clone());
                    }
                    if !batch2.is_empty() {
                        cursors.push(batch2.cursor());
                        storage.push(batch2.clone());
                    }
                },
                MergeState::Single(ref batch) => {
                    if !batch.is_empty() {
                        cursors.push(batch.cursor());
                        storage.push(batch.clone());
                    }
                },
                MergeState::Vacant => { }
            }
        }

        for batch in self.pending.iter() {

            if !batch.is_empty() {

                // For a non-empty `batch`, it is a catastrophic error if `upper`
                // requires some-but-not-all of the updates in the batch. We can
                // determine this from `upper` and the lower and upper bounds of
                // the batch itself.
                //
                // TODO: It is not clear if this is the 100% correct logic, due
                // to the possible non-total-orderedness of the frontiers.

                let include_lower = upper.iter().all(|t1| batch.lower().iter().any(|t2| t2.less_equal(t1)));
                let include_upper = upper.iter().all(|t1| batch.upper().iter().any(|t2| t2.less_equal(t1)));

                if include_lower != include_upper && upper != batch.lower() {
                    panic!("`cursor_through`: `upper` straddles batch");
                }

                // include pending batches
                if include_upper {
                    cursors.push(batch.cursor());
                    storage.push(batch.clone());
                }
            }
        }

        Some((CursorList::new(cursors, &storage), storage))
    }
    fn advance_by(&mut self, frontier: &[T]) {
        self.advance_frontier = frontier.to_vec();
        if self.advance_frontier.len() == 0 {
            self.pending.clear();
            self.merging.clear();
        }
    }
    fn advance_frontier(&mut self) -> &[T] { &self.advance_frontier[..] }
    fn distinguish_since(&mut self, frontier: &[T]) {
        self.through_frontier = frontier.to_vec();
        self.consider_merges();
    }
    fn distinguish_frontier(&mut self) -> &[T] { &self.through_frontier[..] }

    fn map_batches<F: FnMut(&Self::Batch)>(&mut self, mut f: F) {
        for batch in self.merging.iter().rev() {
            match batch {
                MergeState::Double(batch1, batch2, _, _) => { f(batch1); f(batch2); },
                MergeState::Single(batch) => { f(batch); },
                MergeState::Vacant => { },
            }
        }
        for batch in self.pending.iter() {
            f(batch);
        }
    }
}

// A trace implementation for any key type that can be borrowed from or converted into `Key`.
// TODO: Almost all this implementation seems to be generic with respect to the trace and batch types.
impl<K, V, T, R, B> Trace for Spine<K, V, T, R, B>
where
    K: Ord+Clone,
    V: Ord+Clone,
    T: Lattice+Ord+Clone+Debug+Default,
    R: Semigroup,
    B: Batch<K, V, T, R>+Clone+'static,
{
    fn new(
        info: ::timely::dataflow::operators::generic::OperatorInfo,
        logging: Option<::logging::Logger>,
        activator: Option<timely::scheduling::activate::Activator>,
    ) -> Self {
        Self::with_effort(4, info, logging, activator)
    }

    /// Apply some amount of effort to trace maintenance.
    ///
    /// The units of effort are updates, and the method should be
    /// though of as analogous to inserting as many empty updates,
    /// where the trace is permitted to perform proportionate work.
    fn exert(&mut self, mut effort: usize) {
        // We could do many things here. My sense is that we might
        // prioritize completing existing merges, and in their absence
        // start introducing empty batches to advance batch compaction.
        self.apply_fuel(&mut effort);
    }

    // Ideally, this method acts as insertion of `batch`, even if we are not yet able to begin
    // merging the batch. This means it is a good time to perform amortized work proportional
    // to the size of batch.
    fn insert(&mut self, batch: Self::Batch) {

        // self.logger.as_ref().map(|l| l.log(::logging::BatchEvent {
        //     operator: self.operator.global_id,
        //     length: batch.len()
        // }));

        assert!(batch.lower() != batch.upper());
        assert_eq!(batch.lower(), &self.upper[..]);

        self.upper = batch.upper().to_vec();

        // TODO: Consolidate or discard empty batches.
        self.pending.push(batch);
        self.consider_merges();
    }

    fn close(&mut self) {
        if !self.upper.is_empty() {
            use trace::Builder;
            let builder = B::Builder::new();
            let batch = builder.done(&self.upper[..], &[], &self.upper[..]);
            self.insert(batch);
        }
    }
}

impl<K, V, T, R, B> Spine<K, V, T, R, B>
where
    K: Ord+Clone,
    V: Ord+Clone,
    T: Lattice+Ord+Clone+Debug+Default,
    R: Semigroup,
    B: Batch<K, V, T, R>,
{
    fn describe(&self) -> Vec<usize> {
        self.merging
            .iter()
            .map(|b| match b {
                MergeState::Vacant => 0,
                MergeState::Single(_) => 1,
                MergeState::Double(_,_,_,_) => 2
            })
            .collect()
    }

    /// Allocates a fueled `Spine` with a specified effort multiplier.
    ///
    /// This trace will merge batches progressively, with each inserted batch applying a multiple
    /// of the batch's length in effort to each merge. The `effort` parameter is that multiplier.
    /// This value should be at least one for the merging to happen; a value of zero is not helpful.
    pub fn with_effort(
        mut effort: usize,
        operator: OperatorInfo,
        logger: Option<::logging::Logger>,
        activator: Option<timely::scheduling::activate::Activator>,
    ) -> Self {

        // Zero effort is .. not smart.
        if effort == 0 { effort = 1; }

        Spine {
            operator,
            logger,
            phantom: ::std::marker::PhantomData,
            advance_frontier: vec![<T as Lattice>::minimum()],
            through_frontier: vec![<T as Lattice>::minimum()],
            merging: Vec::new(),
            pending: Vec::new(),
            upper: vec![Default::default()],
            effort,
            activator,
        }
    }

    // Migrate data from `self.pending` into `self.merging`.
    #[inline(never)]
    fn consider_merges(&mut self) {

        while self.pending.len() > 0 &&
              self.through_frontier.iter().all(|t1| self.pending[0].upper().iter().any(|t2| t2.less_equal(t1)))
        {
            // this could be a VecDeque, if we ever notice this.
            let batch = self.pending.remove(0);
            let index = batch.len().next_power_of_two();
            self.introduce_batch(batch, index.trailing_zeros() as usize);

            // Having performed all of our work, if more than one batch remains reschedule ourself.
            if self.merging.len() > 2 && self.merging[..self.merging.len()-1].iter().any(|b| !b.is_vacant()) {
                if let Some(activator) = &self.activator {
                    activator.activate();
                }
            }
        }
    }

    /// Introduces a batch at an indicated level.
    ///
    /// The level indication is often related to the size of the batch, but
    /// it can also be used to artificially fuel the computation by supplying
    /// empty batches at non-trivial indices, to move merges along.
    pub fn introduce_batch(&mut self, batch: B, batch_index: usize) {

        // This spine is represented as a list of layers, where each element in the list is either
        //
        //   1. MergeState::Vacant  empty
        //   2. MergeState::Single  a single batch
        //   3. MergeState::Double  a pair of batches
        //
        // Each of the batches at layer i contains at most 2^i elements. The sequence of batches
        // should have the upper bound of one match the lower bound of the next. Batches may be
        // logically empty, with matching upper and lower bounds, as a bookkeeping mechanism.
        //
        // Each batch at layer i is treated as if it contains exactly 2^i elements, even though it
        // may actually contain fewer elements. This allows us to decouple the physical representation
        // from logical amounts of effort invested in each batch. It allows us to begin compaction and
        // to reduce the number of updates, without compromising our ability to continue to move
        // updates along the spine.
        //
        // We attempt to maintain the invariant that no two adjacent levels have pairs of batches.
        // This invariant exists to make sure we have the breathing room to initiate but not
        // immediately complete merges. As soon as a layer contains two batches we initiate a merge
        // into a batch of the next sized layer, and we start to apply fuel to this merge. When it
        // completes, we install the newly merged batch in the next layer and uninstall the two
        // batches from their layer.
        //
        // When a merge completes, it vacates an entire level. Assuming we have maintained our invariant,
        // the number of updates below that level (say "k") is at most
        //
        //         2^k-1 + 2*2^k-2 + 2^k-3 + 2*2^k-4 + ...
        //       = \---  2^k  ---/ + \--- 2^k-2 ---/ + ...
        //      <=
        //         2^k+1 - 2^k-1 - 2^k-3 - ...
        //
        // Fortunately, the now empty layer k needs 2^k+1 updates before it will initiate a new merge,
        // and the collection must accept 2^k-1 updates before such a merge could possibly be initiated.
        // This means that if each introduced merge applies a proportionate amount of fuel to the merge,
        // we should be able to complete any merge at the next level before a new one is proposed.
        //
        // This reasoning can likely be extended to justify other manipulations of the spine, in particular
        // tidying the area around the largest batch, which we likely want to draw down whenever possible.
        // If sufficient merging fuel is applied to the largest merges, we might expect them to complete
        // well before the empty space below them becomes occupied, at which point we could downgrade them
        // without risk (because there are no higher batches to be concerned about).

        // Step 0.  Determine an amount of fuel to use for the computation.
        //
        //          Fuel is used to drive maintenance of the data structure,
        //          and in particular are used to make progress through merges
        //          that are in progress. The amount of fuel to use should be
        //          proportional to the number of records introduced, so that
        //          we are guaranteed to complete all merges before they are
        //          required as arguments to merges again.
        //
        //          The fuel use policy is negotiable, in that we might aim
        //          to use relatively less when we can, so that we return
        //          control promptly, or we might account more work to larger
        //          batches. Not clear to me which are best, of if there
        //          should be a configuration knob controlling this.

        // The amount of fuel to use is proportional to 2^batch_index, scaled
        // by a factor of self.effort which determines how eager we are in
        // performing maintenance work. We need to ensure that each merge in
        // progress receives fuel for each introduced batch, and so multiply
        // by that as well.
        if batch_index > 32 { println!("Large batch index: {}", batch_index); }
        let mut fuel = 1 << batch_index;
        fuel *= self.effort;
        fuel *= self.merging.len();

        // Step 1.  Apply fuel to each in-progress merge.
        //
        //          Before we can introduce new updates, we must apply any
        //          fuel to in-progress merges, as this fuel is what ensures
        //          that the merges will be complete by the time we insert
        //          the updates.
        self.apply_fuel(&mut fuel);

        // Step 2.  Before installing the batch we must ensure the invariant
        //          that no adjacent layers contain two batches. We can make
        //          this happen by forcibly completing all merges at layers
        //          lower than `batch_index`
        //
        //          This can be interpreted as the introduction of some
        //          volume of fake updates, and we will need to fuel merges
        //          by a proportional amount to ensure that they are not
        //          surprised later on. These fake updates should have total
        //          size proportional to the batch size itself.
        self.roll_up(batch_index);

        // Step 4. This insertion should be into an empty layer. It is a
        //         logical error otherwise, as we may be violating our
        //         invariant, from which all derives.
        self.insert_at(batch, batch_index);

        // Step 3. Tidy the largest layers.
        //
        //         It is important that we not tidy only smaller layers,
        //         as their ascension is what ensures the merging and
        //         eventual compaction of the largest layers.
        self.tidy_layers();
    }

    /// Ensures that layers up through and including `index` are empty.
    ///
    /// This method is used to prepare for the insertion of a single batch
    /// at `index`, which should maintain the invariant that no adjacent
    /// layers both contain `MergeState::Double` variants.
    fn roll_up(&mut self, index: usize) {

        while self.merging.len() <= index {
            self.merging.push(MergeState::Vacant);
        }

        let merge =
        self.merging[.. index+1]
            .iter_mut()
            .fold(None, |merge, level|
                match (merge, level.complete()) {
                    (Some(batch_new), Some(batch_old)) => {
                        MergeState::begin_merge(batch_old, batch_new, None).complete()
                    },
                    (None, batch) => batch,
                    (merge, None) => merge,
                }
            );

        // We have collected all batches at levels less or equal to index, which represents
        // 2^{index+1} updates. It now belongs at level index+1, which we hope has resolved
        // any merging through the prior application of fuel.
        if let Some(batch) = merge {
            self.insert_at(batch, index + 1);
        }
    }

    /// Applies an amount of fuel to merges in progress.
    ///
    /// The intended invariants maintain that each merge in progress completes before
    /// there are enough records in lower levels to fully populate one batch at its
    /// layer. This invariant ensures that we can always apply an unbounded amount of
    /// fuel and not encounter merges in to merging layers (the "safety" does not result
    /// from insufficient fuel applied to lower levels).
    pub fn apply_fuel(&mut self, fuel: &mut usize) {
        for index in 0 .. self.merging.len() {
            if let Some(batch) = self.merging[index].work(fuel) {
                self.insert_at(batch, index+1);
            }
        }
    }

    /// Inserts a batch at a specific location.
    ///
    /// This is a non-public internal method that can panic if we try and insert into a
    /// layer which already contains two batches (and is in the process of merging).
    fn insert_at(&mut self, batch: B, index: usize) {
        while self.merging.len() <= index {
            self.merging.push(MergeState::Vacant);
        }
        let frontier = if index == self.merging.len()-1 { Some(self.advance_frontier.clone()) } else { None };
        self.merging[index].insert(batch, frontier);
    }

    /// Attempts to draw down large layers to size appropriate layers.
    fn tidy_layers(&mut self) {

        // If the largest layer is complete (not merging), we can attempt
        // to draw it down to the next layer if either that layer is empty,
        // or if it is a singleton and the layer below it is not merging.
        // We expect this should happen at various points if we have enough
        // fuel rolling around.

        let mut length = self.merging.len();
        if self.merging[length-1].is_single() {
            while (self.merging[length-1].len().next_power_of_two().trailing_zeros() as usize) < length && length > 1 && self.merging[length-2].is_vacant() {
                let batch = self.merging.pop().unwrap();
                self.merging[length-2] = batch;
                length = self.merging.len();
            }
        }
    }
}


/// Describes the state of a layer.
///
/// A layer can be empty, contain a single batch, or contain a pair of batches
/// that are in the process of merging into a batch for the next layer.
enum MergeState<K, V, T, R, B: Batch<K, V, T, R>> {
    /// An empty layer, containing no updates.
    Vacant,
    /// A layer containing a single batch.
    Single(B),
    /// A layer containing two batch, in the process of merging.
    Double(B, B, Option<Vec<T>>, <B as Batch<K,V,T,R>>::Merger),
}

impl<K, V, T: Eq, R, B: Batch<K, V, T, R>> MergeState<K, V, T, R, B> {

    /// The number of actual updates contained in the level.
    fn len(&self) -> usize {
        match self {
            MergeState::Vacant => 0,
            MergeState::Single(b) => b.len(),
            MergeState::Double(b1,b2,_,_) => b1.len() + b2.len(),
        }
    }

    /// True only for the MergeState::Vacant variant.
    fn is_vacant(&self) -> bool {
        if let MergeState::Vacant = self { true } else { false }
    }

    /// True only for the MergeState::Single variant.
    fn is_single(&self) -> bool {
        if let MergeState::Single(_) = self { true } else { false }
    }

    /// Immediately complete any merge.
    ///
    /// A vacant layer returns `None`, other variants return the merged batch.
    /// This consumes the layer, though we should probably consider returning
    /// the resources of the underlying source batches if we can manage that.
    fn complete(&mut self) -> Option<B>  {
        match std::mem::replace(self, MergeState::Vacant) {
            MergeState::Vacant => None,
            MergeState::Single(batch) => Some(batch),
            MergeState::Double(b1, b2, frontier, mut merge) => {
                let mut fuel = usize::max_value();
                merge.work(&b1, &b2, &frontier, &mut fuel);
                assert!(fuel > 0);
                let finished = merge.done();
                // logger.as_ref().map(|l|
                //     l.log(::logging::MergeEvent {
                //         operator,
                //         scale,
                //         length1: b1.len(),
                //         length2: b2.len(),
                //         complete: Some(finished.len()),
                //     })
                // );
                Some(finished)
            },
        }
    }

    /// Performs a bounded amount of work towards a merge.
    ///
    /// If the merge completes, the resulting batch is returned.
    /// If a batch is returned, it is the obligation of the caller
    /// to correctly install the result.
    fn work(&mut self, fuel: &mut usize) -> Option<B> {
        match std::mem::replace(self, MergeState::Vacant) {
            MergeState::Double(b1, b2, frontier, mut merge) => {
                merge.work(&b1, &b2, &frontier, fuel);
                if *fuel > 0 {
                    let finished = merge.done();
                    // logger.as_ref().map(|l|
                    //     l.log(::logging::MergeEvent {
                    //         operator,
                    //         scale,
                    //         length1: b1.len(),
                    //         length2: b2.len(),
                    //         complete: Some(finished.len()),
                    //     })
                    // );
                    Some(finished)
                }
                else {
                    *self = MergeState::Double(b1, b2, frontier, merge);
                    None
                }
            }
            x => {
                *self = x;
                None
            },
        }
    }

    /// Extract the merge state, typically temporarily.
    fn take(&mut self) -> Self {
        std::mem::replace(self, MergeState::Vacant)
    }

    /// Inserts a batch and begins a merge if needed.
    fn insert(&mut self, batch: B, frontier: Option<Vec<T>>) {
        match self.take() {
            MergeState::Vacant => {
                *self = MergeState::Single(batch);
            },
            MergeState::Single(batch_old) => {
                // logger.as_ref().map(|l| l.log(
                //     ::logging::MergeEvent {
                //         operator,
                //         scale,
                //         length1: batch1.len(),
                //         length2: batch2.len(),
                //         complete: None,
                //     }
                // ));
                *self = MergeState::begin_merge(batch_old, batch, frontier);
            }
            MergeState::Double(_,_,_,_) => {
                panic!("Attempted to insert batch into incomplete merge!");
            }
        };
    }

    fn begin_merge(batch1: B, batch2: B, frontier: Option<Vec<T>>) -> Self {
        assert!(batch1.upper() == batch2.lower());
        let begin_merge = <B as Batch<K, V, T, R>>::begin_merge(&batch1, &batch2);
        MergeState::Double(batch1, batch2, frontier, begin_merge)
    }

}