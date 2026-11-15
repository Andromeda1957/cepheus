package io.github.cepheus.app.ui

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import io.github.cepheus.app.protocol.OutsideAgent
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.launch

/** Shell-owned handoff state survives removal of the outside row that initiated it. */
class MoveInRequest(
    private val scope: CoroutineScope,
    private val move: suspend (OutsideAgent) -> Result<Long>,
    private val opened: (Long) -> Unit,
) {
    var agent by mutableStateOf<OutsideAgent?>(null)
        private set
    var busy by mutableStateOf(false)
        private set
    var error by mutableStateOf<String?>(null)
        private set

    fun select(outside: OutsideAgent) { if (!busy) { agent = outside; error = null } }
    fun dismiss() { if (!busy) agent = null }
    fun confirm() {
        val outside = agent ?: return
        if (busy) return
        busy = true
        error = null
        scope.launch {
            try {
                move(outside).fold(
                    onSuccess = { agent = null; opened(it) },
                    onFailure = { error = it.message ?: "Could not move this session into the deck." },
                )
            } finally { busy = false }
        }
    }
}
