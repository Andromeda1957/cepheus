package io.github.cepheus.app.service

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import io.github.cepheus.app.Deck
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull

/** A notification's option button or Reply: answered over the open connection. */
class ActionReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != ACTION_ANSWER && intent.action != ACTION_REPLY) return
        Deck.init(context)
        val pending = goAsync()
        Deck.scope.launch {
            try {
                withTimeoutOrNull(50_000) { Deck.handleAction(intent) }
            } finally {
                pending.finish()
            }
        }
    }

    companion object {
        const val ACTION_ANSWER = "io.github.cepheus.app.ANSWER"
        const val ACTION_REPLY = "io.github.cepheus.app.REPLY"
        const val EXTRA_ID = "id"
        const val EXTRA_INPUTS = "inputs"
        const val EXTRA_FP = "fp"
        const val EXTRA_CHOICE = "choice"
    }
}
