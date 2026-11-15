package io.github.cepheus.app.ui

import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.AttachFile
import androidx.compose.material.icons.outlined.Build
import androidx.compose.material.icons.outlined.Description
import androidx.compose.material.icons.outlined.Edit
import androidx.compose.material.icons.outlined.ErrorOutline
import androidx.compose.material.icons.outlined.Language
import androidx.compose.material.icons.outlined.Search
import androidx.compose.material.icons.outlined.SmartToy
import androidx.compose.material.icons.outlined.Terminal
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.minimumInteractiveComponentSize
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import io.github.cepheus.app.Deck
import io.github.cepheus.app.protocol.AsyncQuestion
import io.github.cepheus.app.protocol.Item
import io.github.cepheus.app.store.ChatView
import io.github.cepheus.app.store.Outbox
import io.github.cepheus.app.store.Outgoing
import kotlinx.coroutines.launch
import kotlinx.coroutines.delay
import java.util.Date
import android.text.format.DateFormat
import android.text.format.DateUtils

/** One row of the chat: an item, and for a tool call the result that answers it. */
data class ChatRow(val key: String, val item: Item, val result: Item.Result?, val outgoing: Outgoing? = null)

fun chatRows(view: ChatView, outgoing: List<Outgoing> = emptyList()): List<ChatRow> {
    val calls = view.items.filterIsInstance<Item.Tool>().map { it.call }.filter { it.isNotEmpty() }.toSet()
    val results = view.items.filterIsInstance<Item.Result>().filter { it.call.isNotEmpty() }.associateBy { it.call }
    val rows = view.items.mapIndexedNotNull { i, item ->
        when {
            // Filter presentation only; raw wire positions still drive pagination and acknowledgement.
            item is Item.Screen || item is Item.Unknown -> null
            item is Item.Result && item.call in calls -> null
            else -> ChatRow("${view.start + i}", item, (item as? Item.Tool)?.let { results[it.call] })
        }
    } + listOfNotNull(view.stream?.takeIf { it.isNotBlank() }?.let {
        ChatRow("stream", Item.Assistant(it, null), null)
    })
    // Insert optimistic rows at their original history boundary, before
    // grouping tools. Canonical positions and pagination remain untouched.
    val pending = Outbox.settle(outgoing, view, null, 0).sortedBy { it.key }
    var followingAnchor = view.total
    val positioned = pending.asReversed().map { message ->
        val anchor = if (message.generation == view.generation) message.after.coerceIn(view.start, view.total)
        else view.items.indexOfFirst { item ->
            val at = when (item) { is Item.User -> item.at; is Item.Assistant -> item.at; else -> null }
            at != null && at > (message.hostAt ?: message.createdAt)
        }.let { if (it < 0) view.total else view.start + it }
        // A reconnect or initial prompt can have an older generation's anchor.
        // Its row must stay before every later send, even without log timestamps.
        followingAnchor = minOf(followingAnchor, anchor)
        followingAnchor to message
    }.asReversed().groupBy({ it.first }, { it.second }).toSortedMap()
    return buildList {
        val left = positioned.toMutableMap()
        for (row in rows) {
            val index = row.key.toIntOrNull() ?: view.total
            left.keys.filter { it <= index }.sorted().forEach { at ->
                left.remove(at).orEmpty().forEach { add(ChatRow("out-${it.key}", Item.User(it.text, it.createdAt), null, it)) }
            }
            add(row)
        }
        left.toSortedMap().values.flatten().forEach { add(ChatRow("out-${it.key}", Item.User(it.text, it.createdAt), null, it)) }
    }
}

/** The page's side margin, as the Claude app sets its messages in from the edge. */
val ChatMargin = 20.dp

@Composable
fun ChatItem(row: ChatRow, live: Boolean, tool: String, sessionId: Long, answerEnabled: Boolean, replyActions: (@Composable () -> Unit)? = null) {
    when (val item = row.item) {
        is Item.User -> UserBubble(item.text, at = item.at, images = item.images, sessionId = sessionId)
        is Item.Assistant -> Column(Modifier.fillMaxWidth().padding(horizontal = ChatMargin, vertical = 8.dp)) {
            if (item.questions.isEmpty()) {
                SelectionContainer { MarkdownView(item.text, Modifier.fillMaxWidth()) }
            } else {
                // The gateway keeps a Markdown fallback for older phones. Keep the
                // original message above the controls without repeating that fallback.
                val fallback = item.questions.joinToString("") { question ->
                    "\n\n**${question.title}**" + question.options.joinToString("") { "\n- $it" }
                }
                val body = item.text.removeSuffix(fallback).trim()
                if (body.isNotBlank()) SelectionContainer { MarkdownView(body) }
                item.questions.forEach { question -> QuestionChoices(question, sessionId, answerEnabled) }
            }
            MessageTime(item.at, Modifier.padding(top = 4.dp))
            replyActions?.invoke()
        }
        is Item.Thinking -> Thinking(item.text)
        is Item.Tool -> ToolCall(item, row.result, live)
        is Item.Result -> ToolResultAlone(item)
        is Item.Notice -> Text(
            item.text,
            style = MaterialTheme.typography.bodySmall,
            color = deck.textTertiary,
            textAlign = TextAlign.Center,
            modifier = Modifier.fillMaxWidth().padding(horizontal = 40.dp, vertical = 8.dp),
        )
        // PTY snapshots belong exclusively to the dedicated terminal screen.
        is Item.Screen -> {}
        is Item.Unknown -> {}
    }
}

/**
 * A message this phone sent that the session's log does not show yet: shown
 * at once, as Claude's and Codex's apps show yours, until the log takes over.
 */
@Composable
fun OutgoingBubble(out: Outgoing, onSendNow: (() -> Unit)? = null, onRecover: (() -> Unit)? = null) {
    val caption = when {
        out.error != null && out.knownUnsent -> "Not sent"
        out.error != null -> "Delivery unconfirmed"
        out.submitted -> "Submitted"
        !out.delivered -> "Sending…"
        out.sendingNow -> "Sending now…"
        out.queued -> "Queued"
        out.delivery == "steered" -> "Sent to active turn"
        else -> null
    }
    if (out.files.isEmpty() && SLASH_COMMAND.matches(out.text.trim())) {
        CommandSent(out.text.trim(), caption, out.createdAt)
    } else {
        val sendNow = onSendNow?.takeIf { Outbox.offersSendNow(out) }
        UserBubble(out.text, files = out.files, caption = caption, at = out.createdAt, onSendNow = sendNow)
    }
    if (out.error != null) {
        Column(Modifier.fillMaxWidth().padding(horizontal = ChatMargin)) {
            Text(out.error, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall)
            androidx.compose.material3.TextButton(onClick = { onRecover?.invoke() }) { Text("Edit message") }
        }
    }

}

/** A message that is only a command, such as `/clear`, which no log shows as a message. */
private val SLASH_COMMAND = Regex("/[A-Za-z0-9][A-Za-z0-9_.:-]*")






@Composable
private fun CommandSent(command: String, caption: String?, at: Long? = null) {
    Column(
        Modifier.fillMaxWidth().padding(horizontal = ChatMargin, vertical = 8.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                command,
                style = MaterialTheme.typography.labelMedium.copy(fontFamily = FontFamily.Monospace),
                color = deck.textSecondary,
                modifier = Modifier.background(deck.control, RoundedCornerShape(50)).padding(horizontal = 12.dp, vertical = 6.dp),
            )
            if (caption != null) {
                Text(
                    caption,
                    style = MaterialTheme.typography.labelSmall,
                    color = deck.textTertiary,
                    modifier = Modifier.padding(start = 8.dp),
                )
            }
        }
        MessageTime(at, Modifier.padding(top = 4.dp))
    }
}

/**
 * Your message: the blue's slate tint, right-aligned, in the interface sans
 * (Claude's bubble shape), below the pictures sent with it, as the Claude
 * app shows them.
 */
@Composable
private fun UserBubble(
    text: String,
    files: List<String> = emptyList(),
    caption: String? = null,
    at: Long? = null,
    images: List<String> = emptyList(),
    sessionId: Long = 0,
    onSendNow: (() -> Unit)? = null,
) {
    var expanded by rememberSaveable { mutableStateOf(false) }
    val long = text.length > 900 || text.count { it == '\n' } > 14
    Column(
        Modifier.fillMaxWidth().padding(start = 64.dp, end = ChatMargin - 4.dp, top = 10.dp, bottom = 4.dp),
        horizontalAlignment = Alignment.End,
    ) {
        if (images.isNotEmpty()) {
            SentImages(sessionId, images, Modifier.padding(bottom = if (text.isNotBlank() || files.isNotEmpty()) 6.dp else 0.dp))
        }
        if (text.isNotBlank() || files.isNotEmpty()) Surface(
            color = deck.bubble,
            contentColor = deck.text,
            shape = RoundedCornerShape(20.dp),
            modifier = Modifier.animateContentSize(),
        ) {
            Column(Modifier.padding(horizontal = 16.dp, vertical = 11.dp)) {
                if (text.isNotBlank()) {
                    SelectionContainer {
                        Text(
                            text,
                            style = MaterialTheme.typography.bodyLarge,
                            maxLines = if (long && !expanded) 14 else Int.MAX_VALUE,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
                if (long) {
                    Text(
                        if (expanded) "Show less" else "Show more",
                        style = MaterialTheme.typography.labelMedium,
                        color = deck.textSecondary,
                        modifier = Modifier.padding(top = 6.dp).clickable { expanded = !expanded },
                    )
                }
                files.forEachIndexed { i, name ->
                    Row(
                        Modifier.padding(top = if (i == 0 && text.isNotBlank()) 8.dp else 2.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Icon(Icons.Outlined.AttachFile, null, Modifier.size(14.dp), tint = deck.textSecondary)
                        Text(
                            name,
                            style = MaterialTheme.typography.labelMedium,
                            color = deck.textSecondary,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                            modifier = Modifier.padding(start = 4.dp),
                        )
                    }
                }
            }
        }
        MessageTime(at, Modifier.padding(top = 4.dp, end = 8.dp))
        if (caption != null) {
            Row(Modifier.padding(top = 4.dp, end = 6.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(caption, style = MaterialTheme.typography.labelSmall, color = deck.textTertiary)
                if (onSendNow != null) SendNowChip(onSendNow, Modifier.padding(start = 6.dp))
            }
        }
    }
}

/** Claude Code's "ctrl+x ctrl+s to send now" under a queued message, as a tap. */
@Composable
private fun SendNowChip(onClick: () -> Unit, modifier: Modifier = Modifier) {
    val shape = RoundedCornerShape(50)
    Text(
        "Send now",
        style = MaterialTheme.typography.labelMedium,
        color = deck.accent,
        modifier = modifier
            .minimumInteractiveComponentSize()
            .clip(shape)
            .border(1.dp, deck.accent.copy(alpha = 0.55f), shape)
            .clickable(role = Role.Button, onClick = onClick)
            .padding(horizontal = 12.dp, vertical = 5.dp),
    )
}







@Composable
private fun MessageTime(at: Long?, modifier: Modifier = Modifier) {
    if (at == null || at <= 0) return
    val context = LocalContext.current
    val label = if (Format.sentToday(at)) {
        DateFormat.getTimeFormat(context).format(Date(at))
    } else {
        DateUtils.formatDateTime(context, at, DateUtils.FORMAT_SHOW_DATE or DateUtils.FORMAT_ABBREV_MONTH or DateUtils.FORMAT_SHOW_TIME)
    }
    Text(
        label,
        style = MaterialTheme.typography.labelSmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        maxLines = 1,
        softWrap = false,
        modifier = modifier,
    )
}








@Composable
fun WorkingRow(label: String, sinceSecs: Long?) {
    val transition = rememberInfiniteTransition(label = "thinking")
    val sweep by transition.animateFloat(
        initialValue = 0f,
        targetValue = 1f,
        animationSpec = infiniteRepeatable(tween(1_600, easing = LinearEasing)),
        label = "sweep",
    )
    val pulse by transition.animateFloat(
        initialValue = 0.45f,
        targetValue = 1f,
        animationSpec = infiniteRepeatable(tween(700), RepeatMode.Reverse),
        label = "pulse",
    )
    val dim = deck.textTertiary
    val bright = deck.text
    val span = with(LocalDensity.current) { 80.dp.toPx() }
    val from = -span + sweep * span * 3
    var now by remember { mutableLongStateOf(System.currentTimeMillis() / 1000) }
    if (sinceSecs != null) {
        LaunchedEffect(sinceSecs) {
            while (true) {
                now = System.currentTimeMillis() / 1000
                delay(1_000)
            }
        }
    }
    val accent = deck.accent
    Row(
        Modifier.fillMaxWidth().padding(horizontal = ChatMargin, vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            Modifier
                .size(12.dp)
                .graphicsLayer {
                    alpha = pulse
                    scaleX = 0.7f + 0.3f * pulse
                    scaleY = 0.7f + 0.3f * pulse
                }
                .background(accent, CircleShape),
        )
        Text(
            label,
            style = MaterialTheme.typography.bodyLarge.copy(
                brush = Brush.linearGradient(listOf(dim, bright, dim), start = Offset(from, 0f), end = Offset(from + span, 0f)),
            ),
            // The label wraps in what the time leaves it; the time keeps one

            modifier = Modifier.padding(start = 12.dp).weight(1f, fill = false),
        )
        if (sinceSecs != null) {
            Text(
                " · ${Format.duration((now - sinceSecs).coerceAtLeast(0))}",
                style = MaterialTheme.typography.bodyLarge,
                color = dim,
                maxLines = 1,
                softWrap = false,
            )
        }
    }
}

/** The agent's reasoning, folded under one quiet line as Claude folds its own. */
@Composable
private fun Thinking(text: String) {
    var open by rememberSaveable { mutableStateOf(false) }
    Column(Modifier.fillMaxWidth().padding(horizontal = ChatMargin - 4.dp, vertical = 2.dp).animateContentSize()) {
        Row(
            Modifier.clip(RoundedCornerShape(10.dp)).clickable { open = !open }.padding(horizontal = 4.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text("Thinking", style = MaterialTheme.typography.bodyMedium, color = deck.textSecondary)
            Icon(
                DeckIcons.ChevronDown,
                contentDescription = if (open) "Hide thinking" else "Show thinking",
                modifier = Modifier.padding(start = 4.dp).size(16.dp).graphicsLayer { rotationZ = if (open) 180f else 0f },
                tint = deck.textSecondary,
            )
        }
        if (open) {
            SelectionContainer {
                Text(
                    text,
                    style = MaterialTheme.typography.bodyMedium.copy(fontStyle = FontStyle.Italic),
                    color = deck.textSecondary,
                    modifier = Modifier.padding(start = 4.dp, end = 4.dp, bottom = 8.dp),
                )
            }
        }
    }
}

private fun toolIcon(name: String): ImageVector {
    val n = name.lowercase()
    return when {
        n == "bash" || n.contains("shell") || n.contains("exec") || n.contains("command") -> Icons.Outlined.Terminal
        n == "read" || n.contains("view") || n.contains("cat") -> Icons.Outlined.Description
        n == "edit" || n == "write" || n.contains("patch") || n.contains("multiedit") -> Icons.Outlined.Edit
        n == "grep" || n == "glob" || n.contains("search") || n.contains("find") -> Icons.Outlined.Search
        n.contains("web") || n.contains("fetch") -> Icons.Outlined.Language
        n == "task" || n == "agent" || n.contains("agent") -> Icons.Outlined.SmartToy
        else -> Icons.Outlined.Build
    }
}

/**
 * A tool the agent used: one quiet line in a hairline box, opening to its
 * input and result, as the Claude app shows a search or a file read.
 */
@Composable
private fun ToolCall(tool: Item.Tool, result: Item.Result?, live: Boolean) {
    var open by rememberSaveable { mutableStateOf(false) }
    Surface(
        color = Color.Transparent,
        contentColor = deck.text,
        shape = RoundedCornerShape(14.dp),
        border = BorderStroke(1.dp, deck.border),
        modifier = Modifier.fillMaxWidth().padding(horizontal = ChatMargin - 4.dp, vertical = 4.dp).animateContentSize(),
    ) {
        Column {
            Row(
                Modifier.fillMaxWidth().heightIn(min = 48.dp).clickable { open = !open }.padding(horizontal = 12.dp, vertical = 10.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Icon(toolIcon(tool.name), contentDescription = null, modifier = Modifier.size(18.dp), tint = deck.textSecondary)
                Row(Modifier.weight(1f), verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        tool.name,
                        style = MaterialTheme.typography.labelLarge,
                        modifier = Modifier.padding(start = 10.dp, end = 8.dp).widthIn(max = 144.dp),
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    Text(
                        tool.label?.takeIf { it.isNotBlank() } ?: tool.summary,
                        style = MaterialTheme.typography.bodyMedium,
                        color = deck.textSecondary,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                        modifier = Modifier.weight(1f),
                    )
                }
                when {
                    result?.error == true -> Icon(
                        Icons.Outlined.ErrorOutline,
                        contentDescription = "Failed",
                        tint = MaterialTheme.colorScheme.error,
                        modifier = Modifier.padding(start = 6.dp).size(16.dp),
                    )
                    result != null -> Icon(
                        DeckIcons.Check,
                        contentDescription = "Done",
                        tint = deck.textTertiary,
                        modifier = Modifier.padding(start = 6.dp).size(16.dp),
                    )
                    live -> CircularProgressIndicator(
                        Modifier.padding(start = 6.dp).size(14.dp),
                        strokeWidth = 1.5.dp,
                        color = deck.textSecondary,
                    )
                }
                IconButton(onClick = { open = !open }, modifier = Modifier.size(48.dp)) {
                    Icon(
                        DeckIcons.ChevronDown,
                        contentDescription = if (open) "Hide details" else "Show details",
                        modifier = Modifier.size(20.dp).graphicsLayer { rotationZ = if (open) 180f else 0f },
                        tint = deck.textTertiary,
                    )
                }
            }
            if (open) {
                Column(Modifier.padding(start = 12.dp, end = 12.dp, bottom = 12.dp)) {
                    if (tool.input.isNotBlank()) {
                        Label("Input")
                        Scrolling { CodeBox(tool.input, fontSize = 12) }
                    }
                    if (result != null) {
                        Label(if (result.error) "Error" else "Result")
                        Scrolling { CodeBox(result.text.ifEmpty { "(no output)" }, fontSize = 12) }
                    }
                }
            }
        }
    }
}

@Composable
private fun ToolResultAlone(result: Item.Result) {
    Column(Modifier.fillMaxWidth().padding(horizontal = ChatMargin - 4.dp, vertical = 4.dp)) {
        Label(if (result.error) "Error" else "Result")
        Scrolling { CodeBox(result.text, fontSize = 12) }
    }
}

@Composable
private fun Label(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.labelMedium,
        color = deck.textTertiary,
        modifier = Modifier.padding(top = 6.dp, bottom = 4.dp),
    )
}

@Composable
private fun Scrolling(content: @Composable () -> Unit) {
    Box(Modifier.heightIn(max = 360.dp).verticalScroll(rememberScrollState())) {
        SelectionContainer { content() }
    }
}

/** Structured choices stay interactive while Codex continues its turn. */
@Composable
private fun QuestionChoices(question: AsyncQuestion, sessionId: Long, enabled: Boolean) {
    Column(Modifier.fillMaxWidth().padding(vertical = 8.dp)) {
        MarkdownView(question.title)
        Text(if (question.answered) "Answered" else "Answer in the question card below",
            style = MaterialTheme.typography.labelMedium, color = deck.textSecondary)
    }
}
