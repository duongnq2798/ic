use crate::{
    mock_time,
    types::{
        arbitrary,
        ids::{canister_test_id, message_test_id, subnet_test_id, user_test_id},
        messages::{RequestBuilder, SignedIngressBuilder},
    },
};
use ic_base_types::NumSeconds;
use ic_btc_types_internal::BitcoinAdapterRequestWrapper;
use ic_ic00_types::CanisterStatusType;
use ic_registry_routing_table::{CanisterIdRange, RoutingTable};
use ic_registry_subnet_features::SubnetFeatures;
use ic_registry_subnet_type::SubnetType;
use ic_replicated_state::{
    bitcoin_state::BitcoinState,
    canister_state::{
        execution_state::{CustomSection, CustomSectionType, WasmBinary, WasmMetadata},
        testing::new_canister_queues_for_test,
    },
    metadata_state::subnet_call_context_manager::{
        BitcoinGetSuccessorsContext, BitcoinSendTransactionInternalContext,
    },
    metadata_state::Stream,
    page_map::PageMap,
    testing::{CanisterQueuesTesting, ReplicatedStateTesting, SystemStateTesting},
    CallContext, CallOrigin, CanisterState, CanisterStatus, ExecutionState, ExportedFunctions,
    InputQueueType, Memory, NumWasmPages, ReplicatedState, SchedulerState, SystemState,
};
use ic_types::messages::CallbackId;
use ic_types::methods::{Callback, WasmClosure};
use ic_types::time::UNIX_EPOCH;
use ic_types::{
    messages::{Ingress, Request, RequestOrResponse},
    xnet::{StreamHeader, StreamIndex, StreamIndexedQueue},
    CanisterId, ComputeAllocation, Cycles, ExecutionRound, MemoryAllocation, NumBytes, PrincipalId,
    SubnetId, Time,
};
use ic_wasm_types::CanisterModule;
use proptest::prelude::*;
use std::convert::TryFrom;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};

const WASM_PAGE_SIZE_BYTES: usize = 65536;
const DEFAULT_FREEZE_THRESHOLD: NumSeconds = NumSeconds::new(1 << 30);
const INITIAL_CYCLES: Cycles = Cycles::new(5_000_000_000_000);

pub struct ReplicatedStateBuilder {
    canisters: Vec<CanisterState>,
    subnet_type: SubnetType,
    subnet_id: SubnetId,
    batch_time: Time,
    subnet_features: SubnetFeatures,
    bitcoin_state: BitcoinState,
    bitcoin_adapter_requests: Vec<BitcoinAdapterRequestWrapper>,
}

impl ReplicatedStateBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_subnet_id(mut self, subnet_id: SubnetId) -> Self {
        self.subnet_id = subnet_id;
        self
    }

    pub fn with_canister(mut self, canister: CanisterState) -> Self {
        self.canisters.push(canister);
        self
    }

    pub fn with_subnet_type(mut self, subnet_type: SubnetType) -> Self {
        self.subnet_type = subnet_type;
        self
    }

    pub fn with_time(mut self, time: Time) -> Self {
        self.batch_time = time;
        self
    }

    pub fn with_subnet_features(mut self, subnet_features: SubnetFeatures) -> Self {
        self.subnet_features = subnet_features;
        self
    }

    pub fn with_bitcoin_adapter_requests(
        mut self,
        bitcoin_adapter_requests: Vec<BitcoinAdapterRequestWrapper>,
    ) -> Self {
        self.bitcoin_adapter_requests = bitcoin_adapter_requests;
        self
    }

    pub fn with_bitcoin_state(mut self, state: BitcoinState) -> Self {
        self.bitcoin_state = state;
        self
    }

    pub fn build(self) -> ReplicatedState {
        let mut state = ReplicatedState::new(self.subnet_id, self.subnet_type);

        for canister in self.canisters {
            state.put_canister_state(canister);
        }
        let mut routing_table = RoutingTable::new();
        routing_table
            .insert(
                CanisterIdRange {
                    start: CanisterId::from(0),
                    end: CanisterId::from(u64::MAX),
                },
                self.subnet_id,
            )
            .unwrap();
        state.metadata.network_topology.routing_table = Arc::new(routing_table);

        state.metadata.batch_time = self.batch_time;
        state.metadata.own_subnet_features = self.subnet_features;
        state.put_bitcoin_state(self.bitcoin_state);

        for request in self.bitcoin_adapter_requests.into_iter() {
            match request {
                BitcoinAdapterRequestWrapper::GetSuccessorsRequest(_)
                | BitcoinAdapterRequestWrapper::SendTransactionRequest(_) => unreachable!(),
                BitcoinAdapterRequestWrapper::CanisterGetSuccessorsRequest(payload) => {
                    state
                        .metadata
                        .subnet_call_context_manager
                        .push_bitcoin_get_successors_request(BitcoinGetSuccessorsContext {
                            request: RequestBuilder::default().build(),
                            payload,
                            time: mock_time(),
                        });
                }
                BitcoinAdapterRequestWrapper::CanisterSendTransactionRequest(payload) => {
                    state
                        .metadata
                        .subnet_call_context_manager
                        .push_bitcoin_send_transaction_internal_request(
                            BitcoinSendTransactionInternalContext {
                                request: RequestBuilder::default().build(),
                                payload,
                                time: mock_time(),
                            },
                        );
                }
            }
        }

        state
    }
}

impl Default for ReplicatedStateBuilder {
    fn default() -> Self {
        Self {
            canisters: Vec::new(),
            subnet_type: SubnetType::Application,
            subnet_id: subnet_test_id(1),
            batch_time: mock_time(),
            subnet_features: SubnetFeatures::default(),
            bitcoin_state: BitcoinState::default(),
            bitcoin_adapter_requests: Vec::new(),
        }
    }
}

pub struct CanisterStateBuilder {
    canister_id: CanisterId,
    controller: PrincipalId,
    cycles: Cycles,
    stable_memory: Option<Vec<u8>>,
    wasm: Option<Vec<u8>>,
    memory_allocation: MemoryAllocation,
    compute_allocation: ComputeAllocation,
    ingress_queue: Vec<Ingress>,
    status: CanisterStatusType,
    freeze_threshold: NumSeconds,
    call_contexts: Vec<CallContext>,
    inputs: Vec<RequestOrResponse>,
    time_of_last_allocation_charge: Time,
}

impl CanisterStateBuilder {
    pub fn new() -> Self {
        // Initialize with sensible defaults.
        Self::default()
    }

    pub fn with_canister_id(mut self, canister_id: CanisterId) -> Self {
        self.canister_id = canister_id;
        self
    }

    pub fn with_controller<P: Into<PrincipalId>>(mut self, controller: P) -> Self {
        self.controller = controller.into();
        self
    }

    pub fn with_stable_memory(mut self, data: Vec<u8>) -> Self {
        self.stable_memory = Some(data);
        self
    }

    pub fn with_cycles<C: Into<Cycles>>(mut self, cycles: C) -> Self {
        self.cycles = cycles.into();
        self
    }

    pub fn with_wasm(mut self, wasm: Vec<u8>) -> Self {
        self.wasm = Some(wasm);
        self
    }

    pub fn with_memory_allocation<B: Into<NumBytes>>(mut self, num_bytes: B) -> Self {
        self.memory_allocation = MemoryAllocation::try_from(num_bytes.into()).unwrap();
        self
    }

    pub fn with_compute_allocation(mut self, allocation: ComputeAllocation) -> Self {
        self.compute_allocation = allocation;
        self
    }

    pub fn with_ingress(mut self, ingress: Ingress) -> Self {
        self.ingress_queue.push(ingress);
        self
    }

    pub fn with_status(mut self, status: CanisterStatusType) -> Self {
        self.status = status;
        self
    }

    pub fn with_freezing_threshold<S: Into<NumSeconds>>(mut self, ft: S) -> Self {
        self.freeze_threshold = ft.into();
        self
    }

    pub fn with_call_context(mut self, call_context: CallContext) -> Self {
        self.call_contexts.push(call_context);
        self
    }

    pub fn with_input(mut self, input: RequestOrResponse) -> Self {
        self.inputs.push(input);
        self
    }

    pub fn with_canister_request(mut self, request: Request) -> Self {
        self.inputs.push(request.into());
        self
    }

    pub fn with_time_of_last_allocation_charge(mut self, time: Time) -> Self {
        self.time_of_last_allocation_charge = time;
        self
    }

    pub fn build(self) -> CanisterState {
        let mut system_state = match self.status {
            CanisterStatusType::Running => SystemState::new_running(
                self.canister_id,
                self.controller,
                self.cycles,
                self.freeze_threshold,
            ),
            CanisterStatusType::Stopping => SystemState::new_stopping(
                self.canister_id,
                self.controller,
                self.cycles,
                self.freeze_threshold,
            ),
            CanisterStatusType::Stopped => SystemState::new_stopped(
                self.canister_id,
                self.controller,
                self.cycles,
                self.freeze_threshold,
            ),
        };

        system_state.memory_allocation = self.memory_allocation;

        // Add ingress messages to the canister's queues.
        for ingress in self.ingress_queue.into_iter() {
            system_state.queues_mut().push_ingress(ingress)
        }

        // Set call contexts. Because there is no way pass in a `CallContext`
        // object to `CallContextManager`, we have to construct them in this
        // bizarre way.
        for call_context in self.call_contexts.into_iter() {
            let call_context_manager = system_state.call_context_manager_mut().unwrap();
            let call_context_id = call_context_manager.new_call_context(
                call_context.call_origin().clone(),
                call_context.available_cycles(),
                call_context.time().unwrap(),
            );

            let call_context_in_call_context_manager = call_context_manager
                .call_context_mut(call_context_id)
                .unwrap();
            if call_context.has_responded() {
                call_context_in_call_context_manager.mark_responded();
            }
            if call_context.is_deleted() {
                call_context_in_call_context_manager.mark_deleted();
            }
        }

        // Add inputs to the input queue.
        for input in self.inputs {
            system_state
                .queues_mut()
                .push_input(input, InputQueueType::RemoteSubnet)
                .unwrap();
        }

        let stable_memory = if let Some(data) = self.stable_memory {
            Memory::new(
                PageMap::from(&data[..]),
                NumWasmPages::new((data.len() / WASM_PAGE_SIZE_BYTES) + 1),
            )
        } else {
            Memory::new_for_testing()
        };

        let execution_state = match self.wasm {
            Some(wasm_binary) => {
                let mut ee = initial_execution_state();
                ee.wasm_binary = WasmBinary::new(CanisterModule::new(wasm_binary));
                ee.stable_memory = stable_memory;
                Some(ee)
            }
            None => None,
        };

        CanisterState {
            system_state,
            execution_state,
            scheduler_state: SchedulerState {
                compute_allocation: self.compute_allocation,
                time_of_last_allocation_charge: self.time_of_last_allocation_charge,
                ..SchedulerState::default()
            },
        }
    }
}

impl Default for CanisterStateBuilder {
    fn default() -> Self {
        Self {
            canister_id: canister_test_id(0),
            controller: user_test_id(0).get(),
            cycles: INITIAL_CYCLES,
            stable_memory: None,
            wasm: None,
            memory_allocation: MemoryAllocation::BestEffort,
            compute_allocation: ComputeAllocation::zero(),
            ingress_queue: Vec::default(),
            status: CanisterStatusType::Running,
            freeze_threshold: DEFAULT_FREEZE_THRESHOLD,
            call_contexts: Vec::default(),
            inputs: Vec::default(),
            time_of_last_allocation_charge: UNIX_EPOCH,
        }
    }
}

pub struct SystemStateBuilder {
    system_state: SystemState,
}

impl Default for SystemStateBuilder {
    fn default() -> Self {
        Self {
            system_state: SystemState::new_running(
                canister_test_id(42),
                user_test_id(24).get(),
                INITIAL_CYCLES,
                DEFAULT_FREEZE_THRESHOLD,
            ),
        }
    }
}

impl SystemStateBuilder {
    pub fn new() -> Self {
        Self {
            system_state: SystemState::new_running(
                canister_test_id(42),
                user_test_id(24).get(),
                INITIAL_CYCLES,
                DEFAULT_FREEZE_THRESHOLD,
            ),
        }
    }

    pub fn initial_cycles(mut self, cycles: Cycles) -> Self {
        self.system_state.set_balance(cycles);
        self
    }

    pub fn canister_id(mut self, canister_id: CanisterId) -> Self {
        self.system_state.set_canister_id(canister_id);
        self
    }

    pub fn memory_allocation(mut self, memory_allocation: NumBytes) -> Self {
        self.system_state.memory_allocation =
            MemoryAllocation::try_from(memory_allocation).unwrap();
        self
    }

    pub fn freeze_threshold(mut self, threshold: NumSeconds) -> Self {
        self.system_state.freeze_threshold = threshold;
        self
    }

    pub fn build(self) -> SystemState {
        self.system_state
    }
}

pub struct CallContextBuilder {
    call_origin: CallOrigin,
    responded: bool,
    time: Time,
}

impl CallContextBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_call_origin(mut self, call_origin: CallOrigin) -> Self {
        self.call_origin = call_origin;
        self
    }

    pub fn with_responded(mut self, responded: bool) -> Self {
        self.responded = responded;
        self
    }

    pub fn with_time(mut self, time: Time) -> Self {
        self.time = time;
        self
    }

    pub fn build(self) -> CallContext {
        CallContext::new(
            self.call_origin,
            self.responded,
            false,
            Cycles::zero(),
            self.time,
        )
    }
}

impl Default for CallContextBuilder {
    fn default() -> Self {
        Self {
            call_origin: CallOrigin::Ingress(user_test_id(0), message_test_id(0)),
            responded: false,
            time: Time::from_nanos_since_unix_epoch(0),
        }
    }
}

pub fn initial_execution_state() -> ExecutionState {
    let mut metadata: BTreeMap<String, CustomSection> = BTreeMap::new();
    metadata.insert(
        String::from("candid"),
        CustomSection::new(CustomSectionType::Private, vec![0, 2]),
    );
    metadata.insert(
        String::from("dummy"),
        CustomSection::new(CustomSectionType::Public, vec![2, 1]),
    );
    let wasm_metadata = WasmMetadata::new(metadata);

    ExecutionState {
        canister_root: "NOT_USED".into(),
        session_nonce: None,
        wasm_binary: WasmBinary::new(CanisterModule::new(vec![])),
        wasm_memory: Memory::new_for_testing(),
        stable_memory: Memory::new_for_testing(),
        exported_globals: vec![],
        exports: ExportedFunctions::new(BTreeSet::new()),
        metadata: wasm_metadata,
        last_executed_round: ExecutionRound::from(0),
    }
}

pub fn canister_from_exec_state(
    execution_state: ExecutionState,
    canister_id: CanisterId,
) -> CanisterState {
    CanisterState {
        system_state: SystemStateBuilder::new()
            .memory_allocation(NumBytes::new(8 * 1024 * 1024 * 1024)) // 8GiB
            .canister_id(canister_id)
            .initial_cycles(INITIAL_CYCLES)
            .build(),
        execution_state: Some(execution_state),
        scheduler_state: Default::default(),
    }
}

pub fn get_running_canister_with_balance(
    canister_id: CanisterId,
    initial_balance: Cycles,
) -> CanisterState {
    get_running_canister_with_args(canister_id, user_test_id(1).get(), initial_balance)
}

pub fn get_running_canister(canister_id: CanisterId) -> CanisterState {
    get_running_canister_with_balance(canister_id, INITIAL_CYCLES)
}

pub fn get_running_canister_with_args(
    canister_id: CanisterId,
    controller: PrincipalId,
    initial_cycles: Cycles,
) -> CanisterState {
    CanisterState {
        system_state: SystemState::new_running(
            canister_id,
            controller,
            initial_cycles,
            DEFAULT_FREEZE_THRESHOLD,
        ),
        execution_state: None,
        scheduler_state: Default::default(),
    }
}

pub fn get_stopping_canister(canister_id: CanisterId) -> CanisterState {
    get_stopping_canister_with_controller(canister_id, user_test_id(1).get())
}

pub fn get_stopping_canister_on_nns(canister_id: CanisterId) -> CanisterState {
    get_stopping_canister_with_controller(canister_id, user_test_id(1).get())
}

pub fn get_stopping_canister_with_controller(
    canister_id: CanisterId,
    controller: PrincipalId,
) -> CanisterState {
    CanisterState {
        system_state: SystemState::new_stopping(
            canister_id,
            controller,
            INITIAL_CYCLES,
            DEFAULT_FREEZE_THRESHOLD,
        ),
        execution_state: None,
        scheduler_state: Default::default(),
    }
}

pub fn get_stopped_canister_on_system_subnet(canister_id: CanisterId) -> CanisterState {
    get_stopped_canister_with_controller(canister_id, user_test_id(1).get())
}

pub fn get_stopped_canister(canister_id: CanisterId) -> CanisterState {
    get_stopped_canister_with_controller(canister_id, user_test_id(1).get())
}

pub fn get_stopped_canister_with_controller(
    canister_id: CanisterId,
    controller: PrincipalId,
) -> CanisterState {
    CanisterState {
        system_state: SystemState::new_stopped(
            canister_id,
            controller,
            INITIAL_CYCLES,
            DEFAULT_FREEZE_THRESHOLD,
        ),
        execution_state: None,
        scheduler_state: Default::default(),
    }
}

/// Convert a running canister into a stopped canister. This functionality
/// is added here since it is only allowed in tests.
pub fn running_canister_into_stopped(mut canister: CanisterState) -> CanisterState {
    canister.system_state.status = CanisterStatus::Stopped;
    canister
}

/// Returns a `ReplicatedState` with SubnetType::Application, variable amount of canisters, input
/// messages per canister and methods that are to be called.
pub fn get_initial_state(canister_num: u64, message_num_per_canister: u64) -> ReplicatedState {
    get_initial_state_with_balance(
        canister_num,
        message_num_per_canister,
        INITIAL_CYCLES,
        SubnetType::Application,
    )
}

/// Returns a `ReplicatedState` with SubnetType::System, variable amount of canisters, input
/// messages per canister and methods that are to be called.
pub fn get_initial_system_subnet_state(
    canister_num: u64,
    message_num_per_canister: u64,
) -> ReplicatedState {
    get_initial_state_with_balance(
        canister_num,
        message_num_per_canister,
        INITIAL_CYCLES,
        SubnetType::System,
    )
}

pub fn get_initial_state_with_balance(
    canister_num: u64,
    message_num_per_canister: u64,
    initial_cycles: Cycles,
    own_subnet_type: SubnetType,
) -> ReplicatedState {
    let mut state = ReplicatedState::new(subnet_test_id(1), own_subnet_type);

    for canister_id in 0..canister_num {
        let mut canister_state_builder = CanisterStateBuilder::new()
            .with_canister_id(canister_test_id(canister_id))
            .with_cycles(initial_cycles)
            .with_wasm(vec![]);

        for i in 0..message_num_per_canister {
            canister_state_builder = canister_state_builder.with_ingress(
                (
                    SignedIngressBuilder::new()
                        .canister_id(canister_test_id(canister_id))
                        .nonce(i)
                        .build(),
                    None,
                )
                    .into(),
            );
        }

        state.put_canister_state(canister_state_builder.build());
    }
    state.metadata.network_topology.routing_table = Arc::new({
        let mut rt = ic_registry_routing_table::RoutingTable::new();
        rt.insert(
            ic_registry_routing_table::CanisterIdRange {
                start: CanisterId::from(0),
                end: CanisterId::from(u64::MAX),
            },
            subnet_test_id(1),
        )
        .unwrap();
        rt
    });
    state
}

/// Returns the ordered IDs of the canisters contained within `state`.
pub fn canister_ids(state: &ReplicatedState) -> Vec<CanisterId> {
    state
        .canisters_iter()
        .map(|canister_state| canister_state.canister_id())
        .collect()
}

pub fn new_canister_state(
    canister_id: CanisterId,
    controller: PrincipalId,
    initial_cycles: Cycles,
    freeze_threshold: NumSeconds,
) -> CanisterState {
    let scheduler_state = SchedulerState::default();
    let system_state =
        SystemState::new_running(canister_id, controller, initial_cycles, freeze_threshold);
    CanisterState::new(system_state, None, scheduler_state)
}

/// Helper function to register a callback.
pub fn register_callback(
    canister_state: &mut CanisterState,
    originator: CanisterId,
    respondent: CanisterId,
    callback_id: CallbackId,
) {
    let call_context_manager = canister_state
        .system_state
        .call_context_manager_mut()
        .unwrap();
    let call_context_id = call_context_manager.new_call_context(
        CallOrigin::CanisterUpdate(originator, callback_id),
        Cycles::zero(),
        Time::from_nanos_since_unix_epoch(0),
    );

    call_context_manager.register_callback(Callback::new(
        call_context_id,
        Some(originator),
        Some(respondent),
        Cycles::zero(),
        Some(Cycles::new(42)),
        Some(Cycles::new(84)),
        WasmClosure::new(0, 2),
        WasmClosure::new(0, 2),
        None,
    ));
}

/// Helper function to insert a canister in the provided `ReplicatedState`.
pub fn insert_dummy_canister(
    state: &mut ReplicatedState,
    canister_id: CanisterId,
    controller: PrincipalId,
) {
    let wasm = CanisterModule::new(vec![]);
    let mut canister_state = new_canister_state(
        canister_id,
        controller,
        INITIAL_CYCLES,
        NumSeconds::from(100_000),
    );
    let mut execution_state = initial_execution_state();
    execution_state.wasm_binary = WasmBinary::new(wasm);
    canister_state.execution_state = Some(execution_state);
    state.put_canister_state(canister_state);
}

prop_compose! {
    /// Produces a strategy that generates an arbitrary `signals_end` and between
    /// `[sig_min_size, sig_max_size]` reject signals .
    pub fn arb_reject_signals(sig_min_size: usize, sig_max_size: usize)(
        sig_start in 0..10000u64,
        sigs in prop::collection::btree_set(arbitrary::stream_index(100 + sig_max_size as u64), sig_min_size..=sig_max_size),
        sig_end_delta in 0..10u64,
    ) -> (StreamIndex, VecDeque<StreamIndex>) {
        let mut reject_signals = VecDeque::with_capacity(sigs.len());
        let sig_start = sig_start.into();
        for s in sigs {
            reject_signals.push_back(s + sig_start);
        }
        let sig_end = sig_start + reject_signals.back().unwrap_or(&0.into()).increment() + sig_end_delta.into();
        (sig_end, reject_signals)
    }
}

prop_compose! {
    /// Produces a strategy that generates a stream with between
    /// `[min_size, max_size]` messages and between `[sig_min_size, sig_max_size]`
    /// reject signals.
    pub fn arb_stream(min_size: usize, max_size: usize, sig_min_size: usize, sig_max_size: usize)(
        msg_start in 0..10000u64,
        reqs in prop::collection::vec(arbitrary::request(), min_size..=max_size),
        (signals_end, reject_signals) in arb_reject_signals(sig_min_size, sig_max_size),
    ) -> Stream {
        let mut messages = StreamIndexedQueue::with_begin(StreamIndex::from(msg_start));
        for r in reqs {
            messages.push(r.into())
        }

        Stream::with_signals(messages, signals_end, reject_signals)
    }
}

prop_compose! {
    /// Produces a strategy consisting of an arbitrary stream and valid slice begin and message
    /// count values for extracting a slice from the stream.
    pub fn arb_stream_slice(min_size: usize, max_size: usize, sig_min_size: usize, sig_max_size: usize)(
        stream in arb_stream(min_size, max_size, sig_min_size, sig_max_size),
        from_percent in -20..120i64,
        percent_above_min_size in 0..120i64,
    ) ->  (Stream, StreamIndex, usize) {
        let from_percent = from_percent.max(0).min(100) as usize;
        let percent_above_min_size = percent_above_min_size.max(0).min(100) as usize;
        let msg_count = min_size +
            (stream.messages().len() - min_size) * percent_above_min_size / 100;
        let from = stream.messages_begin() +
            (((stream.messages().len() - msg_count) * from_percent / 100) as u64).into();

        (stream, from, msg_count)
    }
}

prop_compose! {
    pub fn arb_stream_header(sig_min_size: usize, sig_max_size: usize)(
        msg_start in 0..10000u64,
        msg_len in 0..10000u64,
        (signals_end, reject_signals) in arb_reject_signals(sig_min_size, sig_max_size),
    ) -> StreamHeader {
        let begin = StreamIndex::from(msg_start);
        let end = StreamIndex::from(msg_start + msg_len);

        StreamHeader {
            begin,
            end,
            signals_end,
            reject_signals,
        }
    }
}

prop_compose! {
    /// Strategy that generates an arbitrary number (of receivers) between 1 and the
    /// provided value, if `Some`; or else `usize::MAX` (standing for unlimited
    /// receivers).
    pub fn arb_num_receivers(max_receivers: Option<usize>) (
            random in 0..usize::MAX,
        ) -> usize {
        match max_receivers {
            Some(max_receivers) if max_receivers <= 1 => 1,
            Some(max_receivers) => 1 + random % (max_receivers - 1),
            None => usize::MAX,
        }
    }
}

/// Produces a `ReplicatedState` with the given subnet ID and the given output
/// requests. First group of requests are enqueud into the subnet queues; a
/// canister is created for each following group. Each group's requests are
/// routed round-robin to one of `num_receivers`.
///
/// Returns the generated `ReplicatedState`; the requests grouped by canister,
/// in expected iteration order; and the total number of requests.
fn new_replicated_state_for_test(
    own_subnet_id: SubnetId,
    mut output_requests: Vec<Vec<Request>>,
    num_receivers: usize,
) -> (
    ReplicatedState,
    VecDeque<VecDeque<RequestOrResponse>>,
    usize,
) {
    let mut total_requests = 0;
    let mut requests = VecDeque::new();

    let subnet_queues = if let Some(reqs) = output_requests.pop() {
        let (queues, raw_requests) =
            new_canister_queues_for_test(reqs, CanisterId::from(own_subnet_id), num_receivers);
        total_requests += raw_requests.len();
        requests.push_back(raw_requests);
        Some(queues)
    } else {
        None
    };

    let canister_states: BTreeMap<_, _> = output_requests
        .into_iter()
        .enumerate()
        .map(|(i, reqs)| {
            let canister_id = CanisterId::from_u64(i as u64);
            let mut canister = CanisterStateBuilder::new()
                .with_canister_id(canister_id)
                .build();
            let (queues, raw_requests) =
                new_canister_queues_for_test(reqs, canister_test_id(i as u64), num_receivers);
            canister.system_state.put_queues(queues);
            total_requests += raw_requests.len();
            requests.push_back(raw_requests);
            (canister_id, canister)
        })
        .collect();

    let mut replicated_state = ReplicatedStateBuilder::new().build();

    let mut routing_table = RoutingTable::new();
    routing_table
        .insert(
            CanisterIdRange {
                start: CanisterId::from(0),
                end: CanisterId::from(u64::MAX),
            },
            own_subnet_id,
        )
        .unwrap();
    replicated_state.metadata.network_topology.routing_table = Arc::new(routing_table);

    replicated_state.put_canister_states(canister_states);
    if let Some(subnet_queues) = subnet_queues {
        replicated_state.put_subnet_queues(subnet_queues);
    }

    (replicated_state, requests, total_requests)
}

prop_compose! {
     pub fn arb_replicated_state_with_queues(
        own_subnet_id: SubnetId,
        max_canisters: usize,
        max_requests_per_canister: usize,
        max_receivers: Option<usize>,
    ) (
        time in 1..1000_u64,
        request_queues in prop::collection::vec(prop::collection::vec(arbitrary::request(), 0..=max_requests_per_canister), 0..=max_canisters),
        num_receivers in arb_num_receivers(max_receivers)
    ) -> (ReplicatedState, VecDeque<VecDeque<RequestOrResponse>>, usize) {
        use rand::{Rng, SeedableRng};
        use rand_chacha::ChaChaRng;

        let (mut replicated_state, mut raw_requests, total_requests) = new_replicated_state_for_test(own_subnet_id, request_queues, num_receivers);

        // We pseudorandomly rotate the queues to match the rotation applied by the iterator.
        // Note that subnet queues are always at the front which is why we need to pop them
        // before the rotation and push them to the front afterwards.
        let subnet_queue_requests = raw_requests.pop_front();
        let mut raw_requests : VecDeque<_> = raw_requests.into_iter().filter(|requests| !requests.is_empty()).collect();

        replicated_state.metadata.batch_time = Time::from_nanos_since_unix_epoch(time);
        let mut rng = ChaChaRng::seed_from_u64(time);
        let rotation = rng.gen_range(0..raw_requests.len().max(1));
        raw_requests.rotate_left(rotation);

        if let Some(requests) = subnet_queue_requests {
            raw_requests.push_front(requests);
        }

        (replicated_state, raw_requests, total_requests)
    }
}
