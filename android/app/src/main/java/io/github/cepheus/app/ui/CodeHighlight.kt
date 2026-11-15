package io.github.cepheus.app.ui

import java.util.Locale

internal enum class CodeTokenKind { KEYWORD, STRING, NUMBER, COMMENT }
internal data class CodeToken(val start: Int, val end: Int, val kind: CodeTokenKind)

private enum class CodeFamily { C, PYTHON, SHELL, JSON, SQL, YAML }
private val codeFamilies = buildMap {
    for (name in listOf("kotlin", "kt", "java", "javascript", "js", "jsx", "typescript", "ts", "tsx",
        "c", "cpp", "c++", "csharp", "cs", "rust", "rs", "go", "swift")) put(name, CodeFamily.C)
    for (name in listOf("python", "py")) put(name, CodeFamily.PYTHON)
    for (name in listOf("bash", "sh", "shell", "zsh")) put(name, CodeFamily.SHELL)
    put("json", CodeFamily.JSON)
    put("sql", CodeFamily.SQL)
    put("yaml", CodeFamily.YAML)
    put("yml", CodeFamily.YAML)
}
private val codeKeywords = mapOf(
    CodeFamily.C to ("abstract async await as break case catch class const continue default do else enum " +
        "export extends false final finally for from fun func function if implements import in interface " +
        "internal is let match mut new nil null object override package private protected public pub " +
        "return static struct super suspend switch this throw throws trait true try type typeof val var " +
        "void when while yield impl fn use mod").split(" ").toSet(),
    CodeFamily.PYTHON to ("and as assert async await break class continue def del elif else except " +
        "False finally for from global if import in is lambda None nonlocal not or pass raise return " +
        "True try while with yield").split(" ").toSet(),
    CodeFamily.SHELL to "if then else elif fi for while do done case esac function in export local readonly".split(" ").toSet(),
    CodeFamily.JSON to setOf("true", "false", "null"),
    CodeFamily.YAML to setOf("true", "false", "null", "yes", "no"),
    CodeFamily.SQL to ("select from where join inner outer left right on as and or not null true false " +
        "insert into values update set delete create table alter drop order by group having limit offset " +
        "distinct union all exists case when then else end").split(" ").toSet(),
)

/** Lightweight, bounded colouring. Ranges refer to the unchanged source, never evaluated code. */
internal fun codeTokens(text: String, language: String): List<CodeToken> {
    if (text.length > 50_000) return emptyList()
    val family = codeFamilies[language.trim().lowercase(Locale.ROOT).substringBefore(' ')] ?: return emptyList()
    val hashComments = family == CodeFamily.PYTHON || family == CodeFamily.SHELL || family == CodeFamily.YAML
    val backticks = family == CodeFamily.C || family == CodeFamily.SHELL
    val result = mutableListOf<CodeToken>()
    var i = 0
    while (i < text.length) {
        val start = i
        val char = text[i]
        val hashComment = char == '#' && hashComments &&
            (family == CodeFamily.PYTHON || i == 0 || text[i - 1].isWhitespace())
        val slashComment = family == CodeFamily.C && text.startsWith("//", i)
        val sqlComment = family == CodeFamily.SQL && text.startsWith("--", i)
        val kind = when {
            hashComment || slashComment || sqlComment -> {
                while (i < text.length && text[i] != '\n') i++
                CodeTokenKind.COMMENT
            }
            family == CodeFamily.C && text.startsWith("/*", i) -> {
                val end = text.indexOf("*/", i + 2)
                i = if (end < 0) text.length else end + 2
                CodeTokenKind.COMMENT
            }
            char == '"' || char == '\'' || (char == '`' && backticks) -> {
                val triple = family == CodeFamily.PYTHON && text.startsWith(char.toString().repeat(3), i)
                val delimiter = char.toString().repeat(if (triple) 3 else 1)
                i += delimiter.length
                while (i < text.length) {
                    if (text[i] == '\\') { i = (i + 2).coerceAtMost(text.length); continue }
                    if (text.startsWith(delimiter, i)) { i += delimiter.length; break }
                    i++
                }
                CodeTokenKind.STRING
            }
            char.isDigit() -> {
                i++
                if (char == '0' && i < text.length && text[i] in "xXbBoO") {
                    i++
                    while (i < text.length && (text[i].isLetterOrDigit() || text[i] == '_')) i++
                } else {
                    while (i < text.length && (text[i].isDigit() || text[i] == '_')) i++
                    if (i + 1 < text.length && text[i] == '.' && text[i + 1].isDigit()) {
                        i++
                        while (i < text.length && text[i].isDigit()) i++
                    }
                    if (i < text.length && text[i] in "eE") {
                        i++
                        if (i < text.length && text[i] in "+-") i++
                        while (i < text.length && text[i].isDigit()) i++
                    }
                }
                CodeTokenKind.NUMBER
            }
            char.isLetter() || char == '_' || char == '$' -> {
                i++
                while (i < text.length && (text[i].isLetterOrDigit() || text[i] in "_$")) i++
                val word = text.substring(start, i)
                if ((if (family == CodeFamily.SQL) word.lowercase(Locale.ROOT) else word) in codeKeywords.getValue(family))
                    CodeTokenKind.KEYWORD else null
            }
            else -> { i++; null }
        }
        if (kind != null) result += CodeToken(start, i, kind)
    }
    return result
}
