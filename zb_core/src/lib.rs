pub mod build;
pub mod context;
pub mod errors;
pub mod formula;
pub mod platform;

pub use build::{BuildPlan, BuildSystem, InstallMethod};
pub use context::{ConcurrencyLimits, Context, LogLevel, LoggerHandle, Paths};
pub use errors::{ConflictedLink, Error};
pub use formula::{
    Formula, Guard, KegOnly, KegOnlyReason, PathBase, PathSpec, Platform, PostInstallStep,
    ProcessMatch, SelectedBottle, StepAction, Sudo, compatible_codenames, formula_token,
    resolve_closure, select_bottle,
};
pub use platform::{is_android_host, termux};

#[cfg(target_os = "macos")]
pub use formula::macos_major_version;
