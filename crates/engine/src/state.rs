use nscore::{Event, EventId, EventKind};
use std::collections::HashSet;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionState {
    /// Highest turn seen.
    pub turn: u32,
    /// ("user" | "assistant", text)
    pub history: Vec<(String, String)>,
    /// Last unconfirmed PendingConfirmation.
    pub pending_confirmation: Option<EventId>,
    /// Turn on which the pending confirmation was created (expiry substrate).
    pub pending_turn: Option<u32>,
    /// Turn of the most recent Confirmed event.
    pub confirmed_this_turn_of: Option<u32>,
    /// DedupeGate substrate: action names that have ToolCalled this session.
    pub fired_tags: HashSet<String>,
}

pub fn fold(events: &[Event]) -> SessionState {
    let mut s = SessionState::default();
    for e in events {
        if e.turn > s.turn {
            s.turn = e.turn;
        }
        match &e.kind {
            EventKind::UserSaid { text } => s.history.push(("user".into(), text.clone())),
            EventKind::Replied { text } => s.history.push(("assistant".into(), text.clone())),
            EventKind::ToolCalled { action, .. } => {
                s.fired_tags.insert(action.clone());
            }
            EventKind::PendingConfirmation { .. } => {
                s.pending_confirmation = Some(e.id);
                s.pending_turn = Some(e.turn);
            }
            EventKind::Confirmed { pending } if s.pending_confirmation == Some(*pending) => {
                s.pending_confirmation = None;
                s.pending_turn = None;
                s.confirmed_this_turn_of = Some(e.turn);
            }
            _ => {}
        }
    }
    s
}

pub fn state_summary(s: &SessionState) -> String {
    format!("turn {}, {} messages", s.turn, s.history.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::*;

    #[test]
    fn fold_builds_history_and_tracks_confirmation() {
        let mut log = EventLog::new(SessionId("s".into()));
        log.append(1, Timestamp(1), EventKind::UserSaid { text: "hi".into() });
        log.append(1, Timestamp(2), EventKind::Replied { text: "hello".into() });
        log.append(2, Timestamp(3), EventKind::UserSaid { text: "delete it".into() });
        let pending_id = log
            .append(
                2,
                Timestamp(4),
                EventKind::PendingConfirmation { proposal_of: EventId(3), staged: None },
            )
            .id;
        let s = fold(log.events());
        assert_eq!(s.turn, 2);
        assert_eq!(s.history.len(), 3);
        assert_eq!(s.pending_confirmation, Some(pending_id));

        let mut log2 = EventLog::from_events(SessionId("s".into()), log.events().to_vec());
        log2.append(3, Timestamp(5), EventKind::Confirmed { pending: pending_id });
        let s2 = fold(log2.events());
        assert_eq!(s2.pending_confirmation, None);
    }

    #[test]
    fn fold_tracks_fired_actions_pending_turn_and_confirmation_turn() {
        let mut log = EventLog::new(SessionId("s".into()));
        log.append(1, Timestamp(1), EventKind::UserSaid { text: "go".into() });
        log.append(1, Timestamp(2), EventKind::ToolCalled { action: "echo".into(), args: vec![] });
        let pending = log
            .append(
                1,
                Timestamp(3),
                EventKind::PendingConfirmation { proposal_of: EventId(1), staged: None },
            )
            .id;
        let s = fold(log.events());
        assert!(s.fired_tags.contains("echo"));
        assert_eq!(s.pending_confirmation, Some(pending));
        assert_eq!(s.pending_turn, Some(1));
        assert_eq!(s.confirmed_this_turn_of, None);

        log.append(2, Timestamp(4), EventKind::UserSaid { text: "yes".into() });
        log.append(2, Timestamp(5), EventKind::Confirmed { pending });
        let s2 = fold(log.events());
        assert_eq!(s2.pending_confirmation, None);
        assert_eq!(s2.pending_turn, None);
        assert_eq!(s2.confirmed_this_turn_of, Some(2));
    }
}
