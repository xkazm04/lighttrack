use super::*;

/// The decision this change creates. A cancelled run keeps its report and loses its delivery;
/// a reclaimed run loses both; an uninterrupted run keeps both. Before the seam existed there
/// was one boolean, consulted after `exec` had already delivered — so a cancelled run's
/// connector had fired by the time anybody asked.
#[test]
fn the_stop_check_decides_between_producing_and_delivering() {
    assert_eq!(settlement(None), Settlement::DeliverAndSettle);
    assert_eq!(
        settlement(Some(&Interrupt::Cancelled)),
        Settlement::SettleOnly,
        "a cancelled run must not deliver, and must still report"
    );
    assert_eq!(
        settlement(Some(&Interrupt::Lost)),
        Settlement::Abandon,
        "a reclaimed run's result belongs to nobody"
    );
}

/// An operator reading a device log has to be able to tell the two apart. They were one message
/// ("LEASE LOST") for both, because the agent could not tell them apart either.
#[test]
fn a_cancel_and_a_takeover_do_not_read_the_same() {
    assert_ne!(Interrupt::Cancelled.headline(), Interrupt::Lost.headline());
    assert!(Interrupt::Cancelled.headline().contains("CANCELLED"));
    assert!(Interrupt::Lost.headline().contains("LEASE LOST"));
    assert!(Interrupt::Cancelled
        .consequence()
        .contains("still reported"));
    assert!(Interrupt::Lost.consequence().contains("NOT be reported"));
}
