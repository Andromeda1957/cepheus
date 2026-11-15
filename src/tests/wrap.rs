use super::*;

#[test]
fn rows_break_between_words_and_keep_the_hanging_indent() {
    let rows = text("the quick brown fox jumps over the lazy dog", "• ", "  ", 16);
    assert_eq!(rows, ["• the quick", "  brown fox", "  jumps over the", "  lazy dog"]);
    assert!(rows.iter().all(|row| row.width() <= 16));
}

#[test]
fn a_word_wider_than_a_row_breaks_between_characters() {
    let rows = text("see abcdefghijklmnop end", "", "", 8);
    assert_eq!(rows, ["see abcd", "efghijkl", "mnop end"]);
}

#[test]
fn wide_characters_count_their_cells() {
    let rows = text("日本語のテキスト", "", "", 6);
    assert_eq!(rows, ["日本語", "のテキ", "スト"]);
}

#[test]
fn leading_spaces_stay_and_trailing_spaces_follow_where_they_fit() {
    assert_eq!(text("    indented code", "", "", 40), ["    indented code"]);
    assert_eq!(text("typing ", "› ", "  ", 20), ["› typing "]);
    assert_eq!(text("", "› ", "  ", 20), ["› "]);
}

#[test]
fn styles_survive_wrapping() {
    use ratatui::style::{Modifier, Style};
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let rows = spans(&[Span::raw("plain then "), Span::styled("bold words", bold)], &[], &[], 15);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].last().unwrap().style, bold);
    assert_eq!(rows[1][0].content, "words");
    assert_eq!(rows[1][0].style, bold);
}
