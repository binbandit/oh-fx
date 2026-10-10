mod debug_trace;
mod json_preview;
mod preview;
mod ring;

pub use debug_trace::{
    TraceContext, active_log_path, configure_from_env, enabled, event, is_truthy, log,
    next_step_id, next_subagent_id, next_turn_id, timestamp_ms,
};
pub use json_preview::keyless_json_preview;
pub use preview::{preview, terminal_preview};
pub use ring::{Ring, Sequenced};

#[macro_export]
macro_rules! trace_event {
    ($scope:expr, $name:expr, $context:expr) => {
        if $crate::enabled($scope) {
            $crate::event($scope, $name, $context, ::std::option::Option::None);
        }
    };
    ($scope:expr, $name:expr, $context:expr, $($message:tt)+) => {
        if $crate::enabled($scope) {
            $crate::event(
                $scope,
                $name,
                $context,
                ::std::option::Option::Some(::std::format_args!($($message)+)),
            );
        }
    };
}

#[macro_export]
macro_rules! trace_log {
    ($scope:expr, $($message:tt)+) => {
        if $crate::enabled($scope) {
            $crate::log($scope, ::std::format_args!($($message)+));
        }
    };
}
