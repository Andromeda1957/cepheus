package io.github.cepheus.app.markdown

/**
 * The Markdown the agents write, as blocks of styled spans: headings,
 * paragraphs, fenced code, lists, quotes, tables and rules. Anything it does
 * not recognize stays as text.
 */
sealed interface Block {
    data class Heading(val level: Int, val spans: List<Span>) : Block
    data class Paragraph(val spans: List<Span>) : Block
    data class Code(val language: String, val text: String) : Block
    data class ListBlock(val ordered: Boolean, val items: List<ListItem>) : Block
    data class Quote(val spans: List<Span>) : Block
    data class Table(val header: List<List<Span>>, val rows: List<List<List<Span>>>) : Block
    data object Rule : Block
}

/** A list entry: its marker ("3." when ordered), nesting depth and text. */
data class ListItem(val marker: String, val depth: Int, val spans: List<Span>)

data class Span(
    val text: String,
    val bold: Boolean = false,
    val italic: Boolean = false,
    val code: Boolean = false,
    val strike: Boolean = false,
    val link: String? = null,
)

object Markdown {
    private val FENCE = Regex("^ {0,3}(`{3,}|~{3,})\\s*([^`\\s]*).*$")
    private val HEADING = Regex("^ {0,3}(#{1,6})\\s+(.*?)\\s*#*\\s*$")
    private val RULE = Regex("^ {0,3}([-*_])(\\s*\\1){2,}\\s*$")
    private val BULLET = Regex("^(\\s*)[-*+]\\s+(.*)$")
    private val ORDERED = Regex("^(\\s*)(\\d{1,9})[.)]\\s+(.*)$")
    private val QUOTE = Regex("^ {0,3}>\\s?(.*)$")
    private val TABLE_RULE = Regex("^\\s*\\|?\\s*:?-{1,}:?\\s*(\\|\\s*:?-{1,}:?\\s*)*\\|?\\s*$")

    fun parse(text: String): List<Block> {
        val lines = text.replace("\r\n", "\n").split('\n')
        val blocks = mutableListOf<Block>()
        val paragraph = mutableListOf<String>()
        var i = 0

        fun flush() {
            if (paragraph.isNotEmpty()) {
                blocks += Block.Paragraph(inline(paragraph.joinToString("\n")))
                paragraph.clear()
            }
        }

        while (i < lines.size) {
            val line = lines[i]
            val fence = FENCE.matchEntire(line)
            if (fence != null) {
                flush()
                val marker = fence.groupValues[1]
                val body = mutableListOf<String>()
                i++
                while (i < lines.size && !isClosingFence(lines[i], marker)) {
                    body += lines[i]
                    i++
                }
                blocks += Block.Code(fence.groupValues[2], body.joinToString("\n"))
                i++
                continue
            }
            if (line.isBlank()) {
                flush()
                i++
                continue
            }
            val heading = HEADING.matchEntire(line)
            if (heading != null) {
                flush()
                blocks += Block.Heading(heading.groupValues[1].length, inline(heading.groupValues[2]))
                i++
                continue
            }
            if (RULE.matches(line)) {
                flush()
                blocks += Block.Rule
                i++
                continue
            }
            if (isTableStart(lines, i)) {
                flush()
                val header = cells(line)
                i += 2
                val rows = mutableListOf<List<List<Span>>>()
                while (i < lines.size && lines[i].contains('|') && lines[i].isNotBlank()) {
                    rows += cells(lines[i])
                    i++
                }
                blocks += Block.Table(header, rows)
                continue
            }
            if (QUOTE.matches(line)) {
                flush()
                val quoted = mutableListOf<String>()
                while (i < lines.size) {
                    val m = QUOTE.matchEntire(lines[i]) ?: break
                    quoted += m.groupValues[1]
                    i++
                }
                blocks += Block.Quote(inline(quoted.joinToString("\n")))
                continue
            }
            if (listMarker(line) != null) {
                flush()
                val ordered = ORDERED.matches(line)
                val items = mutableListOf<ListItem>()
                while (i < lines.size) {
                    val current = lines[i]
                    val m = listMarker(current)
                    if (m != null) {
                        val (indent, marker, body) = m
                        items += ListItem(marker, indent / 2, inline(body))
                        i++
                    } else if (current.isNotBlank() && current.startsWith(" ") && items.isNotEmpty()) {
                        // A continuation line of the entry above.
                        val last = items.removeAt(items.lastIndex)
                        items += last.copy(spans = last.spans + Span("\n") + inline(current.trim()))
                        i++
                    } else {
                        break
                    }
                }
                blocks += Block.ListBlock(ordered, items)
                continue
            }
            paragraph += line
            i++
        }
        flush()
        return blocks
    }

    private fun isClosingFence(line: String, marker: String): Boolean {
        val t = line.trim()
        return t.length >= marker.length && t.all { it == marker[0] }
    }

    private fun isTableStart(lines: List<String>, i: Int): Boolean =
        i + 1 < lines.size && lines[i].contains('|') && lines[i + 1].contains('-') &&
            TABLE_RULE.matches(lines[i + 1])

    private fun cells(line: String): List<List<Span>> {
        var t = line.trim()
        if (t.startsWith("|")) t = t.substring(1)
        if (t.endsWith("|") && !t.endsWith("\\|")) t = t.substring(0, t.length - 1)
        val out = mutableListOf<String>()
        val cell = StringBuilder()
        var j = 0
        while (j < t.length) {
            val c = t[j]
            if (c == '\\' && j + 1 < t.length && t[j + 1] == '|') {
                cell.append('|')
                j += 2
                continue
            }
            if (c == '|') {
                out += cell.toString()
                cell.clear()
            } else {
                cell.append(c)
            }
            j++
        }
        out += cell.toString()
        return out.map { inline(it.trim()) }
    }

    /** Indent, marker and text of a list line. */
    private fun listMarker(line: String): Triple<Int, String, String>? {
        if (RULE.matches(line)) return null
        BULLET.matchEntire(line)?.let { return Triple(width(it.groupValues[1]), "•", it.groupValues[2]) }
        ORDERED.matchEntire(line)?.let { return Triple(width(it.groupValues[1]), it.groupValues[2] + ".", it.groupValues[3]) }
        return null
    }

    private fun width(indent: String) = indent.sumOf { if (it == '\t') 4 else 1 }

    /** Inline styles: **bold**, *italic*, `code`, ~~strike~~ and [links](url). */
    fun inline(text: String): List<Span> = merge(InlineParser(text).parse(Style()))

    private data class Style(
        val bold: Boolean = false,
        val italic: Boolean = false,
        val strike: Boolean = false,
        val link: String? = null,
    )

    private class InlineParser(private val s: String) {
        fun parse(style: Style, from: Int = 0, to: Int = s.length): List<Span> {
            val out = mutableListOf<Span>()
            val plain = StringBuilder()
            fun emit() {
                if (plain.isNotEmpty()) {
                    out += Span(plain.toString(), style.bold, style.italic, false, style.strike, style.link)
                    plain.clear()
                }
            }
            var i = from
            while (i < to) {
                val c = s[i]
                if (c == '\\' && i + 1 < to && s[i + 1] in ESCAPABLE) {
                    plain.append(s[i + 1])
                    i += 2
                    continue
                }
                if (c == '`') {
                    val run = runLength(i, to, '`')
                    val close = s.indexOf("`".repeat(run), i + run).takeIf { it in 0 until to }
                    if (close != null && close > i + run - 1) {
                        emit()
                        val code = s.substring(i + run, close).let {
                            if (it.length > 2 && it.startsWith(' ') && it.endsWith(' ')) it.substring(1, it.length - 1) else it
                        }
                        out += Span(code, style.bold, style.italic, true, style.strike, style.link)
                        i = close + run
                        continue
                    }
                    plain.append("`".repeat(run))
                    i += run
                    continue
                }
                if ((c == '*' || c == '_') && i + 1 < to && s[i + 1] == c && opens(i, 2, to)) {
                    val close = closing(i + 2, to, "$c$c")
                    if (close != null) {
                        emit()
                        out += parse(style.copy(bold = true), i + 2, close)
                        i = close + 2
                        continue
                    }
                }
                if ((c == '*' || c == '_') && opens(i, 1, to)) {
                    val close = closingSingle(i + 1, to, c)
                    if (close != null) {
                        emit()
                        out += parse(style.copy(italic = true), i + 1, close)
                        i = close + 1
                        continue
                    }
                }
                if (c == '~' && i + 1 < to && s[i + 1] == '~') {
                    val close = closing(i + 2, to, "~~")
                    if (close != null && close > i + 2) {
                        emit()
                        out += parse(style.copy(strike = true), i + 2, close)
                        i = close + 2
                        continue
                    }
                }
                if (c == '[') {
                    val link = link(i, to)
                    if (link != null) {
                        emit()
                        val (textEnd, url, end) = link
                        out += parse(style.copy(link = url), i + 1, textEnd)
                        i = end
                        continue
                    }
                }
                plain.append(c)
                i++
            }
            emit()
            return out
        }

        private fun runLength(i: Int, to: Int, c: Char): Int {
            var j = i
            while (j < to && s[j] == c) j++
            return j - i
        }

        /** An opening marker is followed by text, and `_` must not sit inside a word. */
        private fun opens(i: Int, width: Int, to: Int): Boolean {
            val next = i + width
            if (next >= to || s[next].isWhitespace()) return false
            if (s[i] == '_' && i > 0 && s[i - 1].isLetterOrDigit()) return false
            return true
        }

        /**
         * The index after the escaped character or the complete code span
         * starting at `j`, or null when neither starts there. Every closing
         * search steps over these as [parse] reads them, so a marker inside
         * `code` or after a backslash never ends a style: "**see `a**b`**"
         * once closed at the code's own `**`.
         */
        private fun literalEnd(j: Int, to: Int): Int? {
            if (s[j] == '\\' && j + 1 < to && s[j + 1] in ESCAPABLE) return j + 2
            if (s[j] != '`') return null
            val run = runLength(j, to, '`')
            val close = s.indexOf("`".repeat(run), j + run)
            return if (close in 0 until to) close + run else j + run
        }

        private fun closing(from: Int, to: Int, marker: String): Int? {
            var j = from
            while (j < to) {
                val skip = literalEnd(j, to)
                if (skip != null) {
                    j = skip
                    continue
                }
                if (s.startsWith(marker, j) && j > from && !s[j - 1].isWhitespace() &&
                    !(marker[0] == '_' && j + 2 < s.length && s[j + 2].isLetterOrDigit())
                ) {
                    return j
                }
                j++
            }
            return null
        }

        private fun closingSingle(from: Int, to: Int, c: Char): Int? {
            var j = from
            while (j < to) {
                val skip = literalEnd(j, to)
                if (skip != null) {
                    j = skip
                    continue
                }
                if (s[j] == c) {
                    val doubled = (j + 1 < to && s[j + 1] == c)
                    if (doubled) {
                        j += 2
                        continue
                    }
                    if (j > from && !s[j - 1].isWhitespace() &&
                        !(c == '_' && j + 1 < s.length && s[j + 1].isLetterOrDigit())
                    ) {
                        return j
                    }
                }
                j++
            }
            return null
        }

        /** `[text](url)`: the end of the text, the url and the index after `)`. */
        private fun link(i: Int, to: Int): Triple<Int, String, Int>? {
            var depth = 0
            var j = i
            while (j < to) {
                when (s[j]) {
                    '[' -> depth++
                    ']' -> {
                        depth--
                        if (depth == 0) break
                    }
                }
                j++
            }
            if (j >= to || j + 1 >= to || s[j + 1] != '(') return null
            val close = s.indexOf(')', j + 2)
            if (close !in 0 until to) return null
            val url = s.substring(j + 2, close).trim().substringBefore(' ')
            if (url.isEmpty() || j == i + 1) return null
            return Triple(j, url, close + 1)
        }

        companion object {
            const val ESCAPABLE = "\\`*_{}[]()#+-.!|~>"
        }
    }

    /** Joins neighbours with the same style. */
    private fun merge(spans: List<Span>): List<Span> {
        val out = mutableListOf<Span>()
        for (span in spans) {
            val last = out.lastOrNull()
            if (last != null && last.copy(text = "") == span.copy(text = "")) {
                out[out.lastIndex] = last.copy(text = last.text + span.text)
            } else if (span.text.isNotEmpty()) {
                out += span
            }
        }
        return out
    }

    /** The text without its styling, as a notification shows it. */
    fun plain(text: String): String = parse(text).joinToString("\n") { block ->
        when (block) {
            is Block.Heading -> block.spans.joinToString("") { it.text }
            is Block.Paragraph -> block.spans.joinToString("") { it.text }
            is Block.Code -> block.text
            is Block.ListBlock -> block.items.joinToString("\n") { "${it.marker} " + it.spans.joinToString("") { s -> s.text } }
            is Block.Quote -> block.spans.joinToString("") { it.text }
            is Block.Table -> (listOf(block.header) + block.rows).joinToString("\n") { row ->
                row.joinToString(" | ") { cell -> cell.joinToString("") { it.text } }
            }
            Block.Rule -> "—"
        }
    }
}
