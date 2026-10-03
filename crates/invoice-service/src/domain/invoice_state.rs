// The invoice state machine, exactly as in DESIGN.md section 2:
//
//   draft --finalize--> open --payment--> paid (terminal)
//   draft --void-----> void (terminal)
//   open  --void-----> void
//   open  --mark----> uncollectible --void--> void
//
// `paid` and `void` are terminal. Nothing moves backwards.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceState {
    Draft,
    Open,
    Paid,
    Void,
    Uncollectible,
}

impl InvoiceState {
    /// The text stored in the database and shown in the API.
    pub fn as_str(self) -> &'static str {
        match self {
            InvoiceState::Draft => "draft",
            InvoiceState::Open => "open",
            InvoiceState::Paid => "paid",
            InvoiceState::Void => "void",
            InvoiceState::Uncollectible => "uncollectible",
        }
    }

    pub fn parse(text: &str) -> Option<InvoiceState> {
        match text {
            "draft" => Some(InvoiceState::Draft),
            "open" => Some(InvoiceState::Open),
            "paid" => Some(InvoiceState::Paid),
            "void" => Some(InvoiceState::Void),
            "uncollectible" => Some(InvoiceState::Uncollectible),
            _ => None,
        }
    }
}

/// True if moving from `from` to `to` is allowed. This is the only place the rules live.
pub fn can_transition(from: InvoiceState, to: InvoiceState) -> bool {
    use InvoiceState::*;
    matches!(
        (from, to),
        (Draft, Open)
            | (Draft, Void)
            | (Open, Paid)
            | (Open, Void)
            | (Open, Uncollectible)
            | (Uncollectible, Void)
    )
}

#[cfg(test)]
mod tests {
    use super::InvoiceState::*;
    use super::*;

    const ALL: [InvoiceState; 5] = [Draft, Open, Paid, Void, Uncollectible];

    #[test]
    fn every_valid_transition_is_allowed() {
        let valid = [
            (Draft, Open),
            (Draft, Void),
            (Open, Paid),
            (Open, Void),
            (Open, Uncollectible),
            (Uncollectible, Void),
        ];
        for (from, to) in valid {
            assert!(
                can_transition(from, to),
                "{from:?} -> {to:?} should be allowed"
            );
        }
    }

    #[test]
    fn some_named_invalid_transitions_are_refused() {
        let invalid = [
            (Draft, Paid),          // must be finalized first
            (Draft, Uncollectible), // only open invoices can be written off
            (Open, Draft),          // no going back
            (Paid, Void),           // paid is terminal
            (Paid, Open),
            (Void, Open), // void is terminal
            (Void, Draft),
            (Uncollectible, Paid), // late payment was deliberately cut
            (Uncollectible, Open),
            (Open, Open), // no self-transitions
        ];
        for (from, to) in invalid {
            assert!(
                !can_transition(from, to),
                "{from:?} -> {to:?} should be refused"
            );
        }
    }

    #[test]
    fn exactly_six_of_twenty_five_pairs_are_valid() {
        let count = ALL
            .iter()
            .flat_map(|&f| ALL.iter().map(move |&t| (f, t)))
            .filter(|&(f, t)| can_transition(f, t))
            .count();
        assert_eq!(count, 6);
    }

    #[test]
    fn terminal_states_have_no_exits() {
        for to in ALL {
            assert!(!can_transition(Paid, to));
            assert!(!can_transition(Void, to));
        }
    }

    #[test]
    fn text_round_trips() {
        for s in ALL {
            assert_eq!(InvoiceState::parse(s.as_str()), Some(s));
        }
        assert_eq!(InvoiceState::parse("DRAFT"), None);
        assert_eq!(InvoiceState::parse("refunded"), None);
    }
}
