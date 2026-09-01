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
    /// DedupeGate substrate (used from M3 on).
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
            EventKind::PendingConfirmation { .. } => s.pending_confirmation = Some(e.id),
            EventKind::Confirmed { pending } => {
                if s.pending_confirmation == Some(*pending) {
                    s.pending_confirmation = None;
                }
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
}
