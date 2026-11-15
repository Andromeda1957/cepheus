package io.github.cepheus.app.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Search
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TextField
import androidx.compose.material3.TextFieldDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.cepheus.app.Deck
import io.github.cepheus.app.net.Link
import io.github.cepheus.app.protocol.SearchHit
import io.github.cepheus.app.protocol.SearchSnippet
import io.github.cepheus.app.protocol.ServerMessage
import io.github.cepheus.app.protocol.Session
import io.github.cepheus.app.protocol.Tools
import kotlinx.coroutines.delay


// conversations, just like to type in "agy" and find which one i gave a
// particular instruction to about the agent deck agy agents. the sessions
// need search"). The deck reads each session's whole log; the phone adds
// the sessions whose title, branch or program holds the words.

/** How long typing must pause before the deck is asked. */
private const val SEARCH_PAUSE_MS = 300L

/** The words of a search, lowercased once each, as the deck splits them. */
fun searchWords(query: String): List<String> =
    query.split(Regex("\\s+")).filter { it.isNotEmpty() }.map(::fold).distinct()

/** Lowercase one character for one, so positions stay the original's. */
private fun fold(text: String): String = buildString(text.length) { text.forEach { append(it.lowercaseChar()) } }

/** Whether every word is in the session's title, program, branch, task or machine. */
fun titleMatches(s: Session, words: List<String>): Boolean {
    val fields = listOfNotNull(sessionTitle(s), Tools.label(s.tool), s.tool, s.branch, s.task, s.hostLabel)
        .map(::fold)
    val antigravity = s.tool == "antigravity"
    return words.isNotEmpty() && words.all { w -> fields.any { it.contains(w) } || (antigravity && "agy".startsWith(w)) }
}

/** A session a search found: by its conversation (`hit`), its title, or both. */
data class SearchResult(val session: Session, val number: Int, val hit: SearchHit?, val byTitle: Boolean)

/**
 * The sessions `query` finds, numbered as the deck numbers them. A session
 * whose title or the owner's own message holds the words comes first; one
 * where only the agent's replies or its screen do follows. Each keeps the
 * deck's order within its group. `reply` counts only when it answers these
 * same words, so a slower answer to an earlier search is never shown.
 */
fun searchResults(sessions: List<Session>, query: String, reply: ServerMessage.Search?): List<SearchResult> {
    val words = searchWords(query)
    if (words.isEmpty()) return emptyList()
    val hits = reply?.takeIf { searchWords(it.q) == words }?.hits.orEmpty().associateBy { it.id }
    val found = sessions.mapIndexedNotNull { i, s ->
        if (!s.alive) return@mapIndexedNotNull null
        val hit = hits[s.id]
        val byTitle = titleMatches(s, words)
        if (hit == null && !byTitle) null else SearchResult(s, i + 1, hit, byTitle)
    }
    val (first, rest) = found.partition { it.byTitle || (it.hit?.mine ?: 0) > 0 }
    return first + rest
}

/** Where each word is in `text`, ignoring case, merged where they meet. */
fun matchRanges(text: String, words: List<String>): List<IntRange> {
    val folded = fold(text)
    val ranges = mutableListOf<IntRange>()
    for (w in words) {
        var from = folded.indexOf(w)
        while (from >= 0) {
            ranges += from until from + w.length
            from = folded.indexOf(w, from + w.length)
        }
    }
    val merged = mutableListOf<IntRange>()
    for (r in ranges.sortedBy { it.first }) {
        val last = merged.lastOrNull()
        if (last != null && r.first <= last.last + 1) {
            merged[merged.lastIndex] = last.first..maxOf(last.last, r.last)
        } else {
            merged += r
        }
    }
    return merged
}

/** Who wrote a matching message, as its line begins. */
fun snippetWho(snippet: SearchSnippet, tool: String): String = when (snippet.role) {
    "user" -> "You"
    "screen" -> "Screen"
    else -> Tools.label(tool)
}

/** The search as it stands: the deck's last answer, whether one is coming, and why one failed. */
@Stable
class SessionSearch {
    var reply by mutableStateOf<ServerMessage.Search?>(null)
    var busy by mutableStateOf(false)
    var error by mutableStateOf<String?>(null)
    var attempt by mutableIntStateOf(0)
}

/** The search the drawer and the home list share: what is typed, and what it found. */
class Find(val query: String, val onQuery: (String) -> Unit, val search: SessionSearch)

/**
 * Asks the deck once typing pauses, again after a reconnect or Try again;
 * a new query cancels the wait for the old one.
 */
@Composable
fun rememberSessionSearch(query: String): SessionSearch {
    val search = remember { SessionSearch() }
    val link by Deck.link.collectAsStateWithLifecycle()
    val online = link == Link.Online
    val words = searchWords(query)
    LaunchedEffect(words, online, search.attempt) {
        search.error = null
        if (words.isEmpty() || !online) {
            search.busy = false
            if (words.isEmpty()) search.reply = null
            return@LaunchedEffect
        }
        search.busy = true
        delay(SEARCH_PAUSE_MS)
        Deck.search(query.trim())
            .onSuccess { search.reply = it }
            .onFailure { search.error = it.message ?: "The search failed." }
        search.busy = false
    }
    return search
}

/** The search box over a list of sessions: filled, as the list's tiles are. */
@Composable
fun SessionSearchField(query: String, onQuery: (String) -> Unit, busy: Boolean, modifier: Modifier = Modifier) {
    val focus = LocalFocusManager.current
    TextField(
        value = query,
        onValueChange = onQuery,
        placeholder = { Text("Search sessions") },
        leadingIcon = { Icon(Icons.Filled.Search, contentDescription = null) },
        trailingIcon = {
            when {
                busy -> CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
                query.isNotEmpty() -> IconButton(onClick = { onQuery("") }) {
                    Icon(Icons.Filled.Close, contentDescription = "Clear the search")
                }
            }
        },
        singleLine = true,
        keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
        keyboardActions = KeyboardActions(onSearch = { focus.clearFocus() }),
        shape = RoundedCornerShape(28.dp),
        colors = TextFieldDefaults.colors(
            focusedContainerColor = MaterialTheme.colorScheme.surfaceContainerHighest,
            unfocusedContainerColor = MaterialTheme.colorScheme.surfaceContainerHigh,
            focusedIndicatorColor = Color.Transparent,
            unfocusedIndicatorColor = Color.Transparent,
        ),
        modifier = modifier.fillMaxWidth().semantics { contentDescription = "Search sessions" },
    )
}

/**
 * What a search found, in place of the session list: each session with the
 * message that matched, the words picked out. `tile` is the cards' colour,
 * which must stand apart from the page under them.
 */
@Composable
fun SearchResultsList(
    sessions: List<Session>,
    query: String,
    search: SessionSearch,
    current: Long?,
    tile: Color,
    onSession: (Long) -> Unit,
    modifier: Modifier = Modifier,
    contentPadding: PaddingValues = PaddingValues(),
) {
    val focus = LocalFocusManager.current
    val link by Deck.link.collectAsStateWithLifecycle()
    val words = searchWords(query)
    val results = searchResults(sessions, query, search.reply)
    val answered = search.reply?.let { searchWords(it.q) == words } == true
    LazyColumn(modifier.fillMaxSize(), contentPadding = contentPadding) {
        item(key = "search-status") {
            val status = when {
                search.error != null -> null
                link != Link.Online && !answered -> "Titles only until the deck is back"
                search.busy || !answered -> "Searching conversations…"
                results.isEmpty() -> "No session mentions “${query.trim()}”."
                results.size == 1 -> "1 session"
                else -> "${results.size} sessions"
            }
            Row(
                Modifier.fillMaxWidth().padding(horizontal = 20.dp, vertical = 8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                if (status != null) {
                    Text(
                        status,
                        style = MaterialTheme.typography.labelLarge,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                } else {
                    Text(
                        search.error.orEmpty(),
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.error,
                        modifier = Modifier.weight(1f),
                    )
                    TextButton(onClick = { search.attempt++ }) { Text("Try again") }
                }
            }
        }
        items(results, key = { it.session.id }) { r ->
            SearchResultRow(r, words, selected = r.session.id == current, tile = tile) {
                focus.clearFocus()
                onSession(r.session.id)
            }
        }
    }
}

/** One session found: its title, the matching message (or its line), and how many messages matched. */
@Composable
private fun SearchResultRow(r: SearchResult, words: List<String>, selected: Boolean, tile: Color, onClick: () -> Unit) {
    val s = r.session
    val snippet = r.hit?.snippets?.firstOrNull()
    val who = snippet?.let { snippetWho(it, s.tool) }
    val details = listOfNotNull(
        sessionLine(s),
        r.hit?.count?.let { if (it == 1) "1 match" else "$it matches" },
    ).joinToString(" · ")
    val highlight = SpanStyle(color = MaterialTheme.colorScheme.primary, fontWeight = FontWeight.Bold)
    Surface(
        onClick = onClick,
        shape = RoundedCornerShape(18.dp),
        color = if (selected) MaterialTheme.colorScheme.secondaryContainer else tile,
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 12.dp, vertical = 4.dp)
            .ended(!s.alive)
            .semantics {
                contentDescription = listOfNotNull(
                    "Session ${r.number}, ${sessionTitle(s)}",
                    snippet?.let { "$who: ${it.text}" },
                    details,
                ).joinToString(", ")
            },
    ) {
        Row(Modifier.padding(horizontal = 12.dp, vertical = 12.dp), verticalAlignment = Alignment.Top) {
            Numbered(r.number) { StatusDot(s.status) }
            Column(Modifier.padding(start = 12.dp).weight(1f), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                Text(
                    highlighted(sessionTitle(s), words, highlight),
                    style = MaterialTheme.typography.titleMedium,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                if (snippet != null) {
                    Text(
                        buildAnnotatedString {
                            withStyle(SpanStyle(fontWeight = FontWeight.SemiBold)) { append("$who: ") }
                            append(highlighted(snippet.text, words, highlight))
                        },
                        style = MaterialTheme.typography.bodyMedium,
                        maxLines = 3,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
                Text(
                    details,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
    }
}

private fun highlighted(text: String, words: List<String>, style: SpanStyle): AnnotatedString = buildAnnotatedString {
    append(text)
    for (r in matchRanges(text, words)) addStyle(style, r.first, r.last + 1)
}
