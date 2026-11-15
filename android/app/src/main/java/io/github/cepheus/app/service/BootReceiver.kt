package io.github.cepheus.app.service

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/** Reconnects after the phone restarts or the app is updated, when the background connection is on. */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        when (intent.action) {
            Intent.ACTION_BOOT_COMPLETED, Intent.ACTION_MY_PACKAGE_REPLACED -> DeckService.start(context)
        }
    }
}
