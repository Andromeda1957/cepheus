package io.github.cepheus.app.ui

import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.tween
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.ChatBubbleOutline
import androidx.compose.material.icons.outlined.ExpandMore
import androidx.compose.material.icons.outlined.QuestionAnswer
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.ArrowForward
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import io.github.cepheus.app.protocol.AsyncQuestion
import kotlinx.coroutines.launch

/**
 * Questions remain reachable while the transcript grows or the owner reads
 * history. Any of them can be set aside to message instead (QuestionRecovery).
 */
@Composable
fun PendingQuestionCard(
    questions: List<AsyncQuestion>, current: AsyncQuestion, busy: Boolean, enabled: Boolean,
    modifier: Modifier = Modifier,
    maxHeight: Dp, error: String?, expanded: Boolean, onExpanded: (Boolean) -> Unit, onSelect: (String) -> Unit, onChoice: (Int) -> Unit,
) {
    val index = questions.indexOfFirst { it.id == current.id }.coerceAtLeast(0)
    val scroll = key(current.id) { rememberScrollState() }
    val scope = rememberCoroutineScope()
    Surface(
        color = deck.raised, shape = androidx.compose.foundation.shape.RoundedCornerShape(20.dp),
        modifier = modifier.fillMaxWidth().padding(horizontal = 10.dp, vertical = 4.dp).animateContentSize(tween(180)),
    ) {
        Column {
            Row(Modifier.fillMaxWidth().padding(start = 16.dp, end = 4.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(if (questions.size == 1) "Question" else "${index + 1} / ${questions.size}",
                    style = MaterialTheme.typography.titleSmall, maxLines = 1, overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f))
                if (questions.size > 1) {
                    IconButton(onClick = { onSelect(questions[(index + questions.size - 1) % questions.size].id) }, enabled = !busy) {
                        Icon(androidx.compose.material.icons.Icons.AutoMirrored.Filled.ArrowBack, "Previous question")
                    }
                    IconButton(onClick = { onSelect(questions[(index + 1) % questions.size].id) }, enabled = !busy) {
                        Icon(androidx.compose.material.icons.Icons.AutoMirrored.Filled.ArrowForward, "Next question")
                    }
                }
                if (expanded && scroll.canScrollForward) IconButton(onClick = { scope.launch { scroll.animateScrollTo(scroll.maxValue) } }) {
                    Icon(Icons.Outlined.ExpandMore, "More options")
                }
                IconButton(enabled = !busy, onClick = { onExpanded(!expanded) }) {
                    Icon(if (expanded) Icons.Outlined.ChatBubbleOutline else Icons.Outlined.QuestionAnswer,
                        if (expanded) "Message instead" else "Answer")
                }
            }
            if (expanded) {
                Column(Modifier.weight(1f, fill = false).heightIn(max = maxHeight).verticalScroll(scroll).padding(horizontal = 16.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    MarkdownView(current.title, Modifier.fillMaxWidth())
                    current.options.forEachIndexed { optionIndex, label ->
                        OutlinedButton(onClick = { onChoice(optionIndex) }, enabled = enabled && !busy && !current.submitted,
                            modifier = Modifier.fillMaxWidth().heightIn(min = 48.dp),
                            shape = androidx.compose.foundation.shape.RoundedCornerShape(12.dp)) {
                            Text(label, modifier = Modifier.fillMaxWidth())
                        }
                    }
                    Text(if (current.submitted) "Your answer is queued." else if (enabled && current.options.isNotEmpty()) "Choose an option or type your answer below." else if (enabled) "Type your answer below." else "Reconnect to answer. Your draft is kept.",
                        style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
                    if (current.submitted) Text("Answer queued for the agent. You can keep messaging.",
                        style = MaterialTheme.typography.bodySmall, color = deck.textSecondary)
                    if (busy) Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp)
                        Text("Sending answer…", style = MaterialTheme.typography.labelMedium)
                    }
                    error?.let { Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall) }
                    Spacer(Modifier.height(8.dp))
                }
            }
        }
    }
}
