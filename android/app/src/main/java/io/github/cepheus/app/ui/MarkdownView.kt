package io.github.cepheus.app.ui

import android.content.ClipData

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.selection.DisableSelection
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material3.Icon
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.TextButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import kotlinx.coroutines.launch
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.LinkAnnotation
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.withLink
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.em
import androidx.compose.ui.unit.sp
import io.github.cepheus.app.markdown.Block
import io.github.cepheus.app.markdown.Markdown
import io.github.cepheus.app.markdown.Span

/** Markdown as the assistant wrote it: prose in a serif, code in monospace. */
@Composable
fun MarkdownView(text: String, modifier: Modifier = Modifier, prose: TextStyle = proseStyle()) {
    val blocks = remember(text) { Markdown.parse(text) }
    Column(modifier, verticalArrangement = Arrangement.spacedBy(10.dp)) {
        for (block in blocks) {
            MarkdownBlock(block, prose)
        }
    }
}

/** Claude's replies: its serif, a size up from the interface text, with open leading. */
@Composable
fun proseStyle(): TextStyle = MaterialTheme.typography.bodyLarge.copy(fontFamily = ProseFamily, fontSize = 17.sp, lineHeight = 26.sp)

@Composable
private fun MarkdownBlock(block: Block, prose: TextStyle) {
    when (block) {
        is Block.Heading -> Text(
            spans(block.spans),
            style = when (block.level) {
                1 -> MaterialTheme.typography.titleLarge
                2 -> MaterialTheme.typography.titleMedium
                else -> MaterialTheme.typography.titleSmall
            }.copy(fontWeight = FontWeight.SemiBold),
            modifier = Modifier.padding(top = 4.dp),
        )
        is Block.Paragraph -> Text(spans(block.spans), style = prose)
        is Block.Code -> CodeBox(block.text, block.language)
        is Block.ListBlock -> Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
            for (item in block.items) {
                Row(Modifier.padding(start = (item.depth * 16).dp)) {
                    Text(
                        item.marker,
                        style = prose,
                        // "10." at a large font widens its column instead of wrapping.
                        maxLines = 1,
                        softWrap = false,
                        modifier = Modifier.widthIn(min = if (block.ordered) 28.dp else 18.dp),
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Text(spans(item.spans), style = prose)
                }
            }
        }
        is Block.Quote -> Row(Modifier.height(IntrinsicSize.Min)) {
            Box(
                Modifier
                    .width(3.dp)
                    .fillMaxHeight()
                    .background(MaterialTheme.colorScheme.outlineVariant),
            )
            Text(
                spans(block.spans),
                style = prose.copy(fontStyle = FontStyle.Italic),
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(start = 12.dp),
            )
        }
        is Block.Table -> TableBox(block)
        Block.Rule -> HorizontalDivider(Modifier.padding(vertical = 4.dp))
    }
}

@Composable
fun CodeBox(text: String, language: String = "", fontSize: Int = 13, modifier: Modifier = Modifier) {
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()
    val dark = LocalDarkTheme.current
    val coloured = remember(text, language, dark) {
        buildAnnotatedString {
            append(text)
            for (token in codeTokens(text, language)) {
                addStyle(
                    SpanStyle(
                        color = codeColor(token.kind, dark),
                        fontWeight = if (token.kind == CodeTokenKind.KEYWORD) FontWeight.SemiBold else null,
                    ),
                    token.start, token.end,
                )
            }
        }
    }
    // The Claude app's code block: a quiet panel with a hairline edge, the
    // language small at its head and a copy action beside it.
    Surface(
        color = deck.code,
        contentColor = deck.text,
        shape = RoundedCornerShape(14.dp),
        border = BorderStroke(1.dp, deck.border),
        modifier = modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(start = 14.dp, end = 6.dp, bottom = 14.dp)) {
            DisableSelection {
                Row(
                    Modifier.fillMaxWidth(),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.SpaceBetween,
                ) {
                    Text(
                        language.ifBlank { "Code" },
                        style = MaterialTheme.typography.labelMedium,
                        color = deck.textTertiary,
                        maxLines = 1,
                        overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
                        modifier = Modifier.weight(1f),
                    )
                    TextButton(
                        colors = ButtonDefaults.textButtonColors(contentColor = deck.textSecondary),
                        onClick = { scope.launch { clipboard.setClipEntry(ClipEntry(ClipData.newPlainText("Code", text))) } },
                        modifier = Modifier.heightIn(min = 48.dp).semantics { contentDescription = "Copy code" },
                    ) {
                        Icon(DeckIcons.Copy, contentDescription = null, modifier = Modifier.size(16.dp))
                        Text("Copy", style = MaterialTheme.typography.labelMedium, modifier = Modifier.padding(start = 6.dp))
                    }
                }
            }
            Box(Modifier.padding(end = 8.dp).horizontalScroll(rememberScrollState())) {
                Text(
                    coloured,
                    style = TextStyle(fontFamily = FontFamily.Monospace, fontSize = fontSize.sp, lineHeight = (fontSize + 5).sp),
                    color = deck.text,
                    softWrap = false,
                )
            }
        }
    }
}

@Composable
private fun TableBox(table: Block.Table) {
    val columns = maxOf(table.header.size, table.rows.maxOfOrNull { it.size } ?: 0)
    // Characters are as wide as the phone's font scale makes them: a column
    // sized for the default font wrapped a time such as "601s" a character or
    // two a line at a large one.
    val scale = LocalDensity.current.fontScale
    val widths = (0 until columns).map { c ->
        val chars = (listOf(table.header) + table.rows).maxOf { row -> row.getOrNull(c)?.sumOf { it.text.length } ?: 0 }
        (chars * 8 * scale + 24).coerceIn(56f, 260 * scale).dp
    }
    val line = MaterialTheme.colorScheme.outlineVariant
    Box(Modifier.horizontalScroll(rememberScrollState())) {
        Column(Modifier.border(1.dp, line, RoundedCornerShape(8.dp))) {
            TableRow(table.header, widths, header = true)
            for (row in table.rows) {
                HorizontalDivider(color = line)
                TableRow(row, widths, header = false)
            }
        }
    }
}

@Composable
private fun TableRow(cells: List<List<Span>>, widths: List<androidx.compose.ui.unit.Dp>, header: Boolean) {
    Row(
        Modifier.background(if (header) MaterialTheme.colorScheme.surfaceContainer else Color.Transparent),
    ) {
        widths.forEachIndexed { i, w ->
            Text(
                spans(cells.getOrNull(i).orEmpty()),
                style = MaterialTheme.typography.bodyMedium.copy(fontWeight = if (header) FontWeight.SemiBold else null),
                modifier = Modifier.width(w).padding(horizontal = 10.dp, vertical = 8.dp),
            )
        }
    }
}

@Composable
private fun spans(spans: List<Span>): AnnotatedString {
    val codeBackground = MaterialTheme.colorScheme.surfaceContainerHighest
    val linkColor = MaterialTheme.colorScheme.primary
    return remember(spans, codeBackground, linkColor) {
        buildAnnotatedString {
            for (s in spans) {
                val style = SpanStyle(
                    fontWeight = if (s.bold) FontWeight.SemiBold else null,
                    fontStyle = if (s.italic) FontStyle.Italic else null,
                    fontFamily = if (s.code) FontFamily.Monospace else null,
                    fontSize = if (s.code) 0.88.em else androidx.compose.ui.unit.TextUnit.Unspecified,
                    background = if (s.code) codeBackground else Color.Unspecified,
                    textDecoration = if (s.strike) TextDecoration.LineThrough else null,
                )
                val link = s.link?.takeIf { it.startsWith("http://") || it.startsWith("https://") }
                if (link != null) {
                    withLink(
                        LinkAnnotation.Url(
                            link,
                            TextLinkStyles(SpanStyle(color = linkColor, textDecoration = TextDecoration.Underline)),
                        ),
                    ) { withStyle(style) { append(s.text) } }
                } else {
                    withStyle(style) { append(s.text) }
                }
            }
        }
    }
}
