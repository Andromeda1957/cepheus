package io.github.cepheus.app.ui

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.util.Base64
import android.util.LruCache
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectTransformGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.BrokenImage
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import io.github.cepheus.app.Deck
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext







private object SentImageCache {
    private const val THUMBS_BYTES = 24 shl 20
    private const val DATA_BYTES = 24 shl 20

    private val thumbs = object : LruCache<String, Bitmap>(THUMBS_BYTES) {
        override fun sizeOf(key: String, value: Bitmap) = value.byteCount
    }
    private val data = object : LruCache<String, ByteArray>(DATA_BYTES) {
        override fun sizeOf(key: String, value: ByteArray) = value.size
    }

    fun cachedThumb(id: Long, key: String, px: Int): Bitmap? = thumbs.get("$id/$key/$px")

    private suspend fun bytes(id: Long, key: String): Result<ByteArray> {
        data.get("$id/$key")?.let { return Result.success(it) }
        return Deck.image(id, key)
            .mapCatching { Base64.decode(it.data, Base64.DEFAULT) }
            .onSuccess { data.put("$id/$key", it) }
    }

    /** The picture no larger than `px` on its longer side. */
    suspend fun picture(id: Long, key: String, px: Int, keep: Boolean): Result<Bitmap> {
        cachedThumb(id, key, px)?.let { return Result.success(it) }
        return withContext(Dispatchers.Default) {
            bytes(id, key).mapCatching { decode(it, px) ?: error("This picture could not be shown.") }
        }.onSuccess { if (keep) thumbs.put("$id/$key/$px", it) }
    }

    /** Decoded at the smallest power-of-two step still at least `px` on its longer side. */
    private fun decode(bytes: ByteArray, px: Int): Bitmap? {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeByteArray(bytes, 0, bytes.size, bounds)
        val side = maxOf(bounds.outWidth, bounds.outHeight)
        if (side <= 0) return null
        var sample = 1
        while (side / (sample * 2) >= px) sample *= 2
        return BitmapFactory.decodeByteArray(bytes, 0, bytes.size, BitmapFactory.Options().apply { inSampleSize = sample })
    }
}

private sealed interface Loaded {
    data object Loading : Loaded
    data class Shown(val bitmap: Bitmap) : Loaded
    data class Failed(val why: String) : Loaded
}

private val THUMB_HEIGHT = 120.dp

/** The pictures sent with a message, right-aligned above its bubble; a tap opens one. */
@OptIn(ExperimentalLayoutApi::class)
@Composable
fun SentImages(sessionId: Long, keys: List<String>, modifier: Modifier = Modifier) {
    var viewing by remember { mutableStateOf<String?>(null) }
    FlowRow(
        modifier,
        horizontalArrangement = Arrangement.spacedBy(6.dp, Alignment.End),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        keys.forEach { key -> Thumbnail(sessionId, key) { viewing = key } }
    }
    viewing?.let { ImageViewer(sessionId, it) { viewing = null } }
}

@Composable
private fun Thumbnail(sessionId: Long, key: String, onOpen: () -> Unit) {
    // Sharp at the widest a thumbnail is drawn.
    val px = with(LocalDensity.current) { (THUMB_HEIGHT * 2).roundToPx() }
    var attempt by remember { mutableIntStateOf(0) }
    val start = SentImageCache.cachedThumb(sessionId, key, px)?.let { Loaded.Shown(it) } ?: Loaded.Loading
    val state by produceState<Loaded>(start, sessionId, key, px, attempt) {
        if (value is Loaded.Shown) return@produceState
        value = Loaded.Loading
        value = SentImageCache.picture(sessionId, key, px, keep = true)
            .fold({ Loaded.Shown(it) }, { Loaded.Failed(it.message.orEmpty()) })
    }
    val shape = RoundedCornerShape(12.dp)
    when (val s = state) {
        is Loaded.Shown -> Image(
            s.bitmap.asImageBitmap(),
            contentDescription = "Picture you sent",
            contentScale = ContentScale.Crop,
            modifier = Modifier
                .height(THUMB_HEIGHT)
                .aspectRatio((s.bitmap.width.toFloat() / s.bitmap.height).coerceIn(0.5f, 1.8f))
                .clip(shape)
                .border(1.dp, deck.border, shape)
                .clickable(onClickLabel = "Open picture", onClick = onOpen),
        )
        else -> Box(
            Modifier
                .size(THUMB_HEIGHT)
                .clip(shape)
                .background(deck.control)
                .clickable(enabled = s is Loaded.Failed, onClickLabel = "Try again") { attempt++ },
            contentAlignment = Alignment.Center,
        ) {
            if (s is Loaded.Failed) {
                Icon(Icons.Outlined.BrokenImage, contentDescription = "Picture not loaded, tap to try again", tint = deck.textTertiary)
            } else {
                CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp, color = deck.textTertiary)
            }
        }
    }
}

/** One picture over the whole screen, to pinch and pan. */
@Composable
private fun ImageViewer(sessionId: Long, key: String, onClose: () -> Unit) {
    val config = LocalConfiguration.current
    val px = with(LocalDensity.current) { maxOf(config.screenWidthDp, config.screenHeightDp).dp.roundToPx() }
    val state by produceState<Loaded>(Loaded.Loading, sessionId, key, px) {
        value = SentImageCache.picture(sessionId, key, px, keep = false)
            .fold({ Loaded.Shown(it) }, { Loaded.Failed(it.message.orEmpty()) })
    }
    var scale by remember { mutableFloatStateOf(1f) }
    var offset by remember { mutableStateOf(Offset.Zero) }
    Dialog(onDismissRequest = onClose, properties = DialogProperties(usePlatformDefaultWidth = false)) {
        Box(
            Modifier
                .fillMaxSize()
                .background(Color.Black)
                .pointerInput(Unit) {
                    detectTransformGestures { _, pan, zoom, _ ->
                        scale = (scale * zoom).coerceIn(1f, 6f)
                        offset = if (scale == 1f) Offset.Zero else offset + pan
                    }
                },
            contentAlignment = Alignment.Center,
        ) {
            when (val s = state) {
                is Loaded.Shown -> Image(
                    s.bitmap.asImageBitmap(),
                    contentDescription = "Picture you sent",
                    contentScale = ContentScale.Fit,
                    modifier = Modifier.fillMaxSize().graphicsLayer {
                        scaleX = scale
                        scaleY = scale
                        translationX = offset.x
                        translationY = offset.y
                    },
                )
                is Loaded.Failed -> Text(
                    s.why.ifEmpty { "This picture could not be loaded." },
                    color = Color.White,
                    style = MaterialTheme.typography.bodyMedium,
                    textAlign = TextAlign.Center,
                    modifier = Modifier.padding(32.dp),
                )
                Loaded.Loading -> CircularProgressIndicator(Modifier.size(28.dp), strokeWidth = 2.dp, color = Color.White)
            }
            IconButton(onClick = onClose, modifier = Modifier.align(Alignment.TopEnd).statusBarsPadding().padding(8.dp)) {
                Icon(DeckIcons.Close, contentDescription = "Close", tint = Color.White)
            }
        }
    }
}
