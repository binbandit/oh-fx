use super::*;

const CAPACITY: usize = 64;

#[test]
fn the_ring_retains_the_newest_events_in_order() {
    let ring = Ring::new(CAPACITY);
    for index in 0..CAPACITY + 3 {
        ring.record(index);
    }
    let events = ring.snapshot();
    assert_eq!(events.len(), CAPACITY);
    assert_eq!(
        events[0],
        Sequenced {
            sequence: 4,
            event: 3
        }
    );
    assert_eq!(
        events[CAPACITY - 1],
        Sequenced {
            sequence: u64::try_from(CAPACITY + 3).unwrap(),
            event: CAPACITY + 2,
        }
    );
    assert_eq!(
        events[CAPACITY - 2].sequence,
        u64::try_from(CAPACITY + 2).unwrap()
    );
    assert!(Ring::<usize>::new(CAPACITY).snapshot().is_empty());
}

#[test]
fn a_reset_ring_numbers_its_next_event_from_one() {
    let ring = Ring::new(2);
    ring.record("first");
    ring.record("second");
    ring.record("third");
    ring.reset();
    assert!(ring.snapshot().is_empty());
    ring.record("after");
    assert_eq!(
        ring.snapshot(),
        [Sequenced {
            sequence: 1,
            event: "after"
        }]
    );
}

#[test]
fn a_ring_without_capacity_keeps_nothing() {
    let ring = Ring::new(0);
    ring.record(1);
    assert!(ring.snapshot().is_empty());
}
