package io.github.cepheus.app.markdown

import org.junit.Assert.assertEquals
import org.junit.Test

class MarkdownTest {
    @Test
    fun headingsParagraphsAndRules() {
        assertEquals(
            listOf(
                Block.Heading(2, listOf(Span("Plan"))),
                Block.Paragraph(listOf(Span("First line\nsecond line"))),
                Block.Rule,
                Block.Paragraph(listOf(Span("After"))),
            ),
            Markdown.parse("## Plan\n\nFirst line\nsecond line\n\n---\nAfter"),
        )
    }

    @Test
    fun fencedCodeKeepsItsTextAndLanguage() {
        assertEquals(
            listOf(
                Block.Paragraph(listOf(Span("Run:"))),
                Block.Code("bash", "cargo test\n\n  # **not bold**"),
                Block.Paragraph(listOf(Span("Done"))),
            ),
            Markdown.parse("Run:\n```bash\ncargo test\n\n  # **not bold**\n```\nDone"),
        )
        // An unclosed fence runs to the end.
        assertEquals(listOf(Block.Code("", "a\nb")), Markdown.parse("~~~\na\nb"))
    }

    @Test
    fun lists() {
        assertEquals(
            listOf(
                Block.ListBlock(
                    false,
                    listOf(
                        ListItem("•", 0, listOf(Span("one"))),
                        ListItem("•", 1, listOf(Span("nested"))),
                        ListItem("•", 0, listOf(Span("two"), Span("\n"), Span("more"))),
                    ),
                ),
                Block.ListBlock(true, listOf(ListItem("3.", 0, listOf(Span("three"))), ListItem("4.", 0, listOf(Span("four"))))),
            ),
            Markdown.parse("- one\n  * nested\n- two\n  more\n\n3. three\n4) four"),
        )
    }

    @Test
    fun inlineStyles() {
        assertEquals(
            listOf(
                Span("A "),
                Span("bold", bold = true),
                Span(", "),
                Span("italic", italic = true),
                Span(", "),
                Span("code", code = true),
                Span(", "),
                Span("gone", strike = true),
                Span(" and "),
                Span("a link", link = "https://example.com"),
                Span("."),
            ),
            Markdown.inline("A **bold**, *italic*, `code`, ~~gone~~ and [a link](https://example.com)."),
        )
    }

    @Test
    fun nestedEmphasis() {
        assertEquals(
            listOf(Span("very ", italic = true), Span("bold", bold = true, italic = true), Span(" text", italic = true)),
            Markdown.inline("*very **bold** text*"),
        )
    }

    @Test
    fun snakeCaseAndLoneMarkersStayText() {
        assertEquals(listOf(Span("call my_func_name now")), Markdown.inline("call my_func_name now"))
        assertEquals(listOf(Span("2 * 3 * 4 and a ** pair")), Markdown.inline("2 * 3 * 4 and a ** pair"))
        assertEquals(listOf(Span("an `unclosed tick")), Markdown.inline("an `unclosed tick"))
        assertEquals(listOf(Span("*literal*")), Markdown.inline("\\*literal\\*"))
    }

    @Test
    fun markersInsideCodeAreText() {
        assertEquals(listOf(Span("a **b** c", code = true)), Markdown.inline("`a **b** c`"))
    }

    /** A style's closing search once stopped at its own marker inside code or after a backslash. */
    @Test
    fun codeAndEscapedMarkersDoNotCloseAStyle() {
        assertEquals(
            listOf(Span("compute ", bold = true), Span("2**8", bold = true, code = true), Span(" first", bold = true)),
            Markdown.inline("**compute `2**8` first**"),
        )
        assertEquals(
            listOf(Span("call ", bold = true), Span("__init__", bold = true, code = true), Span(" first", bold = true)),
            Markdown.inline("__call `__init__` first__"),
        )
        assertEquals(
            listOf(Span("run ", strike = true), Span("a~~b", strike = true, code = true), Span(" now", strike = true)),
            Markdown.inline("~~run `a~~b` now~~"),
        )
        assertEquals(listOf(Span("a**b", bold = true)), Markdown.inline("**a\\**b**"))
        assertEquals(listOf(Span("a*b", italic = true)), Markdown.inline("*a\\*b*"))
        // Copy and share use the same spans.
        assertEquals("compute 2**8 first", Markdown.plain("**compute `2**8` first**"))
    }

    @Test
    fun tables() {
        assertEquals(
            listOf(
                Block.Table(
                    header = listOf(listOf(Span("Name")), listOf(Span("Status"))),
                    rows = listOf(
                        listOf(listOf(Span("api")), listOf(Span("ok", bold = true))),
                        listOf(listOf(Span("a|b")), emptyList()),
                    ),
                ),
            ),
            Markdown.parse("| Name | Status |\n|---|:---:|\n| api | **ok** |\n| a\\|b | |"),
        )
    }

    @Test
    fun quotes() {
        assertEquals(listOf(Block.Quote(listOf(Span("said\nthis")))), Markdown.parse("> said\n> this"))
    }

    @Test
    fun plainTextForNotifications() {
        assertEquals("Title\nSome bold text\n• a", Markdown.plain("# Title\nSome **bold** text\n\n- a"))
    }
}
