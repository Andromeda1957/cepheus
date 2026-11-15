package io.github.cepheus.app.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class CodeHighlightTest {
    private fun marked(text: String, language: String, kind: CodeTokenKind) =
        codeTokens(text, language).filter { it.kind == kind }.map { text.substring(it.start, it.end) }

    @Test fun pythonStringsProtectCommentMarkersAndPreserveTheOriginalText() {
        val text = "def greet():\n    return \"# still a string\" + str(42) # note"
        assertEquals(listOf("def", "return"), marked(text, "python", CodeTokenKind.KEYWORD))
        assertEquals(listOf("\"# still a string\""), marked(text, "py", CodeTokenKind.STRING))
        assertEquals(listOf("# note"), marked(text, "python", CodeTokenKind.COMMENT))
        assertEquals(listOf("42"), marked(text, "python", CodeTokenKind.NUMBER))
    }

    @Test fun multilineCommentsAndEscapedQuotesDoNotLeakKeywords() {
        val text = "const value = \"say \\\"hello\\\" // text\"; /* return 42\n */ return true;"
        assertEquals(listOf("const", "return", "true"), marked(text, "javascript", CodeTokenKind.KEYWORD))
        assertEquals(listOf("/* return 42\n */"), marked(text, "js", CodeTokenKind.COMMENT))
        assertTrue(marked(text, "javascript", CodeTokenKind.NUMBER).isEmpty())
    }

    @Test fun jsonAndSqlHaveAppropriateLiteralAndCommentRules() {
        val json = "{\"active\": true, \"count\": 12.5}"
        assertEquals(listOf("true"), marked(json, "json", CodeTokenKind.KEYWORD))
        assertEquals(listOf("12.5"), marked(json, "json", CodeTokenKind.NUMBER))
        val sql = "SELECT count FROM logs -- where false\nWHERE count > 0"
        assertEquals(listOf("SELECT", "FROM", "WHERE"), marked(sql, "sql", CodeTokenKind.KEYWORD))
        assertEquals(listOf("-- where false"), marked(sql, "sql", CodeTokenKind.COMMENT))
    }

    @Test fun unknownOrOversizedBlocksStayPlainAndRangesNeverAlterSource() {
        assertTrue(codeTokens("class A {}", "unrecognised").isEmpty())
        assertTrue(codeTokens("return ".repeat(8000), "python").isEmpty())
        val text = "val title = \"caf\u00e9\"\n// keep whitespace\nreturn 2"
        val tokens = codeTokens(text, "kotlin")
        assertTrue(tokens.isNotEmpty())
        assertTrue(tokens.all { it.start >= 0 && it.end <= text.length && it.start < it.end })
        assertTrue(tokens.zipWithNext().all { (a, b) -> a.end <= b.start })
        assertEquals(listOf("\"caf\u00e9\""), marked(text, "kt", CodeTokenKind.STRING))
    }
}
