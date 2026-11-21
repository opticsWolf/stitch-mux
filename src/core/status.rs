use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgramState {
    #[default]
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgramStatusKind {
    Permission,
    Question,
    Auth,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProgramStatus {
    pub id: Option<String>,
    pub state: ProgramState,
    pub kind: Option<ProgramStatusKind>,
    pub app: Option<String>,
    pub message: Option<String>,
    pub updated_at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActivitySummary {
    pub idle: u32,
    pub working: u32,
    pub blocked: u32,
    pub done: u32,
    pub error: u32,
}

impl ActivitySummary {
    pub fn from_statuses<'a>(
        statuses: impl IntoIterator<Item = Option<&'a ProgramStatus>>,
    ) -> Self {
        let mut summary = Self::default();
        for status in statuses.into_iter().flatten() {
            match status.state {
                ProgramState::Idle => summary.idle += 1,
                ProgramState::Working => summary.working += 1,
                ProgramState::Blocked => summary.blocked += 1,
                ProgramState::Done => summary.done += 1,
                ProgramState::Error => summary.error += 1,
            }
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_counts_current_statuses_only() {
        let statuses = [
            Some(ProgramStatus {
                state: ProgramState::Working,
                ..Default::default()
            }),
            Some(ProgramStatus {
                state: ProgramState::Blocked,
                ..Default::default()
            }),
            None,
        ];
        let summary = ActivitySummary::from_statuses(statuses.iter().map(Option::as_ref));
        assert_eq!(summary.working, 1);
        assert_eq!(summary.blocked, 1);
        assert_eq!(summary.idle, 0);
    }
}
