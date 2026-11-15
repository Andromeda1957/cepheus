# Host upkeep

This bundled native plugin maintains each service's local Claude Enter policy:
Enter sends immediately and `ctrl+x ctrl+s` is unbound. It checks the default
Claude home, named profiles and configured credential mirrors initially and
every minute, repairing valid files in place. Repeated failures are reported
once until they change or recover; missing mirrors and malformed files hold
the change with an error.

The plugin operates only on local configured consumers. Host roles, session
placement, repository mirrors and tool installation are configured separately.
It does not send messages or submit a terminal draft.

Set `"plugins": {"host-bootstrap": false}` to disable local policy upkeep.
