package io.github.cepheus.app.ui

import android.content.ActivityNotFoundException
import android.content.ClipData
import android.content.Intent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Check
import androidx.compose.material.icons.outlined.MoreVert
import androidx.compose.material.icons.outlined.Share
import androidx.compose.material.icons.outlined.StopCircle
import androidx.compose.material.icons.automirrored.outlined.VolumeUp
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import io.github.cepheus.app.markdown.Markdown
import io.github.cepheus.app.protocol.Item
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** Keep references to native text blocks; join/parse only when an action is used. */
internal data class CompletedReply(val parts: List<String>, val markdown: Boolean = true) {
    fun plainText() = if (markdown) Markdown.plain(text()) else text()
    fun text() = parts.joinToString("\n\n")
}

/**
 * Native logs do not all label final answers. User boundaries seal a reply;
 * intervening work invalidates progress text. Only a settled turn seals its tail.
 */
internal fun completedReplies(rows: List<ChatRow>, settled: Boolean): Map<String, CompletedReply> {
    val replies = linkedMapOf<String, CompletedReply>()
    val pending = mutableListOf<ChatRow>()
    fun seal() {
        pending.lastOrNull()?.let { last ->
            replies[last.key] = CompletedReply(pending.map { (it.item as Item.Assistant).text })
        }
        pending.clear()
    }
    for (row in rows) {
        when (val item = row.item) {
            is Item.User -> seal()
            is Item.Assistant -> if (row.key != "stream" && item.text.isNotBlank() && item.questions.isEmpty()) pending += row else pending.clear()
            is Item.Tool, is Item.Result, is Item.Thinking -> pending.clear()
            is Item.Screen -> pending.clear()
            is Item.Notice -> if (item.text.contains("interrupt", ignoreCase = true)) pending.clear()
            is Item.Unknown -> {}
        }
    }
    if (settled) seal()
    return replies
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun ReplyActions(reply: CompletedReply, key: String, speech: ReplySpeech, report: (String) -> Unit) {
    val clipboard = LocalClipboard.current
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var more by remember(key) { mutableStateOf(false) }
    var copied by remember(key) { mutableStateOf(false) }
    var selected by remember(key) { mutableStateOf<String?>(null) }
    var busy by remember(key) { mutableStateOf(false) }
    LaunchedEffect(copied) { if (copied) { delay(1_800); copied = false } }

    fun copy(markdown: Boolean) {
        if (busy) return
        busy = true
        scope.launch {
            try {
                val text = withContext(Dispatchers.Default) {
                    if (markdown) reply.text() else reply.plainText()
                }
                clipboard.setClipEntry(ClipEntry(ClipData.newPlainText("Agent reply", text)))
                copied = true
            } catch (cancelled: CancellationException) {
                throw cancelled
            } catch (_: RuntimeException) {
                report("Could not copy this reply. Use Select text to copy a smaller part.")
            } finally { busy = false }
        }
    }

    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        ReplyAction(if (copied) Icons.Outlined.Check else DeckIcons.Copy,
            if (copied) "Reply copied" else "Copy reply", enabled = !busy) { copy(false) }
        val reading = speech.activeKey == key
        ReplyAction(if (reading) Icons.Outlined.StopCircle else Icons.AutoMirrored.Outlined.VolumeUp,
            if (reading) "Stop reading" else "Read aloud") { speech.toggle(key, reply) }
        ReplyAction(Icons.Outlined.Share, "Share reply", enabled = !busy) {
            busy = true
            scope.launch {
                try {
                    val text = withContext(Dispatchers.Default) { reply.plainText() }
                    context.startActivity(Intent.createChooser(Intent(Intent.ACTION_SEND).apply {
                        type = "text/plain"
                        putExtra(Intent.EXTRA_TEXT, text)
                    }, "Share reply"))
                } catch (cancelled: CancellationException) {
                    throw cancelled
                } catch (_: ActivityNotFoundException) {
                    report("No app is available to share this reply.")
                } catch (_: SecurityException) {
                    report("Android could not share this reply.")
                } catch (_: RuntimeException) {
                    report("Android could not share this reply. Copy or select the text instead.")
                } finally { busy = false }
            }
        }
        Column {
            ReplyAction(Icons.Outlined.MoreVert, "More reply actions") { more = true }
            DropdownMenu(expanded = more, onDismissRequest = { more = false }) {
                DropdownMenuItem(text = { Text("Copy Markdown") }, onClick = { more = false; copy(true) })
                DropdownMenuItem(text = { Text("Select text") }, onClick = {
                    more = false
                    scope.launch { selected = withContext(Dispatchers.Default) { reply.plainText() } }
                })
            }
        }
    }
    selected?.let { text ->
        ModalBottomSheet(onDismissRequest = { selected = null }, containerColor = deck.page) {
            Column(Modifier.fillMaxWidth().padding(horizontal = ChatMargin)) {
                Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                    Text("Select text", style = MaterialTheme.typography.titleLarge, color = deck.text)
                    TextButton(onClick = { copy(false) }) { Text("Copy all") }
                }
                SelectionContainer {
                    Text(text, color = deck.text, style = MaterialTheme.typography.bodyLarge,
                        modifier = Modifier.fillMaxWidth()
                            .heightIn(max = LocalConfiguration.current.screenHeightDp.dp * 0.7f)
                            .verticalScroll(rememberScrollState()).padding(vertical = 16.dp))
                }
            }
        }
    }
}

@Composable
private fun ReplyAction(icon: ImageVector, label: String, enabled: Boolean = true, action: () -> Unit) {
    IconButton(onClick = action, enabled = enabled, modifier = Modifier.size(48.dp)) {
        Icon(icon, contentDescription = label, tint = deck.textSecondary, modifier = Modifier.size(22.dp))
    }
}
