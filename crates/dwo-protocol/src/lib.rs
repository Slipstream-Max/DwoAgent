#![doc = include_str!("../README.md")]

mod registry;
mod rpc;

pub use registry::{
    ManagementCapabilities, MethodOperation, MethodRoute, MethodSpec, capabilities,
    is_side_effect_method, method_allowed, method_spec,
};
pub use rpc::{
    PromptDirectiveOption, PromptDirectiveOptions, ReasoningOption, RpcError, RpcEvent, RpcRequest,
    RpcResponse, RpcRoute, SessionConfig, SessionInfo, SessionMode, SessionModelOption,
    SessionOptions, SessionRecord, SessionSnapshot,
};
