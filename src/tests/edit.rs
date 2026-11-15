use super::*;

fn draft(text: &str) -> Draft {
    let mut draft = Draft::default();
    draft.insert(text);
    draft
}

#[test]
fn the_cursor_moves_and_deletes_by_character_and_word_in_any_script() {
    let mut d = draft("héllo wörld, 東京");
    d.left();
    assert_eq!(d.cursor(), "héllo wörld, 東".len());
    d.backspace();
    assert_eq!(d.text(), "héllo wörld, 京");
    d.word_left();
    assert_eq!(d.cursor(), "héllo ".len());
    d.delete_word_back();
    assert_eq!((d.text(), d.cursor()), ("wörld, 京", 0));
    d.end();
    d.delete_word_back();
    assert_eq!(d.text(), "wörld, ");
    d.home();
    d.delete_word_forward();
    assert_eq!((d.text(), d.cursor()), (", ", 0));
    d.right();
    d.delete();
    assert_eq!(d.text(), ",");
}

#[test]
fn home_end_and_kill_work_on_the_line_the_cursor_is_on() {
    let mut d = draft("first line\nsecond line");
    d.home();
    assert_eq!(d.cursor(), "first line\n".len());
    d.word_right();
    d.kill_line();
    assert_eq!(d.text(), "first line\nsecond");
    d.home();
    d.left();
    assert_eq!(d.cursor(), "first line".len());
    d.home();
    assert_eq!(d.cursor(), 0);
    d.end();
    assert_eq!(d.cursor(), "first line".len());
}

#[test]
fn a_paste_goes_in_at_the_cursor_with_its_lines_and_without_control_characters() {
    let mut d = draft("ab");
    d.left();
    d.insert("1\r\n2\r3\t\x1b[31m");
    assert_eq!(d.text(), "a1\n2\n3    [31mb");
    assert_eq!(d.cursor(), "a1\n2\n3    [31m".len());
}

#[test]
fn up_brings_back_earlier_questions_and_down_the_one_being_typed() {
    let earlier = ["newest".to_string(), "oldest".to_string()];
    let mut d = draft("half");
    assert!(d.older(&earlier));
    assert_eq!(d.text(), "newest");
    assert!(d.older(&earlier));
    assert_eq!(d.text(), "oldest");
    assert!(!d.older(&earlier));
    assert_eq!(d.text(), "oldest");
    assert!(d.newer(&earlier));
    assert_eq!(d.text(), "newest");
    assert!(d.newer(&earlier));
    assert_eq!((d.text(), d.cursor()), ("half", 4));
    assert!(!d.newer(&earlier));
    // A question brought back and edited is the one being typed.
    d.older(&earlier);
    d.insert("!");
    assert!(!d.newer(&earlier));
    assert_eq!(d.text(), "newest!");
}
