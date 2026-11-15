package io.github.cepheus.app.ui

import android.content.Intent
import android.graphics.Color
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.cepheus.app.Deck
import io.github.cepheus.app.notify.Notifier
import io.github.cepheus.app.service.DeckService

/** Where a notification asked the app to open. */
data class Launch(val sessionId: Long?, val screen: String?, val serial: Long = System.nanoTime())

class MainActivity : ComponentActivity() {
    private val launch = mutableStateOf<Launch?>(null)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        Deck.init(this)
        if (savedInstanceState == null) launch.value = launchOf(intent)
        setContent {
            val settings by Deck.settings.collectAsStateWithLifecycle()
            val dark = settings.theme.isDark(isSystemInDarkTheme())
            // The status and navigation bar icons follow the app's theme, not only the phone's.
            DisposableEffect(dark) {
                enableEdgeToEdge(
                    statusBarStyle = SystemBarStyle.auto(Color.TRANSPARENT, Color.TRANSPARENT) { dark },
                    navigationBarStyle = SystemBarStyle.auto(LIGHT_SCRIM, DARK_SCRIM) { dark },
                )
                onDispose {}
            }
            CepheusTheme(dark) {
                AppRoot(launch = launch.value, onLaunchHandled = { launch.value = null })
            }
        }
    }

    override fun onStart() {
        super.onStart()
        Deck.appShown()
    }

    override fun onResume() {
        super.onResume()
        // The battery-permission dialog can return without another onStart.
        DeckService.start(this)
    }

    override fun onStop() {
        Deck.appHidden()
        super.onStop()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        launch.value = launchOf(intent)
    }

    private fun launchOf(intent: Intent?): Launch? {
        if (intent == null) return null
        val session = if (intent.hasExtra(Notifier.EXTRA_SESSION)) intent.getLongExtra(Notifier.EXTRA_SESSION, -1) else null
        val screen = intent.getStringExtra(Notifier.EXTRA_SCREEN)
        if ((intent.hasExtra(Notifier.EXTRA_CONNECTION) || session != null || screen != null) && !Notifier.acceptsOpen(intent, session, screen)) return null
        intent.getStringExtra(Notifier.EXTRA_CONNECTION)?.let { id ->
            if (Deck.connection.value?.id != id && Deck.selectConnection(id) != null) return null
        }
        return if (session == null && screen == null) null else Launch(session?.takeIf { it >= 0 }, screen)
    }

    private companion object {
        // enableEdgeToEdge's own scrims for three-button navigation.
        val LIGHT_SCRIM = Color.argb(0xe6, 0xFF, 0xFF, 0xFF)
        val DARK_SCRIM = Color.argb(0x80, 0x1b, 0x1b, 0x1b)
    }
}
