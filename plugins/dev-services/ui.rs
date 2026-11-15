use super::*;
impl App {
    /// `:dev [on|off]`.
    pub(crate) fn dev_command(&mut self, rest: &str) -> Result<(), String> {
        let running = self.state.dev_server.pid.is_some();
        match rest {
            "" => self.toggle_dev_server(),
            "on" if running => self.set_notice("The dev server is running.", false),
            "off" if !running => self.set_notice("The dev server is not running.", false),
            "on" | "off" => self.toggle_dev_server(),
            other => return Err(format!("{other:?} is not on or off")),
        }
        Ok(())
    }
}
