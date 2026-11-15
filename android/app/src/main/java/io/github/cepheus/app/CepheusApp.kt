package io.github.cepheus.app

import android.app.Application

class CepheusApp : Application() {
    override fun onCreate() {
        super.onCreate()
        Deck.init(this)
    }
}
