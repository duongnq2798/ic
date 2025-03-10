pub mod cycles_balance_change;
mod request_in_prep;
mod routing;
pub mod sandbox_safe_system_state;
mod stable_memory;
pub mod system_api_empty;

use ic_config::flag_status::FlagStatus;
use ic_error_types::RejectCode;
use ic_interfaces::execution_environment::{
    ExecutionComplexity, ExecutionMode,
    HypervisorError::{self, *},
    HypervisorResult, OutOfInstructionsHandler, PerformanceCounterType, StableGrowOutcome,
    StableMemoryApi, SubnetAvailableMemory, SystemApi,
    TrapCode::{self, CyclesAmountTooBigFor64Bit},
};
use ic_logger::{error, ReplicaLogger};
use ic_registry_subnet_type::SubnetType;
use ic_replicated_state::{memory_required_to_push_request, Memory, NumWasmPages, PageIndex};
use ic_sys::PageBytes;
use ic_types::{
    ingress::WasmResult,
    messages::{CallContextId, RejectContext, Request, MAX_INTER_CANISTER_PAYLOAD_IN_BYTES},
    methods::{SystemMethod, WasmClosure},
    CanisterId, CanisterTimer, ComputeAllocation, Cycles, NumBytes, NumInstructions, NumPages,
    PrincipalId, SubnetId, Time,
};
use ic_utils::deterministic_operations::deterministic_copy_from_slice;
use request_in_prep::{into_request, RequestInPrep};
use sandbox_safe_system_state::{CanisterStatusView, SandboxSafeSystemState, SystemStateChanges};
use serde::{Deserialize, Serialize};
use stable_memory::StableMemory;
use std::{
    convert::{From, TryFrom},
    sync::Arc,
};

const MULTIPLIER_MAX_SIZE_LOCAL_SUBNET: u64 = 5;
const MAX_NON_REPLICATED_QUERY_REPLY_SIZE: NumBytes = NumBytes::new(3 << 20);
const CERTIFIED_DATA_MAX_LENGTH: u32 = 32;

// Enables tracing of system calls for local debugging.
const TRACE_SYSCALLS: bool = false;

/// This error should be displayed if stable memory is used through the system
/// API when Wasm-native stable memory is enabled.
const WASM_NATIVE_STABLE_MEMORY_ERROR: &str = "Stable memory cannot be accessed through the System API when Wasm-native stable memory is enabled.";

const MAX_32_BIT_STABLE_MEMORY_IN_PAGES: u64 = 64 * 1024; // 4GiB

// This macro is used in system calls for tracing.
macro_rules! trace_syscall {
    ($self:ident, $name:ident, $result:expr $( , $args:expr )*) => {{
        if TRACE_SYSCALLS {
            // Output to both logger and stderr to simplify debugging.
            error!(
                $self.log,
                "[system-api][{}] {}: {:?} => {:?}",
                $self.sandbox_safe_system_state.canister_id,
                stringify!($name),
                ($(&$args, )*),
                &$result
            );
            eprintln!(
                "[system-api][{}] {}: {:?} => {:?}",
                $self.sandbox_safe_system_state.canister_id,
                stringify!($name),
                ($(&$args, )*),
                &$result
            );
        }
    }}
}

// This helper is used in system calls for displaying a summary hash of a heap region.
#[inline]
fn summarize(heap: &[u8], start: u32, size: u32) -> u64 {
    if TRACE_SYSCALLS {
        let start = (start as usize).min(heap.len());
        let end = (start + (size as usize)).min(heap.len());
        // The actual hash function doesn't matter much as long as it is
        // cheap to compute and maps the input to u64 reasonably well.
        let mut sum = 0;
        for (i, byte) in heap[start..end].iter().enumerate() {
            sum += (i + 1) as u64 * *byte as u64
        }
        sum
    } else {
        0
    }
}

/// Keeps the message instruction limit and the maximum slice instruction limit.
/// Supports operations to reduce the message limit while keeping the maximum
/// slice limit the same, which is useful for messages that have multiple
/// execution steps such as install, upgrade, and response.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InstructionLimits {
    /// The total instruction limit for message execution. With deterministic
    /// time slicing this limit may exceed the per-round instruction limit.  The
    /// message fails with an `InstructionLimitExceeded` error if it executes
    /// more instructions than this limit.
    message: NumInstructions,

    /// The number of instructions in the largest possible slice. It may
    /// exceed `self.message()` if the latter was reduced or updated by the
    /// previous executions.
    max_slice: NumInstructions,
}

impl InstructionLimits {
    /// Returns the message and slice instruction limits based on the
    /// deterministic time slicing flag.
    pub fn new(dts: FlagStatus, message: NumInstructions, max_slice: NumInstructions) -> Self {
        Self {
            message,
            max_slice: match dts {
                FlagStatus::Enabled => max_slice,
                FlagStatus::Disabled => message,
            },
        }
    }

    /// See the comments of the corresponding field.
    pub fn message(&self) -> NumInstructions {
        self.message
    }

    /// Returns the effective slice size, which is the smallest of
    /// `self.max_slice` and `self.message`.
    pub fn slice(&self) -> NumInstructions {
        self.max_slice.min(self.message)
    }

    /// Reduces the message instruction limit by the given number.
    /// Note that with DTS, the slice size is constant for a fixed message type.
    pub fn reduce_by(&mut self, used: NumInstructions) {
        self.message = NumInstructions::from(self.message.get().saturating_sub(used.get()));
    }

    /// Sets the message instruction limit to the given number.
    /// Note that with DTS, the slice size is constant for a fixed message type.
    pub fn update(&mut self, left: NumInstructions) {
        self.message = left;
    }
}

// Canister and subnet configuration parameters required for execution.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecutionParameters {
    pub instruction_limits: InstructionLimits,
    pub canister_memory_limit: NumBytes,
    pub compute_allocation: ComputeAllocation,
    pub subnet_type: SubnetType,
    pub execution_mode: ExecutionMode,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub enum ResponseStatus {
    // Indicates that the current call context was never replied.
    NotRepliedYet,
    // Indicates that the current call context was replied in one of other
    // executions belonging to this call context (other callbacks, e.g.).
    AlreadyReplied,
    // Contains the response assigned during the current execution.
    JustRepliedWith(Option<WasmResult>),
}

/// This enum indicates whether execution of a non-replicated query
/// should keep track of the state or not. The distinction is necessary
/// because some non-replicated queries can call other queries. In such
/// a case the caller has too keep the state until the callee returns.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum NonReplicatedQueryKind {
    Stateful {
        call_context_id: CallContextId,
        /// Optional outgoing request under construction. If `None` no outgoing
        /// request is currently under construction.
        outgoing_request: Option<RequestInPrep>,
    },
    Pure,
}

/// This enum indicates whether state modifications are important for
/// an API type or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModificationTracking {
    Ignore,
    Track,
}

/// Describes the context within which a canister message is executed.
///
/// The `Arc` values in this type are safe to serialize because the contain
/// read-only data that is only shared for cheap cloning. Serializing and
/// deserializing will result in duplication of the data, but no issues in
/// correctness.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Serialize, Deserialize)]
pub enum ApiType {
    /// For executing the `canister_start` method
    Start,

    /// For executing the `canister_init` method
    Init {
        time: Time,
        #[serde(with = "serde_bytes")]
        incoming_payload: Vec<u8>,
        caller: PrincipalId,
    },

    /// For executing canister methods marked as `update`
    Update {
        time: Time,
        #[serde(with = "serde_bytes")]
        incoming_payload: Vec<u8>,
        incoming_cycles: Cycles,
        caller: PrincipalId,
        call_context_id: CallContextId,
        /// Begins as empty and used to accumulate data for sending replies.
        #[serde(with = "serde_bytes")]
        response_data: Vec<u8>,
        response_status: ResponseStatus,
        /// Optional outgoing request under construction. If `None` no outgoing
        /// request is currently under construction.
        outgoing_request: Option<RequestInPrep>,
        max_reply_size: NumBytes,
    },

    // For executing canister methods marked as `query`
    ReplicatedQuery {
        time: Time,
        #[serde(with = "serde_bytes")]
        incoming_payload: Vec<u8>,
        caller: PrincipalId,
        #[serde(with = "serde_bytes")]
        response_data: Vec<u8>,
        response_status: ResponseStatus,
        data_certificate: Option<Vec<u8>>,
        max_reply_size: NumBytes,
    },

    NonReplicatedQuery {
        time: Time,
        caller: PrincipalId,
        own_subnet_id: SubnetId,
        #[serde(with = "serde_bytes")]
        incoming_payload: Vec<u8>,
        data_certificate: Option<Vec<u8>>,
        // Begins as empty and used to accumulate data for sending replies.
        #[serde(with = "serde_bytes")]
        response_data: Vec<u8>,
        response_status: ResponseStatus,
        max_reply_size: NumBytes,
        query_kind: NonReplicatedQueryKind,
    },

    // For executing closures when a `Reply` is received
    ReplyCallback {
        time: Time,
        #[serde(with = "serde_bytes")]
        incoming_payload: Vec<u8>,
        incoming_cycles: Cycles,
        call_context_id: CallContextId,
        // Begins as empty and used to accumulate data for sending replies.
        #[serde(with = "serde_bytes")]
        response_data: Vec<u8>,
        response_status: ResponseStatus,
        /// Optional outgoing request under construction. If `None` no outgoing
        /// request is currently under construction.
        outgoing_request: Option<RequestInPrep>,
        max_reply_size: NumBytes,
        execution_mode: ExecutionMode,
    },

    // For executing closures when a `Reject` is received
    RejectCallback {
        time: Time,
        reject_context: RejectContext,
        incoming_cycles: Cycles,
        call_context_id: CallContextId,
        // Begins as empty and used to accumulate data for sending replies.
        #[serde(with = "serde_bytes")]
        response_data: Vec<u8>,
        response_status: ResponseStatus,
        /// Optional outgoing request under construction. If `None` no outgoing
        /// request is currently under construction.
        outgoing_request: Option<RequestInPrep>,
        max_reply_size: NumBytes,
        execution_mode: ExecutionMode,
    },

    PreUpgrade {
        caller: PrincipalId,
        time: Time,
    },

    /// For executing canister_inspect_message method that allows the canister
    /// to decide pre-consensus if it actually wants to accept the message or
    /// not.
    InspectMessage {
        caller: PrincipalId,
        method_name: String,
        #[serde(with = "serde_bytes")]
        incoming_payload: Vec<u8>,
        time: Time,
        message_accepted: bool,
    },

    // For executing the `canister_heartbeat` or `canister_global_timer` methods
    SystemTask {
        /// System task to execute.
        /// Only `canister_heartbeat` and `canister_global_timer` are allowed.
        system_task: SystemMethod,
        time: Time,
        call_context_id: CallContextId,
        /// Optional outgoing request under construction. If `None` no outgoing
        /// request is currently under construction.
        outgoing_request: Option<RequestInPrep>,
    },

    /// For executing the `call_on_cleanup` callback.
    ///
    /// The `call_on_cleanup` callback is executed iff the `reply` or the
    /// `reject` callback was executed and trapped (for any reason).
    ///
    /// See https://sdk.dfinity.org/docs/interface-spec/index.html#system-api-call
    Cleanup {
        time: Time,
    },
}

impl ApiType {
    pub fn start() -> Self {
        Self::Start {}
    }

    pub fn init(time: Time, incoming_payload: Vec<u8>, caller: PrincipalId) -> Self {
        Self::Init {
            time,
            incoming_payload,
            caller,
        }
    }

    pub fn system_task(
        system_task: SystemMethod,
        time: Time,
        call_context_id: CallContextId,
    ) -> Self {
        Self::SystemTask {
            time,
            call_context_id,
            outgoing_request: None,
            system_task,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update(
        time: Time,
        incoming_payload: Vec<u8>,
        incoming_cycles: Cycles,
        caller: PrincipalId,
        call_context_id: CallContextId,
    ) -> Self {
        Self::Update {
            time,
            incoming_payload,
            incoming_cycles,
            caller,
            call_context_id,
            response_data: vec![],
            response_status: ResponseStatus::NotRepliedYet,
            outgoing_request: None,
            max_reply_size: MAX_INTER_CANISTER_PAYLOAD_IN_BYTES,
        }
    }

    pub fn replicated_query(
        time: Time,
        incoming_payload: Vec<u8>,
        caller: PrincipalId,
        data_certificate: Option<Vec<u8>>,
    ) -> Self {
        Self::ReplicatedQuery {
            time,
            incoming_payload,
            caller,
            response_data: vec![],
            response_status: ResponseStatus::NotRepliedYet,
            data_certificate,
            max_reply_size: MAX_INTER_CANISTER_PAYLOAD_IN_BYTES,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn non_replicated_query(
        time: Time,
        caller: PrincipalId,
        own_subnet_id: SubnetId,
        incoming_payload: Vec<u8>,
        data_certificate: Option<Vec<u8>>,
        query_kind: NonReplicatedQueryKind,
    ) -> Self {
        Self::NonReplicatedQuery {
            time,
            caller,
            own_subnet_id,
            incoming_payload,
            data_certificate,
            response_data: vec![],
            response_status: ResponseStatus::NotRepliedYet,
            max_reply_size: MAX_NON_REPLICATED_QUERY_REPLY_SIZE,
            query_kind,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn reply_callback(
        time: Time,
        incoming_payload: Vec<u8>,
        incoming_cycles: Cycles,
        call_context_id: CallContextId,
        replied: bool,
        execution_mode: ExecutionMode,
    ) -> Self {
        Self::ReplyCallback {
            time,
            incoming_payload,
            incoming_cycles,
            call_context_id,
            response_data: vec![],
            response_status: if replied {
                ResponseStatus::AlreadyReplied
            } else {
                ResponseStatus::NotRepliedYet
            },
            outgoing_request: None,
            max_reply_size: MAX_INTER_CANISTER_PAYLOAD_IN_BYTES,
            execution_mode,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn reject_callback(
        time: Time,
        reject_context: RejectContext,
        incoming_cycles: Cycles,
        call_context_id: CallContextId,
        replied: bool,
        execution_mode: ExecutionMode,
    ) -> Self {
        Self::RejectCallback {
            time,
            reject_context,
            incoming_cycles,
            call_context_id,
            response_data: vec![],
            response_status: if replied {
                ResponseStatus::AlreadyReplied
            } else {
                ResponseStatus::NotRepliedYet
            },
            outgoing_request: None,
            max_reply_size: MAX_INTER_CANISTER_PAYLOAD_IN_BYTES,
            execution_mode,
        }
    }

    pub fn pre_upgrade(time: Time, caller: PrincipalId) -> Self {
        Self::PreUpgrade { time, caller }
    }

    pub fn inspect_message(
        caller: PrincipalId,
        method_name: String,
        incoming_payload: Vec<u8>,
        time: Time,
    ) -> Self {
        Self::InspectMessage {
            caller,
            method_name,
            incoming_payload,
            time,
            message_accepted: false,
        }
    }

    /// Indicates whether state modifications are important for this API type or
    /// not.
    pub fn modification_tracking(&self) -> ModificationTracking {
        match self {
            ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery {
                query_kind: NonReplicatedQueryKind::Pure,
                ..
            }
            | ApiType::InspectMessage { .. } => ModificationTracking::Ignore,
            ApiType::NonReplicatedQuery {
                query_kind: NonReplicatedQueryKind::Stateful { .. },
                ..
            }
            | ApiType::Start
            | ApiType::Init { .. }
            | ApiType::Update { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Cleanup { .. } => ModificationTracking::Track,
        }
    }

    /// Returns a string slice representation of the enum variant name for use
    /// e.g. as a metric label.
    pub fn as_str(&self) -> &'static str {
        match self {
            ApiType::Start { .. } => "start",
            ApiType::Init { .. } => "init",
            ApiType::SystemTask { system_task, .. } => match system_task {
                SystemMethod::CanisterHeartbeat => "heartbeat",
                SystemMethod::CanisterGlobalTimer => "global timer",
                _ => panic!("Only `canister_heartbeat` and `canister_global_timer` are allowed."),
            },
            ApiType::Update { .. } => "update",
            ApiType::ReplicatedQuery { .. } => "replicated query",
            ApiType::NonReplicatedQuery { .. } => "non replicated query",
            ApiType::ReplyCallback { execution_mode, .. } => match execution_mode {
                ExecutionMode::Replicated => "replicated reply callback",
                ExecutionMode::NonReplicated => "non-replicated reply callback",
            },
            ApiType::RejectCallback { execution_mode, .. } => match execution_mode {
                ExecutionMode::Replicated => "replicated reject callback",
                ExecutionMode::NonReplicated => "non-replicated reject callback",
            },
            ApiType::PreUpgrade { .. } => "pre upgrade",
            ApiType::InspectMessage { .. } => "inspect message",
            ApiType::Cleanup { .. } => "cleanup",
        }
    }
}

// This type is potentially serialized and exposed to the external world.  We
// use custom formatting to avoid exposing its internal details.
impl std::fmt::Display for ApiType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A struct to gather the relevant fields that correspond to a canister's
/// memory consumption.
struct MemoryUsage {
    /// Upper limit on how much the memory the canister could use.
    limit: NumBytes,

    /// The current amount of memory that the canister is using.
    current_usage: NumBytes,

    // This is the amount of memory that the subnet has available. Any
    // expansions in the canister's memory need to be deducted from here.
    subnet_available_memory: SubnetAvailableMemory,

    /// Total memory allocated during this message execution. Note that
    /// `total_allocated_memory` should always be `>= allocated_message_memory`.
    total_allocated_memory: NumBytes,

    /// Message memory allocated during this message execution.
    allocated_message_memory: NumBytes,

    log: ReplicaLogger,
}

impl MemoryUsage {
    fn new(
        log: ReplicaLogger,
        canister_id: CanisterId,
        limit: NumBytes,
        current_usage: NumBytes,
        subnet_available_memory: SubnetAvailableMemory,
    ) -> Self {
        // A canister's current usage should never exceed its limit. This is
        // most probably a bug. Panicking here due to this inconsistency has the
        // danger of putting the entire subnet in a crash loop. Log an error
        // message to page the on-call team and try to stumble along.
        if current_usage > limit {
            error!(
                log,
                "[EXC-BUG] Canister {}: current_usage {} > limit {}",
                canister_id,
                current_usage,
                limit
            );
        }
        Self {
            limit,
            current_usage,
            subnet_available_memory,
            total_allocated_memory: NumBytes::from(0),
            allocated_message_memory: NumBytes::from(0),
            log,
        }
    }

    /// Tries to allocate the requested number of Wasm pages.
    ///
    /// Returns `Err(HypervisorError::OutOfMemory)` and leaves `self` unchanged
    /// if either the canister memory limit or the subnet memory limit would be
    /// exceeded.
    fn allocate_pages(&mut self, pages: usize) -> HypervisorResult<()> {
        let bytes = ic_replicated_state::num_bytes_try_from(NumWasmPages::from(pages))
            .map_err(|_| HypervisorError::OutOfMemory)?;
        self.allocate_memory(bytes, NumBytes::from(0))
    }

    /// Unconditionally deallocates the given number of Wasm pages. Should only
    /// be called immediately after `allocate_pages()`, with the same number of
    /// pages, in case growing the heap failed.
    fn deallocate_pages(&mut self, pages: usize) {
        // Expected to work as we have converted `pages` to bytes when `increase_usage`
        // was called and if it would have failed, we wouldn't call `decrease_usage`.
        let bytes = ic_replicated_state::num_bytes_try_from(NumWasmPages::from(pages))
            .expect("could not convert wasm pages to bytes");
        self.deallocate_memory(bytes, NumBytes::from(0))
    }

    /// Validates that `total_bytes >= message_bytes` holds. In debug build it
    /// will panic in case this condition does not hold. In release builds an
    /// error will be logged. This is because panicking due to this inconsistency
    /// has the potential to put the entire subnet in a crash loop.
    fn validate_requested_memory(&self, total_bytes: NumBytes, message_bytes: NumBytes) {
        debug_assert!(total_bytes >= message_bytes);
        if total_bytes < message_bytes {
            error!(
                self.log,
                "[EXC-BUG] Called `allocate_memory`: with total_bytes {} < message_bytes {}",
                total_bytes,
                message_bytes
            );
        }
    }

    /// Tries to allocate the requested amount of memory (in bytes). `total_bytes`
    /// refers to the total number of requested bytes and `message_bytes` refers to
    /// the required bytes for messages, meaning that this method needs to be called
    /// with `total_bytes >= message_bytes`.
    ///
    /// Returns `Err(HypervisorError::OutOfMemory)` and leaves `self` unchanged
    /// if either the canister memory limit, the subnet memory limit, or the
    /// message memory limit would be exceeded.
    fn allocate_memory(
        &mut self,
        total_bytes: NumBytes,
        message_bytes: NumBytes,
    ) -> HypervisorResult<()> {
        self.validate_requested_memory(total_bytes, message_bytes);

        let (new_usage, overflow) = self.current_usage.get().overflowing_add(total_bytes.get());
        if overflow || new_usage > self.limit.get() {
            return Err(HypervisorError::OutOfMemory);
        }
        match self
            .subnet_available_memory
            .try_decrement(total_bytes, message_bytes)
        {
            Ok(()) => {
                self.current_usage = NumBytes::from(new_usage);
                self.total_allocated_memory += total_bytes;
                self.allocated_message_memory += message_bytes;
                debug_assert!(self.total_allocated_memory >= self.allocated_message_memory);
                Ok(())
            }
            Err(_err) => Err(HypervisorError::OutOfMemory),
        }
    }

    /// Unconditionally deallocates the given number of bytes and message bytes. Should
    /// only be called immediately after `allocate_memory()`, with the same number of bytes,
    /// in case growing the heap failed or upon clean up.
    fn deallocate_memory(&mut self, total_bytes: NumBytes, message_bytes: NumBytes) {
        self.validate_requested_memory(total_bytes, message_bytes);

        self.subnet_available_memory
            .increment(total_bytes, message_bytes);

        debug_assert!(self.current_usage >= total_bytes);
        debug_assert!(self.total_allocated_memory >= total_bytes);
        debug_assert!(self.allocated_message_memory >= message_bytes);
        self.current_usage -= total_bytes;
        self.total_allocated_memory -= total_bytes;
        self.allocated_message_memory -= message_bytes;

        debug_assert!(self.total_allocated_memory >= self.allocated_message_memory);
    }
}

/// Struct that implements the SystemApi trait. This trait enables a canister to
/// have mediated access to its system state.
pub struct SystemApiImpl {
    /// An execution error of the current message.
    execution_error: Option<HypervisorError>,

    log: ReplicaLogger,

    /// The variant of ApiType being executed.
    api_type: ApiType,

    memory_usage: MemoryUsage,

    execution_parameters: ExecutionParameters,

    /// When `stable_memory` is absent, we are using Wasm-native stable memory
    /// which means all stable memory operations should be handled within the
    /// wasm itself and any use of stable memory through the system API should
    /// crash.
    stable_memory: Option<StableMemory>,

    /// System state information that is cached so that we don't need to go
    /// through the `SystemStateAccessor` to read it. This saves on IPC
    /// communication between the sandboxed canister process and the main
    /// replica process.
    sandbox_safe_system_state: SandboxSafeSystemState,

    /// A handler that is invoked when the instruction counter becomes negative
    /// (exceeds the current slice instruction limit).
    out_of_instructions_handler: Arc<dyn OutOfInstructionsHandler>,

    /// The instruction limit of the currently executing slice. It is
    /// initialized to `execution_parameters.instruction_limits.slice()` and
    /// updated after each out-of-instructions call that starts a new slice.
    current_slice_instruction_limit: i64,

    /// The total number of instructions executed before the current slice. It
    /// is initialized to 0 and updated after each out-of-instructions call that
    /// starts a new slice.
    instructions_executed_before_current_slice: i64,

    /// Tracks the complexity accumulated during the message execution.
    execution_complexity: ExecutionComplexity,
}

impl SystemApiImpl {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        api_type: ApiType,
        sandbox_safe_system_state: SandboxSafeSystemState,
        canister_current_memory_usage: NumBytes,
        execution_parameters: ExecutionParameters,
        subnet_available_memory: SubnetAvailableMemory,
        stable_memory: Option<Memory>,
        out_of_instructions_handler: Arc<dyn OutOfInstructionsHandler>,
        log: ReplicaLogger,
    ) -> Self {
        let memory_usage = MemoryUsage::new(
            log.clone(),
            sandbox_safe_system_state.canister_id,
            execution_parameters.canister_memory_limit,
            canister_current_memory_usage,
            subnet_available_memory,
        );
        let stable_memory = stable_memory.map(|m| StableMemory::new(m));
        let slice_limit = execution_parameters.instruction_limits.slice().get();
        Self {
            execution_error: None,
            api_type,
            memory_usage,
            execution_parameters,
            stable_memory,
            sandbox_safe_system_state,
            out_of_instructions_handler,
            log,
            current_slice_instruction_limit: i64::try_from(slice_limit).unwrap_or(i64::MAX),
            instructions_executed_before_current_slice: 0,
            execution_complexity: ExecutionComplexity::default(),
        }
    }

    /// Gets the result of execution, assuming there is no error from
    /// running the canister. Returns any cycles used for an outgoing request
    /// that doesn't get sent and returns allocated memory to the subnet if the
    /// there is an error from running the canister.
    pub fn take_execution_result(
        &mut self,
        wasm_run_error: Option<&HypervisorError>,
    ) -> HypervisorResult<Option<WasmResult>> {
        match &mut self.api_type {
            ApiType::Start { .. }
            | ApiType::Init { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::InspectMessage { .. }
            | ApiType::NonReplicatedQuery { .. } => (),
            ApiType::SystemTask {
                outgoing_request, ..
            }
            | ApiType::Update {
                outgoing_request, ..
            }
            | ApiType::ReplyCallback {
                outgoing_request, ..
            }
            | ApiType::RejectCallback {
                outgoing_request, ..
            } => {
                if let Some(outgoing_request) = outgoing_request.take() {
                    self.sandbox_safe_system_state
                        .refund_cycles(outgoing_request.take_cycles());
                }
            }
        }
        if let Some(err) = wasm_run_error
            .cloned()
            .or_else(|| self.execution_error.take())
        {
            // Return allocated memory in case of failed message execution.
            self.memory_usage.deallocate_memory(
                self.memory_usage.total_allocated_memory,
                self.memory_usage.allocated_message_memory,
            );
            return Err(err);
        }
        match &mut self.api_type {
            ApiType::Start { .. }
            | ApiType::Init { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::Cleanup { .. }
            | ApiType::SystemTask { .. } => Ok(None),
            ApiType::InspectMessage {
                message_accepted, ..
            } => {
                if *message_accepted {
                    Ok(None)
                } else {
                    Err(HypervisorError::MessageRejected)
                }
            }
            ApiType::Update {
                response_status, ..
            }
            | ApiType::ReplicatedQuery {
                response_status, ..
            }
            | ApiType::NonReplicatedQuery {
                response_status, ..
            }
            | ApiType::ReplyCallback {
                response_status, ..
            }
            | ApiType::RejectCallback {
                response_status, ..
            } => match response_status {
                ResponseStatus::JustRepliedWith(ref mut result) => Ok(result.take()),
                _ => Ok(None),
            },
        }
    }

    /// Note that this function is made public only for the tests
    #[doc(hidden)]
    pub fn get_current_memory_usage(&self) -> NumBytes {
        self.memory_usage.current_usage
    }

    /// Bytes allocated in the Wasm/stable memory and messages.
    pub fn get_allocated_bytes(&self) -> NumBytes {
        self.memory_usage.total_allocated_memory
    }

    /// Bytes allocated in messages.
    pub fn get_allocated_message_bytes(&self) -> NumBytes {
        self.memory_usage.allocated_message_memory
    }

    fn error_for(&self, method_name: &str) -> HypervisorError {
        HypervisorError::ContractViolation(format!(
            "\"{}\" cannot be executed in {} mode",
            method_name, self.api_type
        ))
    }

    fn get_msg_caller_id(&self, method_name: &str) -> Result<PrincipalId, HypervisorError> {
        match &self.api_type {
            ApiType::Start { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. } => Err(self.error_for(method_name)),
            ApiType::Init { caller, .. }
            | ApiType::Update { caller, .. }
            | ApiType::ReplicatedQuery { caller, .. }
            | ApiType::PreUpgrade { caller, .. }
            | ApiType::InspectMessage { caller, .. }
            | ApiType::NonReplicatedQuery { caller, .. } => Ok(*caller),
        }
    }

    fn get_response_info(&mut self) -> Option<(&mut Vec<u8>, &NumBytes, &mut ResponseStatus)> {
        match &mut self.api_type {
            ApiType::Start { .. }
            | ApiType::Init { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Cleanup { .. }
            | ApiType::InspectMessage { .. } => None,
            ApiType::Update {
                response_data,
                response_status,
                max_reply_size,
                ..
            }
            | ApiType::ReplicatedQuery {
                response_data,
                response_status,
                max_reply_size,
                ..
            }
            | ApiType::NonReplicatedQuery {
                response_data,
                response_status,
                max_reply_size,
                ..
            }
            | ApiType::ReplyCallback {
                response_data,
                response_status,
                max_reply_size,
                ..
            }
            | ApiType::RejectCallback {
                response_data,
                response_status,
                max_reply_size,
                ..
            } => Some((response_data, max_reply_size, response_status)),
        }
    }

    fn get_reject_code(&self) -> Option<i32> {
        match &self.api_type {
            ApiType::Start { .. }
            | ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::Update { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::InspectMessage { .. } => None,
            ApiType::ReplyCallback { .. } => Some(0),
            ApiType::RejectCallback { reject_context, .. } => Some(reject_context.code as i32),
        }
    }

    fn get_reject_context(&self) -> Option<&RejectContext> {
        match &self.api_type {
            ApiType::Start { .. }
            | ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::Update { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::InspectMessage { .. } => None,
            ApiType::RejectCallback { reject_context, .. } => Some(reject_context),
        }
    }

    fn ic0_call_cycles_add_helper(
        &mut self,
        method_name: &str,
        amount: Cycles,
    ) -> HypervisorResult<()> {
        match &mut self.api_type {
            ApiType::Start { .. }
            | ApiType::Init { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::Cleanup { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::InspectMessage { .. } => Err(self.error_for(method_name)),
            ApiType::Update {
                outgoing_request, ..
            }
            | ApiType::SystemTask {
                outgoing_request, ..
            }
            | ApiType::ReplyCallback {
                outgoing_request, ..
            }
            | ApiType::RejectCallback {
                outgoing_request, ..
            } => match outgoing_request {
                None => Err(HypervisorError::ContractViolation(format!(
                    "{} called when no call is under construction.",
                    method_name
                ))),
                Some(request) => {
                    self.sandbox_safe_system_state
                        .withdraw_cycles_for_transfer(
                            self.memory_usage.current_usage,
                            self.execution_parameters.compute_allocation,
                            amount,
                        )?;
                    request.add_cycles(amount);
                    Ok(())
                }
            },
        }
    }

    fn ic0_canister_cycle_balance_helper(&self, method_name: &str) -> HypervisorResult<Cycles> {
        match &self.api_type {
            ApiType::Start {} => Err(self.error_for(method_name)),
            ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Update { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::InspectMessage { .. } => {
                let res = self.sandbox_safe_system_state.cycles_balance();
                Ok(res)
            }
        }
    }

    fn ic0_msg_cycles_available_helper(&self, method_name: &str) -> HypervisorResult<Cycles> {
        match &self.api_type {
            ApiType::Start { .. }
            | ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::InspectMessage { .. } => Err(self.error_for(method_name)),
            ApiType::Update {
                call_context_id, ..
            }
            | ApiType::ReplyCallback {
                call_context_id, ..
            }
            | ApiType::RejectCallback {
                call_context_id, ..
            } => Ok(self
                .sandbox_safe_system_state
                .msg_cycles_available(*call_context_id)),
        }
    }

    fn ic0_msg_cycles_refunded_helper(&self, method_name: &str) -> HypervisorResult<Cycles> {
        match &self.api_type {
            ApiType::Start { .. }
            | ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::Update { .. }
            | ApiType::InspectMessage { .. } => Err(self.error_for(method_name)),
            ApiType::ReplyCallback {
                incoming_cycles, ..
            }
            | ApiType::RejectCallback {
                incoming_cycles, ..
            } => Ok(*incoming_cycles),
        }
    }

    fn ic0_msg_cycles_accept_helper(
        &mut self,
        method_name: &str,
        max_amount: Cycles,
    ) -> HypervisorResult<Cycles> {
        match &mut self.api_type {
            ApiType::ReplyCallback {
                response_status: ResponseStatus::AlreadyReplied,
                ..
            }
            | ApiType::RejectCallback {
                response_status: ResponseStatus::AlreadyReplied,
                ..
            } => Ok(Cycles::new(0)),
            ApiType::Start { .. }
            | ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::InspectMessage { .. } => Err(self.error_for(method_name)),
            ApiType::Update {
                call_context_id, ..
            }
            | ApiType::ReplyCallback {
                call_context_id, ..
            }
            | ApiType::RejectCallback {
                call_context_id, ..
            } => Ok(self
                .sandbox_safe_system_state
                .msg_cycles_accept(*call_context_id, max_amount)),
        }
    }

    pub fn into_system_state_changes(self) -> SystemStateChanges {
        self.sandbox_safe_system_state.system_state_changes
    }

    pub fn take_system_state_changes(&mut self) -> SystemStateChanges {
        self.sandbox_safe_system_state.take_changes()
    }

    pub fn stable_memory_size(&self) -> NumWasmPages {
        self.stable_memory
            .as_ref()
            .expect(WASM_NATIVE_STABLE_MEMORY_ERROR)
            .stable_memory_size
    }

    /// Wrapper around `self.sandbox_safe_system_state.push_output_request()` that
    /// tries to allocate memory for the `Request` before pushing it.
    ///
    /// On failure to allocate memory or withdraw cycles; or on queue full;
    /// returns `Ok(RejectCode::SysTransient as i32)`.
    ///
    /// Note that this function is made public only for the tests
    #[doc(hidden)]
    pub fn push_output_request(
        &mut self,
        req: Request,
        prepayment_for_response_execution: Cycles,
        prepayment_for_response_transmission: Cycles,
    ) -> HypervisorResult<i32> {
        let abort = |request: Request, sandbox_safe_system_state: &mut SandboxSafeSystemState| {
            sandbox_safe_system_state.refund_cycles(request.payment);
            sandbox_safe_system_state.unregister_callback(request.sender_reply_callback);
            Ok(RejectCode::SysTransient as i32)
        };

        let reservation_bytes = if self.execution_parameters.subnet_type == SubnetType::System {
            // Effectively disable the memory limit checks on system subnets.
            NumBytes::from(0)
        } else {
            (memory_required_to_push_request(&req) as u64).into()
        };
        if self
            .memory_usage
            .allocate_memory(reservation_bytes, reservation_bytes)
            .is_err()
        {
            return abort(req, &mut self.sandbox_safe_system_state);
        }

        match self.sandbox_safe_system_state.push_output_request(
            self.memory_usage.current_usage,
            self.execution_parameters.compute_allocation,
            req,
            prepayment_for_response_execution,
            prepayment_for_response_transmission,
        ) {
            Ok(()) => Ok(0),
            Err(request) => {
                self.memory_usage
                    .deallocate_memory(reservation_bytes, reservation_bytes);
                abort(request, &mut self.sandbox_safe_system_state)
            }
        }
    }
}

impl SystemApi for SystemApiImpl {
    fn set_execution_complexity(&mut self, complexity: ExecutionComplexity) {
        self.execution_complexity = complexity
    }

    fn execution_complexity(&self) -> &ExecutionComplexity {
        &self.execution_complexity
    }

    fn set_execution_error(&mut self, error: HypervisorError) {
        self.execution_error = Some(error)
    }

    fn get_execution_error(&self) -> Option<&HypervisorError> {
        self.execution_error.as_ref()
    }

    fn get_num_instructions_from_bytes(&self, num_bytes: NumBytes) -> NumInstructions {
        match self.sandbox_safe_system_state.subnet_type {
            SubnetType::System => NumInstructions::from(0),
            SubnetType::VerifiedApplication | SubnetType::Application => {
                NumInstructions::from(num_bytes.get())
            }
        }
    }

    fn stable_memory_dirty_pages(&self) -> Vec<(PageIndex, &PageBytes)> {
        self.stable_memory
            .as_ref()
            .expect(WASM_NATIVE_STABLE_MEMORY_ERROR)
            .stable_memory_buffer
            .dirty_pages()
            .collect()
    }

    fn stable_memory_size(&self) -> usize {
        self.stable_memory
            .as_ref()
            .expect(WASM_NATIVE_STABLE_MEMORY_ERROR)
            .stable_memory_size
            .get()
    }

    fn subnet_type(&self) -> SubnetType {
        self.execution_parameters.subnet_type
    }

    fn message_instruction_limit(&self) -> NumInstructions {
        self.execution_parameters.instruction_limits.message()
    }

    fn message_instructions_executed(&self, instruction_counter: i64) -> NumInstructions {
        let result = (self.instructions_executed_before_current_slice as u64)
            .saturating_add(self.slice_instructions_executed(instruction_counter).get());
        NumInstructions::from(result)
    }

    fn slice_instruction_limit(&self) -> NumInstructions {
        // Note that `self.execution_parameters.instruction_limits.slice()` is
        // the instruction limit of the first slice, not the current one.
        NumInstructions::from(u64::try_from(self.current_slice_instruction_limit).unwrap_or(0))
    }

    fn slice_instructions_executed(&self, instruction_counter: i64) -> NumInstructions {
        let result = self
            .current_slice_instruction_limit
            .saturating_sub(instruction_counter)
            .max(0) as u64;
        NumInstructions::from(result)
    }

    fn ic0_msg_caller_size(&self) -> HypervisorResult<u32> {
        let result = self
            .get_msg_caller_id("ic0_msg_caller_size")
            .map(|caller_id| caller_id.as_slice().len() as u32);
        trace_syscall!(self, ic0_msg_caller_size, result);
        result
    }

    fn ic0_msg_caller_copy(
        &self,
        dst: u32,
        offset: u32,
        size: u32,
        heap: &mut [u8],
    ) -> HypervisorResult<()> {
        let result = match self.get_msg_caller_id("ic0_msg_caller_copy") {
            Ok(caller_id) => {
                let id_bytes = caller_id.as_slice();
                valid_subslice("ic0.msg_caller_copy heap", dst, size, heap)?;
                let slice = valid_subslice("ic0.msg_caller_copy id", offset, size, id_bytes)?;
                let (dst, size) = (dst as usize, size as usize);
                deterministic_copy_from_slice(&mut heap[dst..dst + size], slice);
                Ok(())
            }
            Err(err) => Err(err),
        };
        trace_syscall!(
            self,
            ic0_msg_caller_copy,
            result,
            dst,
            offset,
            size,
            summarize(heap, dst, size)
        );
        result
    }

    fn ic0_msg_arg_data_size(&self) -> HypervisorResult<u32> {
        let result = match &self.api_type {
            ApiType::Start { .. }
            | ApiType::Cleanup { .. }
            | ApiType::SystemTask { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::PreUpgrade { .. } => Err(self.error_for("ic0_msg_arg_data_size")),
            ApiType::Init {
                incoming_payload, ..
            }
            | ApiType::Update {
                incoming_payload, ..
            }
            | ApiType::ReplyCallback {
                incoming_payload, ..
            }
            | ApiType::ReplicatedQuery {
                incoming_payload, ..
            }
            | ApiType::InspectMessage {
                incoming_payload, ..
            }
            | ApiType::NonReplicatedQuery {
                incoming_payload, ..
            } => Ok(incoming_payload.len() as u32),
        };
        trace_syscall!(self, ic0_msg_arg_data_size, result);
        result
    }

    fn ic0_msg_arg_data_copy(
        &self,
        dst: u32,
        offset: u32,
        size: u32,
        heap: &mut [u8],
    ) -> HypervisorResult<()> {
        let result = match &self.api_type {
            ApiType::Start { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Cleanup { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::PreUpgrade { .. } => Err(self.error_for("ic0_msg_arg_data_copy")),
            ApiType::Init {
                incoming_payload, ..
            }
            | ApiType::Update {
                incoming_payload, ..
            }
            | ApiType::ReplyCallback {
                incoming_payload, ..
            }
            | ApiType::ReplicatedQuery {
                incoming_payload, ..
            }
            | ApiType::InspectMessage {
                incoming_payload, ..
            }
            | ApiType::NonReplicatedQuery {
                incoming_payload, ..
            } => {
                valid_subslice("ic0.msg_arg_data_copy heap", dst, size, heap)?;
                let payload_subslice = valid_subslice(
                    "ic0.msg_arg_data_copy payload",
                    offset,
                    size,
                    incoming_payload,
                )?;
                let (dst, size) = (dst as usize, size as usize);
                deterministic_copy_from_slice(&mut heap[dst..dst + size], payload_subslice);
                Ok(())
            }
        };
        trace_syscall!(
            self,
            ic0_msg_arg_data_copy,
            result,
            dst,
            offset,
            size,
            summarize(heap, dst, size)
        );
        result
    }

    fn ic0_msg_method_name_size(&self) -> HypervisorResult<u32> {
        let result = match &self.api_type {
            ApiType::Start { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::Cleanup { .. }
            | ApiType::Update { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::SystemTask { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::Init { .. } => Err(self.error_for("ic0_msg_method_name_size")),
            ApiType::InspectMessage { method_name, .. } => Ok(method_name.len() as u32),
        };
        trace_syscall!(self, ic0_msg_method_name_size, result);
        result
    }

    fn ic0_msg_method_name_copy(
        &self,
        dst: u32,
        offset: u32,
        size: u32,
        heap: &mut [u8],
    ) -> HypervisorResult<()> {
        let result = match &self.api_type {
            ApiType::Start { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::Cleanup { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::Update { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::SystemTask { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::Init { .. } => Err(self.error_for("ic0_msg_method_name_copy")),
            ApiType::InspectMessage { method_name, .. } => {
                valid_subslice("ic0.msg_method_name_copy heap", dst, size, heap)?;
                let payload_subslice = valid_subslice(
                    "ic0.msg_method_name_copy payload",
                    offset,
                    size,
                    method_name.as_bytes(),
                )?;
                let (dst, size) = (dst as usize, size as usize);
                deterministic_copy_from_slice(&mut heap[dst..dst + size], payload_subslice);
                Ok(())
            }
        };
        trace_syscall!(
            self,
            ic0_msg_method_name_copy,
            result,
            dst,
            offset,
            size,
            summarize(heap, dst, size)
        );
        result
    }

    fn ic0_accept_message(&mut self) -> HypervisorResult<()> {
        let result = match &mut self.api_type {
            ApiType::Start { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::Cleanup { .. }
            | ApiType::Update { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::SystemTask { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::Init { .. } => Err(self.error_for("ic0_accept_message")),
            ApiType::InspectMessage {
                message_accepted, ..
            } => {
                if *message_accepted {
                    Err(ContractViolation(
                        "ic0.accept_message: the function was already called.".to_string(),
                    ))
                } else {
                    *message_accepted = true;
                    Ok(())
                }
            }
        };
        trace_syscall!(self, ic0_accept_message, result);
        result
    }

    fn ic0_msg_reply(&mut self) -> HypervisorResult<()> {
        let result = match self.get_response_info() {
            None => Err(self.error_for("ic0_msg_reply")),
            Some((data, _, status)) => match status {
                ResponseStatus::NotRepliedYet => {
                    *status = ResponseStatus::JustRepliedWith(Some(WasmResult::Reply(
                        std::mem::take(data),
                    )));
                    Ok(())
                }
                ResponseStatus::AlreadyReplied | ResponseStatus::JustRepliedWith(_) => Err(
                    ContractViolation("ic0.msg_reply: the call is already replied".to_string()),
                ),
            },
        };
        trace_syscall!(self, ic0_msg_reply, result);
        result
    }

    fn ic0_msg_reply_data_append(
        &mut self,
        src: u32,
        size: u32,
        heap: &[u8],
    ) -> HypervisorResult<()> {
        let result = match self.get_response_info() {
            None => Err(self.error_for("ic0_msg_reply_data_append")),
            Some((data, max_reply_size, response_status)) => match response_status {
                ResponseStatus::NotRepliedYet => {
                    let payload_size = (data.len() + size as usize) as u64;
                    if payload_size > max_reply_size.get() {
                        let string = format!(
                            "ic0.msg_reply_data_append: application payload size ({}) cannot be larger than {}",
                            payload_size,
                            max_reply_size,
                        );
                        return Err(ContractViolation(string));
                    }
                    data.extend_from_slice(valid_subslice("msg.reply", src, size, heap)?);
                    Ok(())
                }
                ResponseStatus::AlreadyReplied | ResponseStatus::JustRepliedWith(_) => {
                    Err(ContractViolation(
                        "ic0.msg_reply_data_append: the call is already replied".to_string(),
                    ))
                }
            },
        };
        trace_syscall!(
            self,
            ic0_msg_reply_data_append,
            result,
            src,
            size,
            summarize(heap, src, size)
        );
        result
    }

    fn ic0_msg_reject(&mut self, src: u32, size: u32, heap: &[u8]) -> HypervisorResult<()> {
        let result = match self.get_response_info() {
            None => Err(self.error_for("ic0_msg_reject")),
            Some((_, max_reply_size, response_status)) => match response_status {
                ResponseStatus::NotRepliedYet => {
                    if size as u64 > max_reply_size.get() {
                        let string = format!(
                        "ic0.msg_reject: application payload size ({}) cannot be larger than {}",
                        size, max_reply_size
                    );
                        return Err(ContractViolation(string));
                    }
                    let msg_bytes = valid_subslice("ic0.msg_reject", src, size, heap)?;
                    let msg = String::from_utf8(msg_bytes.to_vec()).map_err(|_| {
                        ContractViolation(
                            "ic0.msg_reject: invalid UTF-8 string provided".to_string(),
                        )
                    })?;
                    *response_status =
                        ResponseStatus::JustRepliedWith(Some(WasmResult::Reject(msg)));
                    Ok(())
                }
                ResponseStatus::AlreadyReplied | ResponseStatus::JustRepliedWith(_) => Err(
                    ContractViolation("ic0.msg_reject: the call is already replied".to_string()),
                ),
            },
        };
        trace_syscall!(
            self,
            ic0_msg_reject,
            result,
            src,
            size,
            summarize(heap, src, size)
        );
        result
    }

    fn ic0_msg_reject_code(&self) -> HypervisorResult<i32> {
        let result = self
            .get_reject_code()
            .ok_or_else(|| self.error_for("ic0_msg_reject_code"));
        trace_syscall!(self, ic0_msg_reject_code, result);
        result
    }

    fn ic0_msg_reject_msg_size(&self) -> HypervisorResult<u32> {
        let reject_context = self
            .get_reject_context()
            .ok_or_else(|| self.error_for("ic0_msg_reject_msg_size"))?;
        let result = Ok(reject_context.message().len() as u32);
        trace_syscall!(self, ic0_msg_reject_msg_size, result);
        result
    }

    fn ic0_msg_reject_msg_copy(
        &self,
        dst: u32,
        offset: u32,
        size: u32,
        heap: &mut [u8],
    ) -> HypervisorResult<()> {
        let result = {
            let reject_context = self
                .get_reject_context()
                .ok_or_else(|| self.error_for("ic0_msg_reject_msg_copy"))?;
            valid_subslice("ic0.msg_reject_msg_copy heap", dst, size, heap)?;

            let msg = reject_context.message();
            let dst = dst as usize;
            let msg_bytes =
                valid_subslice("ic0.msg_reject_msg_copy msg", offset, size, msg.as_bytes())?;
            let size = size as usize;
            deterministic_copy_from_slice(&mut heap[dst..dst + size], msg_bytes);
            Ok(())
        };
        trace_syscall!(
            self,
            ic0_msg_reject_msg_copy,
            result,
            dst,
            offset,
            size,
            summarize(heap, dst, size)
        );
        result
    }

    fn ic0_canister_self_size(&self) -> HypervisorResult<usize> {
        let result = match &self.api_type {
            ApiType::Start { .. } => Err(self.error_for("ic0_canister_self_size")),
            ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Cleanup { .. }
            | ApiType::Update { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::InspectMessage { .. } => Ok(self
                .sandbox_safe_system_state
                .canister_id
                .get_ref()
                .as_slice()
                .len()),
        };
        trace_syscall!(self, ic0_canister_self_size, result);
        result
    }

    fn ic0_canister_self_copy(
        &mut self,
        dst: u32,
        offset: u32,
        size: u32,
        heap: &mut [u8],
    ) -> HypervisorResult<()> {
        let result = match &self.api_type {
            ApiType::Start { .. } => Err(self.error_for("ic0_canister_self_copy")),
            ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Cleanup { .. }
            | ApiType::Update { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::InspectMessage { .. } => {
                valid_subslice("ic0.canister_self_copy heap", dst, size, heap)?;
                let canister_id = self.sandbox_safe_system_state.canister_id;
                let id_bytes = canister_id.get_ref().as_slice();
                let slice = valid_subslice("ic0.canister_self_copy id", offset, size, id_bytes)?;
                let (dst, size) = (dst as usize, size as usize);
                deterministic_copy_from_slice(&mut heap[dst..dst + size], slice);
                Ok(())
            }
        };
        trace_syscall!(
            self,
            ic0_canister_self_copy,
            result,
            dst,
            offset,
            size,
            summarize(heap, dst, size)
        );
        result
    }

    fn ic0_controller_size(&self) -> HypervisorResult<usize> {
        let result = match &self.api_type {
            ApiType::Start {} => Err(self.error_for("ic0_controller_size")),
            ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Update { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::InspectMessage { .. } => {
                Ok(self.sandbox_safe_system_state.controller.as_slice().len())
            }
        };
        trace_syscall!(self, ic0_controller_size, result);
        result
    }

    fn ic0_controller_copy(
        &mut self,
        dst: u32,
        offset: u32,
        size: u32,
        heap: &mut [u8],
    ) -> HypervisorResult<()> {
        let result = match &self.api_type {
            ApiType::Start {} => Err(self.error_for("ic0_controller_copy")),
            ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Update { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::InspectMessage { .. } => {
                valid_subslice("ic0.controller_copy heap", dst, size, heap)?;
                let controller = self.sandbox_safe_system_state.controller;
                let id_bytes = controller.as_slice();
                let slice = valid_subslice("ic0.controller_copy id", offset, size, id_bytes)?;
                let (dst, size) = (dst as usize, size as usize);
                deterministic_copy_from_slice(&mut heap[dst..dst + size], slice);
                Ok(())
            }
        };
        trace_syscall!(self, ic0_controller_copy, result);
        result
    }

    fn ic0_call_new(
        &mut self,
        callee_src: u32,
        callee_size: u32,
        name_src: u32,
        name_len: u32,
        reply_fun: u32,
        reply_env: u32,
        reject_fun: u32,
        reject_env: u32,
        heap: &[u8],
    ) -> HypervisorResult<()> {
        let result = match &mut self.api_type {
            ApiType::Start { .. }
            | ApiType::Init { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery {
                query_kind: NonReplicatedQueryKind::Pure,
                ..
            }
            | ApiType::Cleanup { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::InspectMessage { .. } => Err(self.error_for("ic0_call_new")),
            ApiType::Update {
                outgoing_request, ..
            }
            | ApiType::NonReplicatedQuery {
                query_kind:
                    NonReplicatedQueryKind::Stateful {
                        outgoing_request, ..
                    },
                ..
            }
            | ApiType::SystemTask {
                outgoing_request, ..
            }
            | ApiType::ReplyCallback {
                outgoing_request, ..
            }
            | ApiType::RejectCallback {
                outgoing_request, ..
            } => {
                if let Some(outgoing_request) = outgoing_request.take() {
                    self.sandbox_safe_system_state
                        .refund_cycles(outgoing_request.take_cycles());
                }

                let req = RequestInPrep::new(
                    self.sandbox_safe_system_state.canister_id,
                    callee_src,
                    callee_size,
                    name_src,
                    name_len,
                    heap,
                    WasmClosure::new(reply_fun, reply_env),
                    WasmClosure::new(reject_fun, reject_env),
                    MAX_INTER_CANISTER_PAYLOAD_IN_BYTES,
                    MULTIPLIER_MAX_SIZE_LOCAL_SUBNET,
                )?;
                *outgoing_request = Some(req);
                Ok(())
            }
        };
        trace_syscall!(
            self,
            ic0_call_new,
            result,
            callee_src,
            callee_size,
            name_src,
            name_len,
            reply_fun,
            reply_env,
            reject_fun,
            reject_env,
            summarize(heap, callee_src, callee_size)
        );
        result
    }

    fn ic0_call_data_append(&mut self, src: u32, size: u32, heap: &[u8]) -> HypervisorResult<()> {
        let result = match &mut self.api_type {
            ApiType::Start { .. }
            | ApiType::Init { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery {
                query_kind: NonReplicatedQueryKind::Pure,
                ..
            }
            | ApiType::PreUpgrade { .. }
            | ApiType::Cleanup { .. }
            | ApiType::InspectMessage { .. } => Err(self.error_for("ic0_call_data_append")),
            ApiType::Update {
                outgoing_request, ..
            }
            | ApiType::NonReplicatedQuery {
                query_kind:
                    NonReplicatedQueryKind::Stateful {
                        outgoing_request, ..
                    },
                ..
            }
            | ApiType::SystemTask {
                outgoing_request, ..
            }
            | ApiType::ReplyCallback {
                outgoing_request, ..
            }
            | ApiType::RejectCallback {
                outgoing_request, ..
            } => match outgoing_request {
                None => Err(HypervisorError::ContractViolation(
                    "ic0.call_data_append called when no call is under construction.".to_string(),
                )),
                Some(request) => request.extend_method_payload(src, size, heap),
            },
        };
        trace_syscall!(
            self,
            ic0_call_data_append,
            src,
            size,
            summarize(heap, src, size)
        );
        result
    }

    fn ic0_call_on_cleanup(&mut self, fun: u32, env: u32) -> HypervisorResult<()> {
        let result = match &mut self.api_type {
            ApiType::Start { .. }
            | ApiType::Init { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery {
                query_kind: NonReplicatedQueryKind::Pure,
                ..
            }
            | ApiType::Cleanup { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::InspectMessage { .. } => Err(self.error_for("ic0_call_on_cleanup")),
            ApiType::Update {
                outgoing_request, ..
            }
            | ApiType::NonReplicatedQuery {
                query_kind:
                    NonReplicatedQueryKind::Stateful {
                        outgoing_request, ..
                    },
                ..
            }
            | ApiType::SystemTask {
                outgoing_request, ..
            }
            | ApiType::ReplyCallback {
                outgoing_request, ..
            }
            | ApiType::RejectCallback {
                outgoing_request, ..
            } => match outgoing_request {
                None => Err(HypervisorError::ContractViolation(
                    "ic0.call_on_cleanup called when no call is under construction.".to_string(),
                )),
                Some(request) => request.set_on_cleanup(WasmClosure::new(fun, env)),
            },
        };
        trace_syscall!(self, ic0_call_on_cleanup, fun, env);
        result
    }

    fn ic0_call_cycles_add(&mut self, amount: u64) -> HypervisorResult<()> {
        let result = self.ic0_call_cycles_add_helper("ic0_call_cycles_add", Cycles::from(amount));
        trace_syscall!(self, ic0_call_cycles_add, result, amount);
        result
    }

    fn ic0_call_cycles_add128(&mut self, amount: Cycles) -> HypervisorResult<()> {
        let result = self.ic0_call_cycles_add_helper("ic0_call_cycles_add128", amount);
        trace_syscall!(self, ic0_call_cycles_add128, result, amount);
        result
    }

    // Note that if this function returns an error, then the canister will be
    // trapped and the state will be rolled back. Hence, we do not have to worry
    // about rolling back any modifications that previous calls like
    // ic0_call_cycles_add128() made.
    //
    // However, this call can still "fail" without returning an error. Examples
    // are if the canister does not have sufficient cycles to send the request
    // or the output queues are full. In this case, we need to perform the
    // necessary cleanups.
    fn ic0_call_perform(&mut self) -> HypervisorResult<i32> {
        let result = match &mut self.api_type {
            ApiType::Start { .. }
            | ApiType::Init { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery {
                query_kind: NonReplicatedQueryKind::Pure,
                ..
            }
            | ApiType::Cleanup { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::InspectMessage { .. } => Err(self.error_for("ic0_call_perform")),
            ApiType::Update {
                call_context_id,
                outgoing_request,
                ..
            }
            | ApiType::SystemTask {
                call_context_id,
                outgoing_request,
                ..
            }
            | ApiType::ReplyCallback {
                call_context_id,
                outgoing_request,
                ..
            }
            | ApiType::RejectCallback {
                call_context_id,
                outgoing_request,
                ..
            }
            | ApiType::NonReplicatedQuery {
                query_kind:
                    NonReplicatedQueryKind::Stateful {
                        call_context_id,
                        outgoing_request,
                    },
                ..
            } => {
                let req_in_prep = outgoing_request.take().ok_or_else(|| {
                    ContractViolation(
                        "ic0.call_perform called when no call is under construction.".to_string(),
                    )
                })?;

                let req = into_request(
                    req_in_prep,
                    *call_context_id,
                    &mut self.sandbox_safe_system_state,
                    &self.log,
                )?;

                self.push_output_request(
                    req.request,
                    req.prepayment_for_response_execution,
                    req.prepayment_for_response_transmission,
                )
            }
        };
        trace_syscall!(self, ic0_call_perform, result);
        result
    }

    fn ic0_stable_size(&self) -> HypervisorResult<u32> {
        let result = match &self.api_type {
            ApiType::Start {} if self.stable_memory.is_some() => {
                Err(self.error_for("ic0_stable_size"))
            }
            ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Update { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::InspectMessage { .. }
            | ApiType::Start {} => self
                .stable_memory
                .as_ref()
                .expect(WASM_NATIVE_STABLE_MEMORY_ERROR)
                .stable_size(),
        };
        trace_syscall!(self, ic0_stable_size, result);
        result
    }

    fn ic0_stable_grow(&mut self, additional_pages: u32) -> HypervisorResult<i32> {
        let result = match &self.api_type {
            ApiType::Start {} if self.stable_memory.is_some() => {
                Err(self.error_for("ic0_stable_grow"))
            }
            ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Update { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::InspectMessage { .. }
            | ApiType::Start {} => {
                match self.memory_usage.allocate_pages(additional_pages as usize) {
                    Ok(()) => {
                        let res = self
                            .stable_memory
                            .as_mut()
                            .expect(WASM_NATIVE_STABLE_MEMORY_ERROR)
                            .stable_grow(additional_pages);
                        match &res {
                            Err(_) | Ok(-1) => self
                                .memory_usage
                                .deallocate_pages(additional_pages as usize),
                            _ => {}
                        }
                        res
                    }
                    Err(_err) => Ok(-1),
                }
            }
        };
        trace_syscall!(self, ic0_stable_grow, result, additional_pages);
        result
    }

    fn ic0_stable_read(
        &self,
        dst: u32,
        offset: u32,
        size: u32,
        heap: &mut [u8],
    ) -> HypervisorResult<()> {
        let result = match &self.api_type {
            ApiType::Start {} if self.stable_memory.is_some() => {
                Err(self.error_for("ic0_stable_read"))
            }
            ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Update { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::InspectMessage { .. }
            | ApiType::Start {} => self
                .stable_memory
                .as_ref()
                .expect(WASM_NATIVE_STABLE_MEMORY_ERROR)
                .stable_read(dst, offset, size, heap),
        };
        trace_syscall!(
            self,
            ic0_stable_read,
            result,
            dst,
            offset,
            size,
            summarize(heap, dst, size)
        );
        result
    }

    fn ic0_stable_write(
        &mut self,
        offset: u32,
        src: u32,
        size: u32,
        heap: &[u8],
    ) -> HypervisorResult<()> {
        let result = match &self.api_type {
            ApiType::Start {} if self.stable_memory.is_some() => {
                Err(self.error_for("ic0_stable_write"))
            }
            ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Update { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::InspectMessage { .. }
            | ApiType::Start {} => self
                .stable_memory
                .as_mut()
                .expect(WASM_NATIVE_STABLE_MEMORY_ERROR)
                .stable_write(offset, src, size, heap),
        };
        trace_syscall!(
            self,
            ic0_stable_write,
            result,
            offset,
            src,
            size,
            summarize(heap, src, size)
        );
        result
    }

    fn ic0_stable64_size(&self) -> HypervisorResult<u64> {
        let result = match &self.api_type {
            ApiType::Start {} if self.stable_memory.is_some() => {
                Err(self.error_for("ic0_stable64_size"))
            }
            ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Update { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::InspectMessage { .. }
            | ApiType::Start {} => self
                .stable_memory
                .as_ref()
                .expect(WASM_NATIVE_STABLE_MEMORY_ERROR)
                .stable64_size(),
        };
        trace_syscall!(self, ic0_stable64_size, result);
        result
    }

    fn ic0_stable64_grow(&mut self, additional_pages: u64) -> HypervisorResult<i64> {
        let result = match &self.api_type {
            ApiType::Start {} if self.stable_memory.is_some() => {
                Err(self.error_for("ic0_stable64_grow"))
            }
            ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Update { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::InspectMessage { .. }
            | ApiType::Start {} => {
                match self.memory_usage.allocate_pages(additional_pages as usize) {
                    Ok(()) => {
                        let res = self
                            .stable_memory
                            .as_mut()
                            .expect(WASM_NATIVE_STABLE_MEMORY_ERROR)
                            .stable64_grow(additional_pages);
                        match &res {
                            Err(_) | Ok(-1) => self
                                .memory_usage
                                .deallocate_pages(additional_pages as usize),
                            _ => {}
                        }
                        res
                    }
                    Err(_err) => Ok(-1),
                }
            }
        };
        trace_syscall!(self, ic0_stable64_grow, result, additional_pages);
        result
    }

    fn ic0_stable64_read(
        &self,
        dst: u64,
        offset: u64,
        size: u64,
        heap: &mut [u8],
    ) -> HypervisorResult<()> {
        let result = match &self.api_type {
            ApiType::Start {} if self.stable_memory.is_some() => {
                Err(self.error_for("ic0_stable64_read"))
            }
            ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Update { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::InspectMessage { .. }
            | ApiType::Start {} => self
                .stable_memory
                .as_ref()
                .expect(WASM_NATIVE_STABLE_MEMORY_ERROR)
                .stable64_read(dst, offset, size, heap),
        };
        trace_syscall!(
            self,
            ic0_stable64_read,
            result,
            dst,
            offset,
            size,
            summarize(heap, dst as u32, size as u32)
        );
        result
    }

    fn ic0_stable64_write(
        &mut self,
        offset: u64,
        src: u64,
        size: u64,
        heap: &[u8],
    ) -> HypervisorResult<()> {
        let result = match &self.api_type {
            ApiType::Start {} if self.stable_memory.is_some() => {
                Err(self.error_for("ic0_stable64_write"))
            }
            ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Update { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::InspectMessage { .. }
            | ApiType::Start {} => self
                .stable_memory
                .as_mut()
                .expect(WASM_NATIVE_STABLE_MEMORY_ERROR)
                .stable64_write(offset, src, size, heap),
        };
        trace_syscall!(
            self,
            ic0_stable64_write,
            result,
            offset,
            src,
            size,
            summarize(heap, src as u32, size as u32)
        );
        result
    }

    fn dirty_pages_from_stable_write(
        &self,
        offset: u64,
        size: u64,
    ) -> HypervisorResult<(NumPages, NumInstructions)> {
        let dirty_pages = self
            .stable_memory
            .as_ref()
            .expect(WASM_NATIVE_STABLE_MEMORY_ERROR)
            .dirty_pages_from_write(offset, size);
        let cost = self
            .sandbox_safe_system_state
            .dirty_page_cost(dirty_pages)?;
        Ok((dirty_pages, cost))
    }

    fn ic0_time(&self) -> HypervisorResult<Time> {
        let result = match &self.api_type {
            ApiType::Start { .. } => Err(self.error_for("ic0_time")),
            ApiType::Init { time, .. }
            | ApiType::SystemTask { time, .. }
            | ApiType::Update { time, .. }
            | ApiType::Cleanup { time, .. }
            | ApiType::NonReplicatedQuery { time, .. }
            | ApiType::ReplicatedQuery { time, .. }
            | ApiType::PreUpgrade { time, .. }
            | ApiType::ReplyCallback { time, .. }
            | ApiType::RejectCallback { time, .. }
            | ApiType::InspectMessage { time, .. } => Ok(*time),
        };
        trace_syscall!(self, ic0_time, result);
        result
    }

    fn ic0_global_timer_set(&mut self, time: Time) -> HypervisorResult<Time> {
        let result = match &self.api_type {
            ApiType::Start { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::InspectMessage { .. } => Err(self.error_for("ic0_global_timer_set")),
            ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Update { .. }
            | ApiType::Cleanup { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. } => {
                let prev_time = self.sandbox_safe_system_state.global_timer().to_time();
                self.sandbox_safe_system_state
                    .set_global_timer(CanisterTimer::from_time(time));
                Ok(prev_time)
            }
        };
        trace_syscall!(self, ic0_global_timer_set, result);
        result
    }

    fn ic0_performance_counter(
        &self,
        performance_counter_type: PerformanceCounterType,
    ) -> HypervisorResult<u64> {
        let result = match performance_counter_type {
            PerformanceCounterType::Instructions(instruction_counter) => Ok(self
                .message_instructions_executed(instruction_counter)
                .get()),
        };
        trace_syscall!(self, ic0_performance_counter, result);
        result
    }

    fn ic0_canister_version(&self) -> HypervisorResult<u64> {
        let result = match &self.api_type {
            ApiType::Start { .. } => Err(self.error_for("ic0_canister_version")),
            ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Update { .. }
            | ApiType::Cleanup { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::InspectMessage { .. } => {
                Ok(self.sandbox_safe_system_state.canister_version())
            }
        };
        trace_syscall!(self, ic0_canister_version, result);
        result
    }

    fn out_of_instructions(&mut self, instruction_counter: i64) -> HypervisorResult<i64> {
        let execution_complexity = self.execution_complexity().clone();
        let result = self
            .out_of_instructions_handler
            .out_of_instructions(instruction_counter, execution_complexity);
        if let Ok(new_slice_instruction_limit) = result {
            // A new slice has started, update the instruction sum and limit.
            let slice_instructions = self
                .current_slice_instruction_limit
                .saturating_sub(instruction_counter)
                .max(0);
            self.instructions_executed_before_current_slice += slice_instructions;
            self.current_slice_instruction_limit = new_slice_instruction_limit;
        }
        trace_syscall!(self, out_of_instructions, result, instruction_counter);
        result
    }

    fn update_available_memory(
        &mut self,
        native_memory_grow_res: i64,
        additional_pages: u64,
    ) -> HypervisorResult<()> {
        let result = {
            if native_memory_grow_res == -1 {
                return Ok(());
            }
            match self.memory_usage.allocate_pages(additional_pages as usize) {
                Ok(()) => Ok(()),
                Err(_err) => Err(HypervisorError::OutOfMemory),
            }
        };
        trace_syscall!(
            self,
            update_available_memory,
            result,
            native_memory_grow_res,
            additional_pages
        );
        result
    }

    fn try_grow_stable_memory(
        &mut self,
        current_size: u64,
        additional_pages: u64,
        stable_memory_api: ic_interfaces::execution_environment::StableMemoryApi,
    ) -> HypervisorResult<StableGrowOutcome> {
        if let StableMemoryApi::Stable32 = stable_memory_api {
            if current_size > MAX_32_BIT_STABLE_MEMORY_IN_PAGES {
                return Err(HypervisorError::Trapped(
                    TrapCode::StableMemoryTooBigFor32Bit,
                ));
            }
            if current_size.saturating_add(additional_pages) > MAX_32_BIT_STABLE_MEMORY_IN_PAGES {
                return Ok(StableGrowOutcome::Failure);
            }
        }
        match self.memory_usage.allocate_pages(additional_pages as usize) {
            Ok(()) => Ok(StableGrowOutcome::Success),
            Err(_) => Ok(StableGrowOutcome::Failure),
        }
    }

    fn deallocate_pages(&mut self, additional_pages: u64) {
        self.memory_usage
            .deallocate_pages(additional_pages as usize)
    }

    fn ic0_canister_cycle_balance(&self) -> HypervisorResult<u64> {
        let result = {
            let (high_amount, low_amount) = self
                .ic0_canister_cycle_balance_helper("ic0_canister_cycle_balance")?
                .into_parts();
            if high_amount != 0 {
                return Err(HypervisorError::Trapped(CyclesAmountTooBigFor64Bit));
            }
            Ok(low_amount)
        };
        trace_syscall!(self, ic0_canister_cycle_balance, result);
        result
    }

    fn ic0_canister_cycle_balance128(&self, dst: u32, heap: &mut [u8]) -> HypervisorResult<()> {
        let result = {
            let method_name = "ic0_canister_cycle_balance128";
            let cycles = self.ic0_canister_cycle_balance_helper(method_name)?;
            copy_cycles_to_heap(cycles, dst, heap, method_name)?;
            Ok(())
        };
        trace_syscall!(
            self,
            ic0_canister_cycle_balance128,
            dst,
            summarize(heap, dst, 16)
        );
        result
    }

    fn ic0_msg_cycles_available(&self) -> HypervisorResult<u64> {
        let result = {
            let (high_amount, low_amount) = self
                .ic0_msg_cycles_available_helper("ic0_msg_cycles_available")?
                .into_parts();
            if high_amount != 0 {
                return Err(HypervisorError::Trapped(CyclesAmountTooBigFor64Bit));
            }
            Ok(low_amount)
        };
        trace_syscall!(self, ic0_msg_cycles_available, result);
        result
    }

    fn ic0_msg_cycles_available128(&self, dst: u32, heap: &mut [u8]) -> HypervisorResult<()> {
        let result = {
            let method_name = "ic0_msg_cycles_available128";
            let cycles = self.ic0_msg_cycles_available_helper(method_name)?;
            copy_cycles_to_heap(cycles, dst, heap, method_name)?;
            Ok(())
        };
        trace_syscall!(self, ic0_msg_cycles_available128, result);
        result
    }

    fn ic0_msg_cycles_refunded(&self) -> HypervisorResult<u64> {
        let result = {
            let (high_amount, low_amount) = self
                .ic0_msg_cycles_refunded_helper("ic0_msg_cycles_refunded")?
                .into_parts();
            if high_amount != 0 {
                return Err(HypervisorError::Trapped(CyclesAmountTooBigFor64Bit));
            }
            Ok(low_amount)
        };
        trace_syscall!(self, ic0_msg_cycles_refunded, result);
        result
    }

    fn ic0_msg_cycles_refunded128(&self, dst: u32, heap: &mut [u8]) -> HypervisorResult<()> {
        let result = {
            let method_name = "ic0_msg_cycles_refunded128";
            let cycles = self.ic0_msg_cycles_refunded_helper(method_name)?;
            copy_cycles_to_heap(cycles, dst, heap, method_name)?;
            Ok(())
        };
        trace_syscall!(
            self,
            ic0_msg_cycles_refunded128,
            result,
            summarize(heap, dst, 16)
        );
        result
    }

    fn ic0_msg_cycles_accept(&mut self, max_amount: u64) -> HypervisorResult<u64> {
        let result = {
            // Cannot accept more than max_amount.
            let (high_amount, low_amount) = self
                .ic0_msg_cycles_accept_helper("ic0_msg_cycles_accept", Cycles::from(max_amount))?
                .into_parts();
            if high_amount != 0 {
                error!(
                self.log,
                "ic0_msg_cycles_accept cannot accept more than max_amount {}; accepted amount {}",
                max_amount,
                Cycles::from_parts(high_amount, low_amount).get()
            )
            }
            Ok(low_amount)
        };
        trace_syscall!(self, ic0_msg_cycles_accept, result, max_amount);
        result
    }

    fn ic0_msg_cycles_accept128(
        &mut self,
        max_amount: Cycles,
        dst: u32,
        heap: &mut [u8],
    ) -> HypervisorResult<()> {
        let result = {
            let method_name = "ic0_msg_cycles_accept128";
            let cycles = self.ic0_msg_cycles_accept_helper(method_name, max_amount)?;
            copy_cycles_to_heap(cycles, dst, heap, method_name)?;
            Ok(())
        };
        trace_syscall!(self, ic0_msg_cycles_accept128, result);
        result
    }

    fn ic0_data_certificate_present(&self) -> HypervisorResult<i32> {
        let result = match &self.api_type {
            ApiType::Start { .. } => Err(self.error_for("ic0_data_certificate_present")),
            ApiType::Init { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::Cleanup { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::InspectMessage { .. }
            | ApiType::Update { .. }
            | ApiType::SystemTask { .. } => Ok(0),
            ApiType::ReplicatedQuery {
                data_certificate, ..
            }
            | ApiType::NonReplicatedQuery {
                data_certificate, ..
            } => match data_certificate {
                Some(_) => Ok(1),
                None => Ok(0),
            },
        };
        trace_syscall!(self, ic0_data_certificate_present, result);
        result
    }

    fn ic0_data_certificate_size(&self) -> HypervisorResult<i32> {
        let result = match &self.api_type {
            ApiType::Start { .. }
            | ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Update { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::Cleanup { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::InspectMessage { .. } => Err(self.error_for("ic0_data_certificate_size")),
            ApiType::ReplicatedQuery {
                data_certificate, ..
            }
            | ApiType::NonReplicatedQuery {
                data_certificate, ..
            } => match data_certificate {
                Some(data_certificate) => Ok(data_certificate.len() as i32),
                None => Err(self.error_for("ic0_data_certificate_size")),
            },
        };
        trace_syscall!(self, ic0_data_certificate_size, result);
        result
    }

    fn ic0_data_certificate_copy(
        &self,
        dst: u32,
        offset: u32,
        size: u32,
        heap: &mut [u8],
    ) -> HypervisorResult<()> {
        let result = match &self.api_type {
            ApiType::Start { .. }
            | ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Update { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::Cleanup { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::InspectMessage { .. } => Err(self.error_for("ic0_data_certificate_copy")),
            ApiType::ReplicatedQuery {
                data_certificate, ..
            }
            | ApiType::NonReplicatedQuery {
                data_certificate, ..
            } => match data_certificate {
                Some(data_certificate) => {
                    let (dst, offset, size) = (dst as usize, offset as usize, size as usize);

                    let (upper_bound, overflow) = offset.overflowing_add(size);
                    if overflow || upper_bound > data_certificate.len() {
                        return Err(ContractViolation(format!(
                            "ic0_data_certificate_copy failed because offset + size is out \
                                 of bounds. Found offset = {} and size = {} while offset + size \
                                 must be <= {}",
                            offset,
                            size,
                            data_certificate.len(),
                        )));
                    }

                    let (upper_bound, overflow) = dst.overflowing_add(size);
                    if overflow || upper_bound > heap.len() {
                        return Err(ContractViolation(format!(
                            "ic0_data_certificate_copy failed because dst + size is out \
                                 of bounds. Found dst = {} and size = {} while dst + size \
                                 must be <= {}",
                            dst,
                            size,
                            heap.len(),
                        )));
                    }

                    // Copy the certificate into the canister.
                    deterministic_copy_from_slice(
                        &mut heap[dst..dst + size],
                        &data_certificate[offset..offset + size],
                    );
                    Ok(())
                }
                None => Err(self.error_for("ic0_data_certificate_size")),
            },
        };
        trace_syscall!(
            self,
            ic0_data_certificate_copy,
            dst,
            offset,
            size,
            summarize(heap, dst, size)
        );
        result
    }

    fn ic0_certified_data_set(&mut self, src: u32, size: u32, heap: &[u8]) -> HypervisorResult<()> {
        let result = match &mut self.api_type {
            ApiType::Start { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::Cleanup { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::InspectMessage { .. } => Err(self.error_for("ic0_certified_data_set")),
            ApiType::Init { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Update { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::PreUpgrade { .. } => {
                if size > CERTIFIED_DATA_MAX_LENGTH {
                    return Err(ContractViolation(format!(
                        "ic0_certified_data_set failed because the passed data must be \
                        no larger than 32 bytes. Found {} bytes",
                        size
                    )));
                }

                let (src, size) = (src as usize, size as usize);
                let (upper_bound, overflow) = src.overflowing_add(size);
                if overflow || upper_bound > heap.len() {
                    return Err(ContractViolation(format!(
                        "ic0_certified_data_set failed because src + size is out \
                                 of bounds. Found src = {} and size = {} while src + size \
                                 must be <= {}",
                        src,
                        size,
                        heap.len(),
                    )));
                }

                // Update the certified data.
                self.sandbox_safe_system_state
                    .system_state_changes
                    .new_certified_data = Some(heap[src..src + size].to_vec());
                Ok(())
            }
        };
        trace_syscall!(
            self,
            ic0_certified_data_set,
            result,
            src,
            size,
            summarize(heap, src, size)
        );
        result
    }

    fn ic0_canister_status(&self) -> HypervisorResult<u32> {
        let result = match &self.api_type {
            ApiType::Start { .. } => Err(self.error_for("ic0_canister_status")),
            ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::Init { .. }
            | ApiType::Cleanup { .. }
            | ApiType::SystemTask { .. }
            | ApiType::Update { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::InspectMessage { .. } => match self.sandbox_safe_system_state.status {
                CanisterStatusView::Running => Ok(1),
                CanisterStatusView::Stopping => Ok(2),
                CanisterStatusView::Stopped => Ok(3),
            },
        };
        trace_syscall!(self, ic0_canister_status, result);
        result
    }

    fn ic0_mint_cycles(&mut self, amount: u64) -> HypervisorResult<u64> {
        let result = match self.api_type {
            ApiType::Start { .. }
            | ApiType::Init { .. }
            | ApiType::PreUpgrade { .. }
            | ApiType::Cleanup { .. }
            | ApiType::ReplicatedQuery { .. }
            | ApiType::NonReplicatedQuery { .. }
            | ApiType::InspectMessage { .. } => Err(self.error_for("ic0_mint_cycles")),
            ApiType::Update { .. }
            | ApiType::SystemTask { .. }
            | ApiType::ReplyCallback { .. }
            | ApiType::RejectCallback { .. } => {
                self.sandbox_safe_system_state
                    .mint_cycles(Cycles::from(amount))?;
                Ok(amount)
            }
        };
        trace_syscall!(self, ic0_mint_cycles, result, amount);
        result
    }

    fn ic0_debug_print(&self, src: u32, size: u32, heap: &[u8]) -> HypervisorResult<()> {
        const MAX_DEBUG_MESSAGE_SIZE: u32 = 32 * 1024;
        let size = size.min(MAX_DEBUG_MESSAGE_SIZE);
        let msg = match valid_subslice("ic0.debug_print", src, size, heap) {
            Ok(bytes) => String::from_utf8_lossy(bytes).to_string(),
            Err(_) => {
                // Do not trap here!
                // debug.print should never fail, so if the specified memory range
                // is invalid, we ignore it and print the error message
                "(debug message out of memory bounds)".to_string()
            }
        };
        eprintln!(
            "[Canister {}] {}",
            self.sandbox_safe_system_state.canister_id, msg
        );
        trace_syscall!(self, ic0_debug_print, src, size, summarize(heap, src, size));
        Ok(())
    }

    fn ic0_trap(&self, src: u32, size: u32, heap: &[u8]) -> HypervisorResult<()> {
        const MAX_ERROR_MESSAGE_SIZE: u32 = 16 * 1024;
        let size = size.min(MAX_ERROR_MESSAGE_SIZE);
        let result = {
            let msg = valid_subslice("trap", src, size, heap)
                .map(|bytes| String::from_utf8_lossy(bytes).to_string())
                .unwrap_or_else(|_| "(trap message out of memory bounds)".to_string());
            CalledTrap(msg)
        };
        trace_syscall!(self, ic0_trap, src, size, summarize(heap, src, size));
        Err(result)
    }
}

/// The default implementation of the `OutOfInstructionHandler` trait.
/// It simply returns an out-of-instructions error.
pub struct DefaultOutOfInstructionsHandler {}

impl OutOfInstructionsHandler for DefaultOutOfInstructionsHandler {
    fn out_of_instructions(
        &self,
        _instruction_counter: i64,
        _execution_complexity: ExecutionComplexity,
    ) -> HypervisorResult<i64> {
        Err(HypervisorError::InstructionLimitExceeded)
    }
}

pub(crate) fn copy_cycles_to_heap(
    cycles: Cycles,
    dst: u32,
    heap: &mut [u8],
    method_name: &str,
) -> HypervisorResult<()> {
    // Copy a 128-bit value to the canister memory.
    let bytes = cycles.get().to_le_bytes();
    let size = bytes.len();
    assert_eq!(size, 16);

    let dst = dst as usize;
    let (upper_bound, overflow) = dst.overflowing_add(size);
    if overflow || upper_bound > heap.len() {
        return Err(ContractViolation(format!(
            "{} failed because dst + size is out of bounds.\
                Found dst = {} and size = {} while must be <= {}",
            method_name,
            dst,
            size,
            heap.len(),
        )));
    }
    deterministic_copy_from_slice(&mut heap[dst..dst + size], &bytes);
    Ok(())
}

pub(crate) fn valid_subslice<'a>(
    ctx: &str,
    src: u32,
    len: u32,
    slice: &'a [u8],
) -> HypervisorResult<&'a [u8]> {
    let len = len as usize;
    let src = src as usize;
    if slice.len() < src + len {
        return Err(ContractViolation(format!(
            "{}: src={} + length={} exceeds the slice size={}",
            ctx,
            src,
            len,
            slice.len()
        )));
    }
    Ok(&slice[src..src + len])
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_valid_subslice() {
        // empty slice
        assert!(valid_subslice("", 0, 0, &[]).is_ok());
        // the only possible non-empty slice
        assert!(valid_subslice("", 0, 1, &[1]).is_ok());
        // valid empty slice
        assert!(valid_subslice("", 1, 0, &[1]).is_ok());

        // just some valid cases
        assert!(valid_subslice("", 0, 4, &[1, 2, 3, 4]).is_ok());
        assert!(valid_subslice("", 1, 3, &[1, 2, 3, 4]).is_ok());
        assert!(valid_subslice("", 2, 2, &[1, 2, 3, 4]).is_ok());

        // invalid longer-than-the-heap subslices
        assert!(valid_subslice("", 3, 2, &[1, 2, 3, 4]).is_err());
        assert!(valid_subslice("", 0, 5, &[1, 2, 3, 4]).is_err());
        assert!(valid_subslice("", 4, 1, &[1, 2, 3, 4]).is_err());
    }
}
