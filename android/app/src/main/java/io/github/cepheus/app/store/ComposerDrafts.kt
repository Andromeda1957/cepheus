package io.github.cepheus.app.store

import androidx.compose.runtime.MutableState
import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.TextFieldValue
import org.json.JSONArray

/** One connection's durable drafts and transient, screen-independent actions. */
class ComposerDrafts(private val storage: KeyValue) {
    private val fields = mutableMapOf<String, MutableState<TextFieldValue>>()
    private val operations = mutableMapOf<String, MutableState<Boolean>>()

    fun field(key: String): MutableState<TextFieldValue> = fields.getOrPut(key) {
        val saved = runCatching { JSONArray(storage.getString("draft/$key")) }.getOrNull()
        val text = saved?.optString(0).orEmpty()
        val state = mutableStateOf(TextFieldValue(text, TextRange(
            (saved?.optInt(1) ?: text.length).coerceIn(0, text.length),
            (saved?.optInt(2) ?: text.length).coerceIn(0, text.length),
        )))
        object : MutableState<TextFieldValue> {
            override var value: TextFieldValue
                get() = state.value
                set(next) {
                    state.value = next
                    storage.putString("draft/$key", if (next.text.isEmpty()) null else JSONArray()
                        .put(next.text).put(next.selection.start).put(next.selection.end).toString())
                }
            override fun component1() = value
            override fun component2(): (TextFieldValue) -> Unit = { value = it }
        }
    }

    val hasBusy: Boolean get() = operations.values.any { it.value }

    /** Copy the live edit, including its selection, without overwriting another draft. */
    fun copyMessage(from: Long, to: Long): Boolean {
        val source = field("$from/message").value
        val target = field("$to/message")
        if (target.value.text.isNotEmpty() && target.value != source) return false
        target.value = source
        return true
    }

    fun busy(key: String): MutableState<Boolean> = operations.getOrPut(key) { mutableStateOf(false) }
}
