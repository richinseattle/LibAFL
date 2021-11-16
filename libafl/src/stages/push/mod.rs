//! While normal stages call the executor over and over again, push stages turn this concept upside down:
//! A push stage instead returns an iterator that generates a new result for each time it gets called.
//! With the new testcase, you will have to take care about testcase execution, manually.
//! The push stage relies on internal muttability of the supplied `Observers`.
//!

/// Mutational stage is the normal fuzzing stage,
pub mod mutational;
pub use mutational::StdMutationalPushStage;

use alloc::rc::Rc;
use core::{
    cell::{Cell, RefCell},
    marker::PhantomData,
    time::Duration,
};

use crate::{
    bolts::{current_time, rands::Rand},
    corpus::{Corpus, CorpusScheduler},
    events::EventManager,
    executors::ExitKind,
    inputs::Input,
    observers::ObserversTuple,
    state::{HasClientPerfMonitor, HasCorpus, HasRand},
    Error, EvaluatorObservers, ExecutionProcessor, Fuzzer, HasCorpusScheduler,
};

/// Send a monitor update all 15 (or more) seconds
const STATS_TIMEOUT_DEFAULT: Duration = Duration::from_secs(15);

// The shared state for all [`PushStage`]s
/// Should be stored inside a `[Rc<RefCell<_>>`]
#[derive(Clone, Debug)]
pub struct PushStageSharedState<C, CS, EM, I, OT, R, S, Z>
where
    C: Corpus<I>,
    CS: CorpusScheduler<I, S>,
    EM: EventManager<(), I, S, Z>,
    I: Input,
    OT: ObserversTuple<I, S>,
    R: Rand,
    S: HasClientPerfMonitor + HasCorpus<C, I> + HasRand<R>,
    Z: ExecutionProcessor<I, OT, S>
        + EvaluatorObservers<I, OT, S>
        + Fuzzer<(), EM, I, S, ()>
        + HasCorpusScheduler<CS, I, S>,
{
    /// The [`State`]
    pub state: S,
    /// The [`Fuzzer`] instance
    pub fuzzer: Z,
    /// The [`EventManager`]
    pub event_mgr: EM,
    /// The [`ObserverTuple`]
    pub observers: OT,
    phantom: PhantomData<(C, CS, I, OT, R, S, Z)>,
}

impl<C, CS, EM, I, OT, R, S, Z> PushStageSharedState<C, CS, EM, I, OT, R, S, Z>
where
    C: Corpus<I>,
    CS: CorpusScheduler<I, S>,
    EM: EventManager<(), I, S, Z>,
    I: Input,
    OT: ObserversTuple<I, S>,
    R: Rand,
    S: HasClientPerfMonitor + HasCorpus<C, I> + HasRand<R>,
    Z: ExecutionProcessor<I, OT, S>
        + EvaluatorObservers<I, OT, S>
        + Fuzzer<(), EM, I, S, ()>
        + HasCorpusScheduler<CS, I, S>,
{
    /// Create a new `PushStageSharedState` that can be used by all [`PushStage`]s
    #[must_use]
    pub fn new(fuzzer: Z, state: S, observers: OT, event_mgr: EM) -> Self {
        Self {
            state,
            fuzzer,
            event_mgr,
            observers,
            phantom: PhantomData,
        }
    }
}

/// Helper class for the [`PushStage`] trait, taking care of borrowing the shared state
#[derive(Clone, Debug)]
pub struct PushStageHelper<C, CS, EM, I, OT, R, S, Z>
where
    C: Corpus<I>,
    CS: CorpusScheduler<I, S>,
    EM: EventManager<(), I, S, Z>,
    I: Input,
    OT: ObserversTuple<I, S>,
    R: Rand,
    S: HasClientPerfMonitor + HasCorpus<C, I> + HasRand<R>,
    Z: ExecutionProcessor<I, OT, S>
        + EvaluatorObservers<I, OT, S>
        + Fuzzer<(), EM, I, S, ()>
        + HasCorpusScheduler<CS, I, S>,
{
    /// If this stage has already been initalized.
    /// This gets reset to `false` after one iteration of the stage is done.
    pub initialized: bool,
    /// The last time the monitor was updated
    pub last_monitor_time: Duration,
    /// The shared state, keeping track of the corpus and the fuzzer
    #[allow(clippy::type_complexity)]
    pub shared_state: Rc<RefCell<Option<PushStageSharedState<C, CS, EM, I, OT, R, S, Z>>>>,
    /// If the last iteraation failed
    pub errored: bool,

    #[allow(clippy::type_complexity)]
    phantom: PhantomData<(C, CS, (), EM, I, R, OT, S, Z)>,
    exit_kind: Rc<Cell<Option<ExitKind>>>,
}

impl<C, CS, EM, I, OT, R, S, Z> PushStageHelper<C, CS, EM, I, OT, R, S, Z>
where
    C: Corpus<I>,
    CS: CorpusScheduler<I, S>,
    EM: EventManager<(), I, S, Z>,
    I: Input,
    OT: ObserversTuple<I, S>,
    R: Rand,
    S: HasClientPerfMonitor + HasCorpus<C, I> + HasRand<R>,
    Z: ExecutionProcessor<I, OT, S>
        + EvaluatorObservers<I, OT, S>
        + Fuzzer<(), EM, I, S, ()>
        + HasCorpusScheduler<CS, I, S>,
{
    /// Create a new [`PushStageHelper`]
    #[must_use]
    #[allow(clippy::type_complexity)]
    pub fn new(
        shared_state: Rc<RefCell<Option<PushStageSharedState<C, CS, EM, I, OT, R, S, Z>>>>,
        exit_kind_ref: Rc<Cell<Option<ExitKind>>>,
    ) -> Self {
        Self {
            shared_state,
            initialized: false,
            phantom: PhantomData,
            last_monitor_time: current_time(),
            exit_kind: exit_kind_ref,
            errored: false,
        }
    }

    /// Sets the shared state for this helper (and all other helpers owning the same [`RefCell`])
    #[inline]
    pub fn set_shared_state(
        &mut self,
        shared_state: PushStageSharedState<C, CS, EM, I, OT, R, S, Z>,
    ) {
        (&mut *self.shared_state.borrow_mut()).replace(shared_state);
    }

    /// Takes the shared state from this helper, replacing it with `None`
    #[inline]
    #[allow(clippy::type_complexity)]
    pub fn take_shared_state(&mut self) -> Option<PushStageSharedState<C, CS, EM, I, OT, R, S, Z>> {
        let shared_state_ref = &mut (*self.shared_state).borrow_mut();
        shared_state_ref.take()
    }

    /// Returns the exit kind of the last run
    #[inline]
    #[must_use]
    pub fn exit_kind(&self) -> Option<ExitKind> {
        self.exit_kind.get()
    }

    /// Resets the exit kind
    #[inline]
    pub fn reset_exit_kind(&mut self) {
        self.exit_kind.set(None);
    }
}

/// A push stage is a generator that returns a single testcase for each call.
/// It's an iterator so we can chain it.
/// After it has finished once, we will call it agan for the next fuzzer round.
pub trait PushStage<C, CS, EM, I, OT, R, S, Z>: Iterator
where
    C: Corpus<I>,
    CS: CorpusScheduler<I, S>,
    EM: EventManager<(), I, S, Z>,
    I: Input,
    OT: ObserversTuple<I, S>,
    R: Rand,
    S: HasClientPerfMonitor + HasCorpus<C, I> + HasRand<R>,
    Z: ExecutionProcessor<I, OT, S>
        + EvaluatorObservers<I, OT, S>
        + Fuzzer<(), EM, I, S, ()>
        + HasCorpusScheduler<CS, I, S>,
{
    /// Gets the [`PushStageHelper`]
    fn push_stage_helper(&self) -> &PushStageHelper<C, CS, EM, I, OT, R, S, Z>;
    /// Gets the [`PushStageHelper`], mut
    fn push_stage_helper_mut(&mut self) -> &mut PushStageHelper<C, CS, EM, I, OT, R, S, Z>;

    /// Called by `next_std` when this stage is being initialized.
    /// This is called before the first iteration of the stage.
    /// After the stage has finished once (after `deinit`), this will be called again.
    #[inline]
    fn init(
        &mut self,
        _shared_state: &mut PushStageSharedState<C, CS, EM, I, OT, R, S, Z>,
    ) -> Result<(), Error> {
        Ok(())
    }

    /// Called before the a test case is executed.
    /// Should return the test case to be executed.
    /// After this stage has finished, or if the stage does not process any inputs, this should return `None`.
    fn pre_exec(
        &mut self,
        shared_state: &mut PushStageSharedState<C, CS, EM, I, OT, R, S, Z>,
    ) -> Option<Result<I, Error>>;

    /// Called after the execution of a testcase finished.
    #[inline]
    fn post_exec(
        &mut self,
        _shared_state: &mut PushStageSharedState<C, CS, EM, I, OT, R, S, Z>,
        _exit_kind: ExitKind,
    ) -> Result<(), Error> {
        Ok(())
    }

    /// Called after the stage finished (`pre_exec` returned `None`)
    #[inline]
    fn deinit(
        &mut self,
        _shared_state: &mut PushStageSharedState<C, CS, EM, I, OT, R, S, Z>,
    ) -> Result<(), Error> {
        Ok(())
    }

    /// This is the default implementation for `next` for this stage
    fn next_std(&mut self) -> Option<Result<I, Error>> {
        let mut shared_state = {
            let shared_state_ref = &mut (*self.push_stage_helper_mut().shared_state).borrow_mut();
            shared_state_ref.take().unwrap()
        };

        let step_success = if self.push_stage_helper().initialized {
            // We already ran once
            self.post_exec(
                &mut shared_state,
                self.push_stage_helper().exit_kind().unwrap(),
            )
        } else {
            self.init(&mut shared_state)
        };
        if let Err(err) = step_success {
            self.push_stage_helper_mut().errored = true;
            self.push_stage_helper_mut().set_shared_state(shared_state);
            return Some(Err(err));
        }

        //for i in 0..num {
        let ret = self.pre_exec(&mut shared_state);
        if ret.is_none() {
            // We're done.
            self.push_stage_helper_mut().initialized = false;

            if let Err(err) = self.deinit(&mut shared_state) {
                self.push_stage_helper_mut().errored = true;
                self.push_stage_helper_mut().set_shared_state(shared_state);
                return Some(Err(err));
            };

            let last_monitor_time = self.push_stage_helper().last_monitor_time;

            let new_monitor_time = match Z::maybe_report_monitor(
                &mut shared_state.state,
                &mut shared_state.event_mgr,
                last_monitor_time,
                STATS_TIMEOUT_DEFAULT,
            ) {
                Ok(new_time) => new_time,
                Err(err) => {
                    self.push_stage_helper_mut().errored = true;
                    self.push_stage_helper_mut().set_shared_state(shared_state);
                    return Some(Err(err));
                }
            };

            self.push_stage_helper_mut().last_monitor_time = new_monitor_time;
            //self.fuzzer.maybe_report_monitor();
        } else {
            self.push_stage_helper_mut().reset_exit_kind();
        }
        self.push_stage_helper_mut().set_shared_state(shared_state);
        self.push_stage_helper_mut().errored = false;
        ret
    }
}
