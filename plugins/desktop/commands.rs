use crate::actions::{spec, Args, Spec};
pub const COMMANDS: &[Spec] =
    &[spec("window", &[], "window", "focus the selected outside session's local terminal window", Args::None)];
