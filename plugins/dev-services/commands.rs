use crate::actions::{spec, Args, Spec};
pub const COMMANDS: &[Spec] =
    &[spec("dev", &[], "dev [on|off]", "start or stop the dev server", Args::Words(&["on", "off"]))];
