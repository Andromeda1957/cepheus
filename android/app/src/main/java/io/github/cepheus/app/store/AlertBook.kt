package io.github.cepheus.app.store

import io.github.cepheus.app.protocol.ServerMessage

/**
 * What the phone has already been told, so a reconnect notifies nothing twice.
 *
 * The gateway numbers alerts (`seq`) within an `epoch`, one per gateway run.
 * The phone signs in with the last of each it holds and is sent the alerts
 * after it, or every kept alert (up to 100) when the epoch changed. Replayed
 * alerts older than [FRESH_MS] are recorded but not notified, and a waiting
 * prompt is notified once per fingerprint.
 */
class AlertBook(private val kv: KeyValue) {
    val lastSeq: Long? get() = kv.getLong(LAST_SEQ)
    val epoch: String? get() = kv.getString(EPOCH)

    /** The gateway's `hello`: a new epoch starts its numbering again. */
    @Synchronized
    fun hello(epoch: String) {
        if (epoch != this.epoch) {
            kv.putString(EPOCH, epoch)
            kv.putLong(LAST_SEQ, null)
        }
    }

    /** Records the alert; true when it is new and recent enough to notify. */
    @Synchronized
    fun accept(alert: ServerMessage.Alert, nowMs: Long): Boolean {
        if (alert.epoch != epoch) {
            kv.putString(EPOCH, alert.epoch)
            kv.putLong(LAST_SEQ, null)
        }
        val last = lastSeq
        if (last != null && alert.seq <= last) return false
        kv.putLong(LAST_SEQ, alert.seq)
        return nowMs - alert.at <= FRESH_MS
    }

    /** True the first time session `id` is seen waiting on prompt `fp`. */
    @Synchronized
    fun promptIsNew(id: Long, fp: String): Boolean {
        if (kv.getString(promptKey(id)) == fp) return false
        kv.putString(promptKey(id), fp)
        return true
    }

    /** The session's prompt went; a later one with the same text notifies again. */
    @Synchronized
    fun promptGone(id: Long) = kv.putString(promptKey(id), null)

    private fun promptKey(id: Long) = "prompt.$id"

    companion object {
        /** Alerts older than this when they arrive are history, not news. */
        const val FRESH_MS = 30 * 60 * 1000L
        private const val LAST_SEQ = "alerts.last_seq"
        private const val EPOCH = "alerts.epoch"
    }
}
