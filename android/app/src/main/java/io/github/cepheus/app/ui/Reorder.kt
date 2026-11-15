package io.github.cepheus.app.ui

import androidx.compose.foundation.gestures.detectDragGesturesAfterLongPress
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.lazy.LazyItemScope
import androidx.compose.foundation.lazy.LazyListItemInfo
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.unit.dp
import androidx.compose.ui.zIndex
import io.github.cepheus.app.Deck
import io.github.cepheus.app.protocol.Session
import io.github.cepheus.app.store.SessionOrder
import kotlinx.coroutines.flow.collectLatest

/** A held row starts the list scrolling this close to the list's visible start or end, */
private val EDGE_INSET = 16.dp

/** and scrolls it fastest this much further on, */
private val EDGE_RAMP = 72.dp

/** at this distance a second. */
private val EDGE_FASTEST = 1000.dp

/** The longest frame one scroll step covers, so a stalled frame does not jump the list. */
private const val LONGEST_FRAME_SECONDS = 0.05f

/**
 * How fast the list scrolls under a dragged row drawn from [top] to
 * [bottom], in pixels a second: toward the start (negative) once the row's
 * top comes within [inset] of the visible content's start, at 0, and toward
 * the end once its bottom comes within [inset] of [end]. The further the row
 * goes, the faster, up to [fastest] once it is [ramp] past where scrolling
 * began, as Android's own ItemTouchHelper speeds up a held item.
 */
internal fun edgeScrollSpeed(top: Float, bottom: Float, end: Float, inset: Float, ramp: Float, fastest: Float): Float {
    val up = ((inset - top) / ramp).coerceIn(0f, 1f)
    val down = ((bottom - (end - inset)) / ramp).coerceIn(0f, 1f)
    return when {
        up > down -> -fastest * up * up
        down > up -> fastest * down * down
        else -> 0f
    }
}

/**
 * A long-press drag of the rows of a LazyColumn, after Compose's own
 * LazyColumnDragAndDropDemo. Rows are found by their keys; `onMove` says
 * whether the dragged row took the other's place.
 */
class ReorderState(
    val list: LazyListState,
    private val canDrag: (key: Any) -> Boolean,
    private val onMove: (from: Any, to: Any) -> Boolean,
    private val onEnd: () -> Unit,
) {
    var dragging by mutableStateOf<Any?>(null)
        private set
    private var delta by mutableFloatStateOf(0f)
    private var startOffset by mutableIntStateOf(0)

    /** The dragged row's place when it last moved, until the list lays it out at its new one. */
    private var movedFrom: Int? = null

    private val current: LazyListItemInfo?
        get() = list.layoutInfo.visibleItemsInfo.firstOrNull { it.key == dragging }

    /** How far from its laid-out place the dragged row is drawn. */
    val offset: Float
        get() = current?.let { startOffset + delta - it.offset } ?: 0f

    fun start(at: Offset): Boolean {
        // Item offsets count from the end of the list's top content padding,
        // `at` from the list's edge. The home list pads its top by the
        // floating header, and a press on its second row once picked an
        // outside row further down, so no drag began (journey check,
        // 2026-11-01).
        val y = at.y.toInt() + list.layoutInfo.viewportStartOffset
        val item = list.layoutInfo.visibleItemsInfo.firstOrNull { y in it.offset..(it.offset + it.size) }
        if (item == null || !canDrag(item.key)) return false
        dragging = item.key
        startOffset = item.offset
        delta = 0f
        movedFrom = null
        return true
    }

    /** Follow the finger by `dy`; true when the row took a new place. */
    fun drag(dy: Float): Boolean {
        delta += dy
        return follow()
    }

    /** Give the dragged row the place of the row under its middle; true when it moved. */
    private fun follow(): Boolean {
        val row = current ?: return false
        if (movedFrom != null) {
            if (row.offset == movedFrom) return false
            movedFrom = null
        }
        // Where the row is drawn: it stays under the finger however the list scrolls.
        val middle = (startOffset + delta + row.size / 2f).toInt()
        val target = list.layoutInfo.visibleItemsInfo.firstOrNull {
            it.key != row.key && middle in it.offset..(it.offset + it.size)
        } ?: return false
        val first = list.firstVisibleItemIndex
        val firstOffset = list.firstVisibleItemScrollOffset
        if (!onMove(row.key, target.key)) return false
        movedFrom = row.offset
        // The list keeps its first row in place by key, so a dragged first
        // row would stay where it was laid out and never move again. Keep the
        // place instead, in the same layout as the move: a coroutine's
        // scrollToItem, as Compose's demo has it, could run after that layout
        // had already kept the row.
        if (row.index == first || target.index == first) list.requestScrollToItem(first, firstOffset)
        return true
    }









    internal suspend fun scrollAtEdges(inset: Float, ramp: Float, fastest: Float, onMoved: () -> Unit) {
        var last = withFrameNanos { it }
        while (true) {
            val now = withFrameNanos { it }
            val seconds = ((now - last) / 1_000_000_000f).coerceAtMost(LONGEST_FRAME_SECONDS)
            last = now
            if (follow()) onMoved()
            // A scroll now would lay the list out before it holds the move.
            if (movedFrom != null) continue
            val row = current ?: continue
            val info = list.layoutInfo
            val top = startOffset + delta
            val speed = edgeScrollSpeed(
                top = top,
                bottom = top + row.size,
                end = (info.viewportEndOffset - info.afterContentPadding).toFloat(),
                inset = inset,
                ramp = ramp,
                fastest = fastest,
            )
            if (speed != 0f) list.dispatchRawDelta(speed * seconds)
        }
    }

    fun end() {
        if (dragging != null) onEnd()
        dragging = null
        delta = 0f
        startOffset = 0
        movedFrom = null
    }
}

/** Long-press a row and drag it; the list scrolls while it is held at an edge. */
@Composable
fun Modifier.reorderable(state: ReorderState): Modifier {
    val haptics = LocalHapticFeedback.current
    val density = LocalDensity.current
    LaunchedEffect(state, density) {
        val inset = with(density) { EDGE_INSET.toPx() }
        val ramp = with(density) { EDGE_RAMP.toPx() }
        val fastest = with(density) { EDGE_FASTEST.toPx() }
        snapshotFlow { state.dragging != null }.collectLatest { dragging ->
            if (dragging) {
                state.scrollAtEdges(inset, ramp, fastest) { haptics.performHapticFeedback(HapticFeedbackType.TextHandleMove) }
            }
        }
    }
    return pointerInput(state) {
        detectDragGesturesAfterLongPress(
            onDragStart = { at -> if (state.start(at)) haptics.performHapticFeedback(HapticFeedbackType.LongPress) },
            onDrag = { change, amount ->
                if (state.dragging != null) {
                    change.consume()
                    if (state.drag(amount.y)) haptics.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                }
            },
            onDragEnd = { state.end() },
            onDragCancel = { state.end() },
        )
    }
}

/** A row that lifts while dragged and slides when another passes it. */
@Composable
fun LazyItemScope.ReorderableRow(state: ReorderState, key: Any, content: @Composable () -> Unit) {
    val lifted = state.dragging == key
    // The row lifts by growing a little: a square shadow and fill behind the
    // rounded cards showed their corners.
    val modifier = if (lifted) {
        Modifier.zIndex(1f).graphicsLayer {
            translationY = state.offset
            scaleX = 1.03f
            scaleY = 1.03f
        }
    } else {
        Modifier.animateItem(fadeInSpec = null, fadeOutSpec = null)
    }
    Box(modifier) { content() }
}

/**
 * The deck's sessions as dragging shows them, and the drag. A drop keeps
 * the order with [Deck.reorder], which the deck's list follows.
 */
@Composable
fun rememberSessionReorder(sessions: List<Session>, list: LazyListState): Pair<List<Session>, ReorderState> {
    var dragged by remember { mutableStateOf<List<Long>?>(null) }
    val shown = dragged?.let { SessionOrder.arrange(sessions, it) } ?: sessions
    val latest by rememberUpdatedState(shown)
    val state = remember(list) {
        ReorderState(
            list = list,
            canDrag = { it is Long },
            onMove = { from, to ->
                if (from !is Long || to !is Long) return@ReorderState false
                val now = latest
                val moved = SessionOrder.move(now, from, now.indexOfFirst { it.id == to })
                if (moved == now) return@ReorderState false
                dragged = moved.map { it.id }
                true
            },
            onEnd = {
                dragged?.let(Deck::reorder)
                dragged = null
            },
        )
    }
    return shown to state
}
