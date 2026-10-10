mod debug_trace;
mod ring;

pub use debug_trace::{
    TraceContext, active_log_path, configure_from_env, enabled, event, is_truthy, next_step_id,
    next_turn_id, timestamp_ms,
};
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
