use crate::actions::{spec, Args, Spec};
pub const COMMANDS: &[Spec] = &[spec(
    "launch",
    &[],
    "launch [reset [program] [model|effort]]",
    "what new agents start with; :<model> or :<effort> (e.g. :opus, :xhigh) changes it",
    Args::Words(&["reset"]),
)];
