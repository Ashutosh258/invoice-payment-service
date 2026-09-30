use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum InvoiceStatus {
    Draft,
    Open,
    Paid,
    Void,
    Uncollectible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceAction {
    Finalize,
    Void,
    MarkUncollectible,
    RecordPayment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("cannot {action} an invoice that is {from}")]
pub struct InvalidTransition {
    pub from: InvoiceStatus,
    pub action: InvoiceAction,
}

impl InvoiceStatus {
    pub fn apply(self, action: InvoiceAction) -> Result<InvoiceStatus, InvalidTransition> {
        use InvoiceAction as A;
        use InvoiceStatus as S;

        match (self, action) {
            (S::Draft, A::Finalize) => Ok(S::Open),
            (S::Draft | S::Open | S::Uncollectible, A::Void) => Ok(S::Void),
            (S::Open, A::MarkUncollectible) => Ok(S::Uncollectible),
            (S::Open | S::Uncollectible, A::RecordPayment) => Ok(S::Paid),
            (from, action) => Err(InvalidTransition { from, action }),
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Paid | Self::Void)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Open => "open",
            Self::Paid => "paid",
            Self::Void => "void",
            Self::Uncollectible => "uncollectible",
        }
    }
}

impl fmt::Display for InvoiceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for InvoiceAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Finalize => "finalize",
            Self::Void => "void",
            Self::MarkUncollectible => "mark as uncollectible",
            Self::RecordPayment => "pay",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::InvoiceAction as A;
    use super::InvoiceStatus as S;
    use super::*;

    const ALL_STATES: [S; 5] = [S::Draft, S::Open, S::Paid, S::Void, S::Uncollectible];
    const ALL_ACTIONS: [A; 4] = [A::Finalize, A::Void, A::MarkUncollectible, A::RecordPayment];

    const ALLOWED: &[(S, A, S)] = &[
        (S::Draft, A::Finalize, S::Open),
        (S::Draft, A::Void, S::Void),
        (S::Open, A::Void, S::Void),
        (S::Open, A::MarkUncollectible, S::Uncollectible),
        (S::Open, A::RecordPayment, S::Paid),
        (S::Uncollectible, A::Void, S::Void),
        (S::Uncollectible, A::RecordPayment, S::Paid),
    ];

    #[test]
    fn every_state_action_pair_matches_the_table() {
        for from in ALL_STATES {
            for action in ALL_ACTIONS {
                let expected = ALLOWED
                    .iter()
                    .find(|(f, a, _)| *f == from && *a == action)
                    .map(|(_, _, to)| *to);

                match (from.apply(action), expected) {
                    (Ok(to), Some(want)) => assert_eq!(to, want, "{from} --{action}-->"),
                    (Err(err), None) => assert_eq!(err, InvalidTransition { from, action }),
                    (got, want) => panic!("{from} --{action}--> gave {got:?}, table says {want:?}"),
                }
            }
        }
    }

    #[test]
    fn terminal_states_have_no_way_out() {
        for from in ALL_STATES.into_iter().filter(|s| s.is_terminal()) {
            for action in ALL_ACTIONS {
                assert!(from.apply(action).is_err(), "{from} should reject {action}");
            }
        }
    }

    #[test]
    fn error_message_reads_naturally() {
        let err = S::Paid.apply(A::Void).unwrap_err();
        assert_eq!(err.to_string(), "cannot void an invoice that is paid");
    }
}
