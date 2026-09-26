//! M28: skills a person's message names by keyword load with it
//! (docs/skills.md). The agent asks [`PromptTriggers`] only about the goal
//! of [`crate::Agent::run_user`], a message a person typed: tool results,
//! web pages, sub-agent reports and scheduled prompts never reach it.

/// Which skills a person's message triggers.
pub trait PromptTriggers: Send + Sync {
    /// Skills whose triggers `prompt` names, leaving out those already in
    /// `loaded` (a `<skill_content>` block in the history), vetted and
    /// rendered, within the turn's limits, in the order they load.
    fn triggered(&self, prompt: &str, loaded: &[String]) -> Vec<Triggered>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Triggered {
    pub name: String,
    /// The trigger, as the skill wrote it.
    pub matched: String,
    pub load: TriggerLoad,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerLoad {
    /// The `<skill_content>` block, as `activate_skill` returns it.
    Loaded(String),
    /// Over the turn's budget: the model is told it matched, and can
    /// activate it itself.
    TooLarge,
    /// Not vetted (a scan finding, an edited install): the model isn't
    /// told, the owner's log says why.
    Refused(String),
}
