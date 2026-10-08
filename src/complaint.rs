//! A failure logged once, however long it lasts, and its recovery.
//!
//! The panel, the session bus and logind can each be out of reach for a
//! while, at boot especially, and are tried again every few seconds. The log
//! should say so once, not once per attempt, and say when it ends.

use log::{info, warn};

#[derive(Debug, Default)]
pub struct Complaint {
    standing: Option<String>,
}

impl Complaint {
    /// Warns with `message`, unless that very complaint already stands. A
    /// different message is a different failure, and is logged.
    pub fn raise(&mut self, message: String) {
        if self.standing.as_ref() != Some(&message) {
            warn!("{message}");
            self.standing = Some(message);
        }
    }

    /// Logs `recovery` if a complaint stood, and withdraws it.
    pub fn withdraw(&mut self, recovery: &str) {
        if self.standing.take().is_some() {
            info!("{recovery}");
        }
    }

    /// Withdraws the complaint without a word, for a caller that has its
    /// own to say.
    pub fn clear(&mut self) {
        self.standing = None;
    }

    /// Whether a complaint stands.
    #[must_use]
    pub fn standing(&self) -> bool {
        self.standing.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_complaint_stands_until_withdrawn() {
        let mut complaint = Complaint::default();
        assert!(!complaint.standing());
        complaint.raise("LCD unavailable".to_owned());
        complaint.raise("LCD unavailable".to_owned());
        assert!(complaint.standing());
        complaint.withdraw("LCD connected");
        assert!(!complaint.standing());
        complaint.withdraw("LCD connected");
        assert!(!complaint.standing());
        complaint.raise("LCD unavailable".to_owned());
        complaint.clear();
        assert!(!complaint.standing());
    }
}
