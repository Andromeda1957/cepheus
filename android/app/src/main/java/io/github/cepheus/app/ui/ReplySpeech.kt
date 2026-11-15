package io.github.cepheus.app.ui

import android.content.Context
import android.media.AudioAttributes
import android.media.AudioFocusRequest
import android.media.AudioManager
import android.os.Handler
import android.os.Looper
import android.speech.tts.TextToSpeech
import android.speech.tts.UtteranceProgressListener
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.cepheus.app.Deck
import java.util.Locale
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** Keep every character and avoid breaking a UTF-16 surrogate pair. */
internal fun speechParts(text: String, limit: Int): List<String> {
    require(limit >= 2)
    val parts = mutableListOf<String>()
    var start = 0
    while (start < text.length) {
        var end = minOf(start + limit, text.length)
        if (end < text.length && text[end - 1].isHighSurrogate() && text[end].isLowSurrogate()) end--
        if (end < text.length) {
            val floor = start + limit / 2
            var space = end - 1
            while (space >= floor && text[space] != '\n' && text[space] != ' ') space--
            if (space >= floor) end = space + 1
        }
        parts += text.substring(start, end)
        start = end
    }
    return parts
}

/** One foreground-only, lazily created engine for a chat, never one per row. */
internal class ReplySpeech(
    private val context: Context,
    private val scope: CoroutineScope,
    private val report: (String) -> Unit,
) {
    var activeKey by mutableStateOf<String?>(null)
        private set
    private val main = Handler(Looper.getMainLooper())
    private val audio = context.getSystemService(AudioManager::class.java)
    private val attributes = AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_MEDIA)
        .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH).build()
    private var focus: AudioFocusRequest? = null
    private var engine: TextToSpeech? = null
    private var ready = false
    private var engineEpoch = 0
    private var request = 0
    private var currentId: String? = null
    private var parts = emptyList<String>()
    private var index = 0
    private var preparing: Job? = null
    private var closed = false

    fun toggle(key: String, reply: CompletedReply) {
        if (activeKey == key) { stop(); return }
        stop()
        if (closed) return
        activeKey = key
        val token = request
        preparing = scope.launch {
            val chunks = withContext(Dispatchers.Default) {
                val plain = reply.plainText()
                if (plain.isBlank()) emptyList() else speechParts(plain, TextToSpeech.getMaxSpeechInputLength())
            }
            if (closed || token != request || activeKey == null) return@launch
            if (chunks.isEmpty()) { fail("There is no text to read aloud."); return@launch }
            parts = chunks
            if (engine == null) {
                val epoch = ++engineEpoch
                // Android may invoke onInit before the constructor returns.
                engine = TextToSpeech(context) { status -> main.post {
                    if (!closed && epoch == engineEpoch) initialized(status, epoch)
                } }
            } else if (ready) begin()
            val epoch = engineEpoch
            main.postDelayed({
                if (!closed && epoch == engineEpoch && !ready && activeKey != null) {
                    fail("Read aloud could not start. Check Android's text-to-speech settings.")
                }
            }, 15_000)
        }
    }

    private fun initialized(status: Int, epoch: Int) {
        val current = engine ?: return
        if (status != TextToSpeech.SUCCESS) {
            fail("Read aloud is unavailable. Check Android's text-to-speech settings.")
            return
        }
        current.setAudioAttributes(attributes)
        current.setOnUtteranceProgressListener(object : UtteranceProgressListener() {
            override fun onStart(id: String?) {}
            override fun onDone(id: String?) { main.post {
                if (epoch == engineEpoch && id != null && id == currentId) {
                    index++
                    if (index < parts.size) speak() else stop()
                }
            } }
            @Suppress("OVERRIDE_DEPRECATION")
            override fun onError(id: String?) = error(id)
            override fun onError(id: String?, code: Int) = error(id)
            private fun error(id: String?) { main.post {
                if (epoch == engineEpoch && id != null && id == currentId) {
                    fail("Read aloud stopped. Check the selected voice in Android's text-to-speech settings.")
                }
            } }
            override fun onStop(id: String?, interrupted: Boolean) { main.post {
                if (epoch == engineEpoch && id != null && id == currentId) stop()
            } }
        })
        ready = true
        if (activeKey != null) begin()
    }

    private fun begin() {
        if (activeKey == null || index !in parts.indices) return
        val language = engine?.setLanguage(Locale.getDefault())
        if (language == TextToSpeech.LANG_MISSING_DATA || language == TextToSpeech.LANG_NOT_SUPPORTED) {
            fail("Install a voice for your language in Android's text-to-speech settings.")
            return
        }
        val token = request
        val nextFocus = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN_TRANSIENT)
            .setAudioAttributes(attributes).setWillPauseWhenDucked(true)
            .setOnAudioFocusChangeListener({
                if (it < 0 && token == request && activeKey != null) stop()
            }, main).build()
        focus = nextFocus
        if (audio?.requestAudioFocus(nextFocus) != AudioManager.AUDIOFOCUS_REQUEST_GRANTED) {
            fail("Read aloud could not start. Try again when audio is available.")
            return
        }
        speak()
    }

    private fun speak() {
        if (activeKey == null || index !in parts.indices) return
        val id = "$request/$index"
        currentId = id
        if (engine?.speak(parts[index], TextToSpeech.QUEUE_FLUSH, null, id) != TextToSpeech.SUCCESS) {
            fail("Read aloud could not start. Check Android's text-to-speech settings.")
        }
    }

    fun stop() {
        request++
        activeKey = null
        currentId = null
        parts = emptyList()
        index = 0
        preparing?.cancel()
        preparing = null
        engine?.stop()
        val previousFocus = focus
        focus = null
        if (previousFocus != null) audio?.abandonAudioFocusRequest(previousFocus)
    }

    fun release() {
        stop()
        engineEpoch++
        ready = false
        engine?.shutdown()
        engine = null
    }

    fun close() { closed = true; release() }
    private fun fail(message: String) { release(); report(message) }
}

@Composable
internal fun rememberReplySpeech(sessionId: Long, report: (String) -> Unit): ReplySpeech {
    val context = androidx.compose.ui.platform.LocalContext.current.applicationContext
    val owner = LocalLifecycleOwner.current
    val scope = rememberCoroutineScope()
    val latestReport by rememberUpdatedState(report)
    val connection by Deck.connection.collectAsStateWithLifecycle()
    val speech = remember(sessionId, connection?.id) { ReplySpeech(context, scope) { latestReport(it) } }
    DisposableEffect(speech, owner) {
        val observer = LifecycleEventObserver { _, event -> if (event == Lifecycle.Event.ON_STOP) speech.release() }
        owner.lifecycle.addObserver(observer)
        onDispose { owner.lifecycle.removeObserver(observer); speech.close() }
    }
    return speech
}
