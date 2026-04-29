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
    /// Open the latest run of a workflow in a browser
    Open {
        /// Workflow file name or fuzzy match on workflow name
        workflow: String,
    },
}
