use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "jog", about = "TUI control surface for CI/CD", version)]
pub struct Cli {
    /// Override repo (owner/name); auto-detected from `git remote` if omitted
    #[arg(long, global = true)]
    pub repo: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Trigger a workflow (headless, fire-and-forget)
    Run {
        /// Workflow file name or fuzzy match on workflow name
        workflow: String,
        /// Git ref (branch/tag/SHA); defaults to the current branch
        #[arg(default_value = "")]
        reference: String,
        /// workflow_dispatch input as KEY=VAL (repeat for multiple)
        #[arg(short = 'i', long = "input", value_name = "KEY=VAL")]
        inputs: Vec<String>,
    },
    /// Live status view for the latest run of a workflow
    Watch {
        /// Workflow file name or fuzzy match on workflow name
        workflow: String,
    },
    /// Open a repo tab in a browser, or the latest run of a workflow
    #[command(visible_alias = "o")]
    Open {
        /// Tab: c(ode), i(ssues), pr (pulls), a(ctions), p(rojects), s(ettings);
        /// otherwise a workflow file name or fuzzy match. Omit for Actions.
        workflow: Option<String>,
    },
    /// Multi-repo dashboard for every repo in `[provider] repos`
    Repos,
}
