//! When the broker's answer to "what would this order do?" may be acted on.
//!
//! A preview is a statement about the market a moment ago. **Only a fresh, accepting answer allows
//! an order or an edit to be reviewed**: no answer, a failed answer, a refusal and an old answer do
//! not. (A broker's order endpoint may not require a preview; this is the rule of the application
//! that uses the crate, so a real order is never sent on a guess.)

use std::time::Duration;

/// How long the broker's answer to a preview may be acted on. After this it is asked for again.
pub const PREVIEW_FRESH: Duration = Duration::from_secs(60);

/// How long typing must pause before the broker is asked to preview what is typed.
pub const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(450);

/// What the broker has said about the order as it stands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreviewAnswer {
    /// Not asked yet, or asked and not answered.
    Waiting,
    /// The question could not be answered (the connection, a refusal of the preview itself).
    Failed(String),
    /// The broker answered that it would refuse the order.
    Problems,
    /// The broker answered that it would accept the order.
    Accepted,
}

/// Whether the order may be reviewed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreviewGate {
    Waiting,
    Failed(String),
    Blocked,
    Stale,
    Ready,
}

impl PreviewGate {
    pub fn allows_review(&self) -> bool {
        *self == Self::Ready
    }
}

/// `age` is how long ago the accepting answer arrived.
pub fn preview_gate(answer: PreviewAnswer, age: Option<Duration>) -> PreviewGate {
    match answer {
        PreviewAnswer::Waiting => PreviewGate::Waiting,
        PreviewAnswer::Failed(text) => PreviewGate::Failed(text),
        PreviewAnswer::Problems => PreviewGate::Blocked,
        PreviewAnswer::Accepted => match age {
            Some(age) if age <= PREVIEW_FRESH => PreviewGate::Ready,
            _ => PreviewGate::Stale,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_fresh_accepting_answer_allows_review() {
        let young = Some(Duration::from_secs(1));
        assert_eq!(
            preview_gate(PreviewAnswer::Accepted, young),
            PreviewGate::Ready
        );
        assert!(PreviewGate::Ready.allows_review());
        for gate in [
            preview_gate(PreviewAnswer::Waiting, None),
            preview_gate(PreviewAnswer::Failed("down".to_owned()), None),
            preview_gate(PreviewAnswer::Problems, young),
            preview_gate(PreviewAnswer::Accepted, None),
            preview_gate(
                PreviewAnswer::Accepted,
                Some(PREVIEW_FRESH + Duration::from_secs(1)),
            ),
        ] {
            assert!(!gate.allows_review(), "{gate:?}");
        }
    }

    #[test]
    fn the_age_limit_is_inclusive_and_an_old_answer_is_stale_not_blocked() {
        assert_eq!(
            preview_gate(PreviewAnswer::Accepted, Some(PREVIEW_FRESH)),
            PreviewGate::Ready
        );
        assert_eq!(
            preview_gate(
                PreviewAnswer::Accepted,
                Some(PREVIEW_FRESH + Duration::from_millis(1))
            ),
            PreviewGate::Stale
        );
        assert_eq!(
            preview_gate(PreviewAnswer::Problems, None),
            PreviewGate::Blocked
        );
    }
}
