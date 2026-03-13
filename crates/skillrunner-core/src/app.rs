use crate::state::AppState;
use anyhow::Result;

pub struct SkillRunnerApp {
    pub state: AppState,
}

impl SkillRunnerApp {
    pub fn bootstrap() -> Result<Self> {
        let state = AppState::bootstrap()?;
        Ok(Self { state })
    }
}
