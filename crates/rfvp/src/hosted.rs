//! Standard-library entry point for RFVP's host-neutral core.
//!
//! `hosted` retains the portable RFVP resource, render and audio command
//! surfaces while deliberately excluding the upstream winit/wgpu application.

pub use crate::no_std_core::{
    RfvpBootConfig as HostedBootConfig, RfvpCoreConfig as HostedConfig,
    RfvpCoreRunState as HostedRunState, RfvpLoadedGame as HostedLoadedGame,
    RfvpResourceEntry as HostedResourceEntry, RfvpTickResult as HostedTickResult, RfvpCore,
};

pub const HOSTED_ABI_VERSION: u16 = 1;
