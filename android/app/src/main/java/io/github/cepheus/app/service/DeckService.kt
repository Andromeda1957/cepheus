package io.github.cepheus.app.service

import android.app.ForegroundServiceStartNotAllowedException
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.Network
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import io.github.cepheus.app.Deck
import io.github.cepheus.app.net.Link
import io.github.cepheus.app.notify.Notifier
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch

/**
 * Keeps the process, and so the connection to the deck, alive while the app
 * is closed, so alerts arrive within seconds. It runs only while Settings'
 * "Stay connected in the background" is on (off by default). A battery-exempt
 * app may run an ordinary service, without a foreground-service notice. Until
 * that permission is granted, use a foreground service to keep alerts working.
 */
class DeckService : Service() {
    private var watch: Job? = null
    private var networkCallback: ConnectivityManager.NetworkCallback? = null
    private var foreground = false

    override fun onCreate() {
        super.onCreate()
        Deck.init(this)
        Deck.start()
        watch = Deck.scope.launch {
            Deck.link.first { it is Link.Unpaired }
            stopSelf()
        }
        watchNetwork()
    }

    private fun updateNotice(forceForeground: Boolean = false) {
        if (!forceForeground && batteryExempt(this)) {
            if (foreground) {
                ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
                foreground = false
            }
            return
        }
        if (foreground) return
        val type = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE
        } else {
            0
        }
        // The notice is posted here only: posted again, it would come back
        // after the owner swiped it away (Notifier.foreground).
        ServiceCompat.startForeground(this, Notifier.FOREGROUND_ID, Notifier.foreground(this), type)
        foreground = true
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Android restarts a sticky service after it kills the process, also
        // once the switch was turned off.
        if (!Deck.paired || !Deck.settings.value.background) {
            stopSelf()
            return START_NOT_STICKY
        }
        // Returning from Android's battery question starts us again. Remove
        // the temporary notice as soon as the ordinary service is allowed.
        // Honor a startForegroundService launch even if the user granted
        // the exemption between launch and onStartCommand, then demote.
        updateNotice(intent?.getBooleanExtra(EXTRA_FOREGROUND, false) == true)
        updateNotice()
        Deck.start()
        return START_STICKY
    }

    override fun onDestroy() {
        watch?.cancel()
        networkCallback?.let { cb ->
            try {
                getSystemService(ConnectivityManager::class.java)?.unregisterNetworkCallback(cb)
            } catch (_: IllegalArgumentException) {
            }
        }
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    /** A new network (the VPN coming up, Wi-Fi to mobile) is worth an immediate try. */
    private fun watchNetwork() {
        val manager = getSystemService(ConnectivityManager::class.java) ?: return
        val callback = object : ConnectivityManager.NetworkCallback() {
            // The first call reports the network there already was.
            private var first = true

            override fun onAvailable(network: Network) {
                if (!first) Deck.kick()
                first = false
            }
        }
        try {
            manager.registerDefaultNetworkCallback(callback)
            networkCallback = callback
        } catch (_: RuntimeException) {
        }
    }

    companion object {
        private const val EXTRA_FOREGROUND = "foreground_start"
        private fun batteryExempt(context: Context): Boolean =
            context.getSystemService(PowerManager::class.java)
                ?.isIgnoringBatteryOptimizations(context.packageName) == true

        /**
         * Starts the service when the phone is paired and the background
         * connection is on; from the background only where Android allows it.
         */
        fun start(context: Context) {
            Deck.init(context)
            if (!Deck.paired || !Deck.settings.value.background) return
            try {
                val intent = Intent(context, DeckService::class.java)
                if (batteryExempt(context)) context.startService(intent)
                else ContextCompat.startForegroundService(context, intent.putExtra(EXTRA_FOREGROUND, true))
            } catch (e: IllegalStateException) {
                // Android 12+ refuses a start from the background outside its exemptions;
                // the app starts it the next time it opens.
                if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S || e !is ForegroundServiceStartNotAllowedException) {
                    throw e
                }
            }
        }
    }
}
