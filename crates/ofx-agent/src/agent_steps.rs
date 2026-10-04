pub(crate) fn allows_step(limit: u64, completed_steps: u64) -> bool {
    limit == 0 || completed_steps < limit
}

#[cfg(test)]
mod tests {
    use super::allows_step;

    #[test]
    fn agent_step_policy_treats_zero_as_unbounded_and_positives_as_exact_caps() {
        assert!(allows_step(0, 0));
        assert!(allows_step(0, 25));
        assert!(allows_step(2, 0));
        assert!(allows_step(2, 1));
        assert!(!allows_step(2, 2));
    }
}
