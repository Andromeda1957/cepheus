use super::*;
#[test]
fn shell_advice_preserves_nested_commands_heredoc_data_and_neutral_parse_failure() {
    for (command, category) in [
        ("rm $WORK/file", "unguarded-removal"),
        ("rm \"${HOME:?}\"/cache", "normally-set-removal"),
        ("rm \"$(pwd)\"/file", "computed-removal"),
        ("bash -c 'rm $WORK/file'", "unguarded-removal"),
        ("eval \"rm $FILE\"", "unguarded-removal"),
        ("find . -print0 | xargs -0 rm $TARGET", "unguarded-removal"),
        ("tool &", "background"),
        ("nohup tool >log 2>&1 &", "background"),
        ("echo '{}' > .claude/settings.json", "settings-write"),
        ("echo '{}' >| .claude/settings.json", "settings-write"),
        ("cat <<EOF &\nliteral data\nEOF", "background"),
        ("cp config.json .claude/settings.local.json", "settings-write"),
        ("pkill -f synthetic-process", "self-match"),
        ("kill $(pgrep -f synthetic-process)", "self-match"),
        ("echo $(rm $WORK/file)", "unguarded-removal"),
    ] {
        assert!(violations(command).contains(&category), "{category}: {command}");
    }
    for command in [
        "rm \"${WORK:?}\"/file",
        "rm /scratch/task/file",
        "rmdir /scratch/task/dir",
        "pkill -x synthetic-process",
        "kill 12345",
        "pgrep -fa synthetic-process",
        "echo 'a & b'",
        "printf '%s' 'rm $HOME is documentation'",
        "a && b || c",
        "cat .claude/settings.json",
        "cp .claude/settings.json /scratch/config.json",
        "git push",
        "tool deploy",
        "rm '",
        "echo & 'unterminated",
        "cat <<'EOF'\nrm $WORK/file &\nEOF\n",
    ] {
        assert!(violations(command).is_empty(), "{command}");
    }
    assert!(violations(&"word ".repeat(MAX_TOKENS + 1)).is_empty());
    assert!(violations(&"x".repeat(MAX_COMMAND + 1)).is_empty());
}
