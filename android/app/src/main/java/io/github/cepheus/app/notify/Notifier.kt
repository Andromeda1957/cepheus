package io.github.cepheus.app.notify

import android.Manifest
import android.annotation.SuppressLint
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.RemoteInput
import androidx.core.content.ContextCompat
import io.github.cepheus.app.R
import io.github.cepheus.app.protocol.DeckState
import io.github.cepheus.app.protocol.WaitingPrompt
import io.github.cepheus.app.service.ActionReceiver
import io.github.cepheus.app.ui.MainActivity
import io.github.cepheus.app.store.Connections

/**
 * The app's notifications. Each session has at most one "needs you" and one
 * "finished" notification, tagged by kind, connection and full session identity.
 */
object Notifier {
    const val CH_FINISHED = "finished"
    const val CH_NEEDS_YOU = "needs_you"
    const val CH_ERROR = "error"
    const val CH_CONNECTION = "connection"

    const val FOREGROUND_ID = 1
    private const val TAG_NEEDS_YOU = "needs_you"
    private const val TAG_FINISHED = "finished"
    private const val TAG_ERROR = "error"
    private const val TAG_FAILED = "failed"

    const val EXTRA_SESSION = "session"
    const val EXTRA_SCREEN = "screen"
    const val EXTRA_CONNECTION = "connection"
    const val REPLY_KEY = "reply"

    fun createChannels(context: Context) {
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        // Retire cards left by an older APK, across all saved connections.
        manager.activeNotifications.filter { NotificationIdentity.retired(it.tag, it.id) }
            .forEach { manager.cancel(it.tag, it.id) }
        manager.createNotificationChannels(
            listOf(
                NotificationChannel(CH_NEEDS_YOU, "Needs you", NotificationManager.IMPORTANCE_HIGH).apply {
                    description = "A session asks a question or waits for approval."
                },
                NotificationChannel(CH_FINISHED, "Finished", NotificationManager.IMPORTANCE_DEFAULT).apply {
                    description = "A session finished its turn."
                },
                NotificationChannel(CH_ERROR, "Errors", NotificationManager.IMPORTANCE_DEFAULT).apply {
                    description = "The deck reported an error."
                },
                NotificationChannel(CH_CONNECTION, "Connection", NotificationManager.IMPORTANCE_LOW).apply {
                    description = "The notice Android shows while the app stays connected in the background. " +
                        "Off hides it; alerts still arrive."
                    setShowBadge(false)
                },
            ),
        )
    }

    fun canNotify(context: Context): Boolean =
        NotificationManagerCompat.from(context).areNotificationsEnabled() &&
            (
                Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
                    ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) ==
                    PackageManager.PERMISSION_GRANTED
                )

    fun openIntent(context: Context, sessionId: Long?, screen: String? = null, connection: String = connectionId()): PendingIntent {
        val intent = Intent(context, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP
            if (sessionId != null || screen != null) identifyConnection(this, connection, NotificationIdentity.open(connection, sessionId, screen))
            sessionId?.let { putExtra(EXTRA_SESSION, it) }
            screen?.let { putExtra(EXTRA_SCREEN, it) }
        }
        val code = when {
            sessionId != null -> sessionId.toInt()
            screen != null -> -1 - screen.hashCode().and(0xffff)
            else -> 0
        }
        return PendingIntent.getActivity(
            context,
            code,
            intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
    }










    fun foreground(context: Context): Notification =
        NotificationCompat.Builder(context, CH_CONNECTION)
            .setSmallIcon(R.drawable.ic_stat_deck)
            .setContentTitle("Cepheus")
            .setContentText("Staying connected for alerts")
            .setOngoing(true)
            .setSilent(true)
            .setShowWhen(false)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setContentIntent(openIntent(context, null))
            .addAction(NotificationCompat.Action.Builder(0, "Hide", connectionSettings(context)).build())
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .build()

    /** The system page of the Connection channel, whose switch hides the notice. */
    private fun connectionSettings(context: Context): PendingIntent {
        val intent = Intent(android.provider.Settings.ACTION_CHANNEL_NOTIFICATION_SETTINGS).apply {
            putExtra(android.provider.Settings.EXTRA_APP_PACKAGE, context.packageName)
            putExtra(android.provider.Settings.EXTRA_CHANNEL_ID, CH_CONNECTION)
            flags = Intent.FLAG_ACTIVITY_NEW_TASK
        }
        return PendingIntent.getActivity(
            context,
            0,
            intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
    }

    fun finished(context: Context, id: Long, title: String, text: String, silent: Boolean) {
        val connection = connectionId()
        val n = NotificationCompat.Builder(context, CH_FINISHED)
            .setSmallIcon(R.drawable.ic_stat_deck)
            .setContentTitle(title)
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setAutoCancel(true)
            .setSilent(silent)
            .setCategory(NotificationCompat.CATEGORY_STATUS)
            .setContentIntent(openIntent(context, id, connection = connection))
            .addAction(replyAction(context, id, null, connection))
            .build()
        post(context, NotificationIdentity.tag(TAG_FINISHED, connection, id), id.toInt(), n)
    }

    fun error(context: Context, seq: Long, title: String, text: String, silent: Boolean) {
        val connection = connectionId()
        val n = NotificationCompat.Builder(context, CH_ERROR)
            .setSmallIcon(R.drawable.ic_stat_deck)
            .setContentTitle(title)
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setAutoCancel(true)
            .setSilent(silent)
            .setCategory(NotificationCompat.CATEGORY_ERROR)
            .setContentIntent(openIntent(context, null, "errors", connection))
            .build()
        post(context, NotificationIdentity.tag(TAG_ERROR, connection, seq), (seq % 100_000).toInt(), n)
    }

    /**
     * A session waits. With its prompt known, up to three of its options are
     * buttons that answer it, and a free-text option is the Reply action; a
     * prompt with several choices is answered at the desk. Without a prompt,
     * Reply sends a message.
     */
    fun needsYou(context: Context, id: Long, title: String, text: String, waiting: WaitingPrompt?, silent: Boolean) {
        val connection = connectionId()
        val b = NotificationCompat.Builder(context, CH_NEEDS_YOU)
            .setSmallIcon(R.drawable.ic_stat_deck)
            .setContentTitle(title)
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setCategory(NotificationCompat.CATEGORY_MESSAGE)
            .setOnlyAlertOnce(true)
            .setSilent(silent)
            .setAutoCancel(true)
            .setContentIntent(openIntent(context, id, connection = connection))
        val prompt = waiting?.prompt
        when {
            prompt == null -> b.addAction(replyAction(context, id, null, connection))
            // Boxes are ticked together on the chat's card, which a tap opens.
            prompt.multi -> b.setSubText("Tap to choose")
            else -> {
                val reply = prompt.freeText
                val buttons = prompt.options.indices.filter { it != reply }.take(if (reply != null) 2 else 3)
                for (choice in buttons) {
                    b.addAction(answerAction(context, waiting, choice, prompt.options[choice].label, connection))
                }
                if (reply != null) b.addAction(replyAction(context, id, waiting, connection))
            }
        }
        post(context, NotificationIdentity.tag(TAG_NEEDS_YOU, connection, id), id.toInt(), b.build())
    }

    /** Cancel the question cards of sessions no longer waiting for an answer. */
    fun reconcileAttention(context: Context, state: DeckState) {
        val connection = connectionId()
        for (id in AttentionNotifications.stale(state, needsYouShowing(context))) cancelNeedsYou(context, id, connection)
    }

    /** An answer or reply from a notification failed: say why, and open the chat on a tap. */
    fun actionFailed(context: Context, id: Long, title: String, text: String) {
        val connection = connectionId()
        val n = NotificationCompat.Builder(context, CH_NEEDS_YOU)
            .setSmallIcon(R.drawable.ic_stat_deck)
            .setContentTitle(title)
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setOnlyAlertOnce(true)
            .setAutoCancel(true)
            .setContentIntent(openIntent(context, id, connection = connection))
            .build()
        // Its own tag, so it outlives the prompt it failed to answer.
        cancelNeedsYou(context, id, connection)
        post(context, NotificationIdentity.tag(TAG_FAILED, connection, id), id.toInt(), n)
    }

    fun cancelNeedsYou(context: Context, id: Long, connection: String = connectionId()) =
        NotificationManagerCompat.from(context).cancel(NotificationIdentity.tag(TAG_NEEDS_YOU, connection, id), id.toInt())


    fun cancelFailed(context: Context, id: Long, connection: String = connectionId()) =
        NotificationManagerCompat.from(context).cancel(NotificationIdentity.tag(TAG_FAILED, connection, id), id.toInt())

    fun cancelFinished(context: Context, id: Long, connection: String = connectionId()) =
        NotificationManagerCompat.from(context).cancel(NotificationIdentity.tag(TAG_FINISHED, connection, id), id.toInt())

    fun cancelAll(context: Context) = NotificationManagerCompat.from(context).cancelAll()

    /** Leave the foreground service's quiet notice alone when changing gateways. */
    fun cancelSessionAlerts(context: Context) {
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        manager.activeNotifications.filter { it.tag != null }.forEach { manager.cancel(it.tag, it.id) }
    }

    private fun connectionId(): String = io.github.cepheus.app.Deck.connection.value?.id ?: Connections.LEGACY
    private fun identifyConnection(intent: Intent, connection: String, data: String) {
        intent.putExtra(EXTRA_CONNECTION, connection)
        intent.data = android.net.Uri.parse(data)
    }

    /** A legacy or mismatched PendingIntent cannot inherit collided extras. */
    internal fun acceptsAction(intent: Intent): Boolean {
        val connection = intent.getStringExtra(EXTRA_CONNECTION) ?: return false
        val id = intent.getLongExtra(ActionReceiver.EXTRA_ID, -1)
        if (id <= 0) return false
        val role = when (intent.action) {
            ActionReceiver.ACTION_ANSWER -> "answer"
            ActionReceiver.ACTION_REPLY -> "reply"
            else -> return false
        }
        val slot = intent.getIntExtra(ActionReceiver.EXTRA_CHOICE, 0)
        if (slot < 0) return false
        val fingerprint = intent.getStringExtra(ActionReceiver.EXTRA_FP)
        if (role == "answer" && fingerprint == null || intent.hasExtra(ActionReceiver.EXTRA_FP) && fingerprint == null) return false
        val inputs = if (fingerprint != null) intent.getLongExtra(ActionReceiver.EXTRA_INPUTS, -1).takeIf { it >= 0 } ?: return false else null
        return NotificationIdentity.matchesAction(intent.dataString, connection, id, role, slot, inputs, fingerprint)
    }
    internal fun acceptsOpen(intent: Intent, session: Long?, screen: String?): Boolean {
        val connection = intent.getStringExtra(EXTRA_CONNECTION) ?: return false
        return NotificationIdentity.matchesOpen(intent.dataString, connection, session, screen)
    }

    /** Full session IDs with a current-connection "needs you" card showing. */
    fun needsYouShowing(context: Context): Set<Long> {
        val connection = connectionId()
        return context.getSystemService(NotificationManager::class.java)?.activeNotifications.orEmpty()
            .mapNotNull { NotificationIdentity.session(TAG_NEEDS_YOU, connection, it.tag, it.id) }.toSet()
    }

    private fun answerAction(context: Context, waiting: WaitingPrompt, choice: Int, label: String, connection: String): NotificationCompat.Action {
        val intent = Intent(context, ActionReceiver::class.java).apply {
            action = ActionReceiver.ACTION_ANSWER
            identifyConnection(this, connection, NotificationIdentity.action(connection, waiting.id, "answer", choice, waiting.inputs, waiting.fp))
            putExtra(ActionReceiver.EXTRA_ID, waiting.id)
            putExtra(ActionReceiver.EXTRA_INPUTS, waiting.inputs)
            putExtra(ActionReceiver.EXTRA_FP, waiting.fp)
            putExtra(ActionReceiver.EXTRA_CHOICE, choice)
        }
        val pending = PendingIntent.getBroadcast(
            context,
            requestCode(waiting.id, choice),
            intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        return NotificationCompat.Action.Builder(0, label.take(40), pending).build()
    }

    /** Reply: the free-text answer to `waiting`, or a message when nothing waits. */
    private fun replyAction(context: Context, id: Long, waiting: WaitingPrompt?, connection: String): NotificationCompat.Action {
        val intent = Intent(context, ActionReceiver::class.java).apply {
            action = ActionReceiver.ACTION_REPLY
            identifyConnection(this, connection, NotificationIdentity.action(connection, id, "reply", waiting?.prompt?.freeText ?: 0, waiting?.inputs, waiting?.fp))
            putExtra(ActionReceiver.EXTRA_ID, id)
            if (waiting != null) {
                putExtra(ActionReceiver.EXTRA_INPUTS, waiting.inputs)
                putExtra(ActionReceiver.EXTRA_FP, waiting.fp)
                putExtra(ActionReceiver.EXTRA_CHOICE, waiting.prompt.freeText ?: 0)
            }
        }
        // The system writes the typed text into this intent, so it must be mutable.
        val flags = PendingIntent.FLAG_UPDATE_CURRENT or
            (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) PendingIntent.FLAG_MUTABLE else 0)
        val pending = PendingIntent.getBroadcast(context, requestCode(id, 7), intent, flags)
        val input = RemoteInput.Builder(REPLY_KEY).setLabel("Reply").build()
        return NotificationCompat.Action.Builder(0, "Reply", pending)
            .addRemoteInput(input)
            .setAllowGeneratedReplies(false)
            .build()
    }

    private fun requestCode(id: Long, slot: Int): Int = (id * 8 + slot).toInt()

    @SuppressLint("MissingPermission") // canNotify checks it.
    private fun post(context: Context, tag: String?, id: Int, notification: Notification) {
        if (!canNotify(context)) return
        try {
            NotificationManagerCompat.from(context).notify(tag, id, notification)
        } catch (_: SecurityException) {
        }
    }
}
