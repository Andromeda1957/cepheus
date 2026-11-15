package io.github.cepheus.app.ui

/** User intent to follow new rows, independent of lazy-list key anchoring. */
internal class ConversationFollow(private var following: Boolean = true) {
    val isFollowing: Boolean get() = following
    fun pause() { following = false }

    fun resume() { following = true }
    fun observe(scrolling: Boolean, atLatest: Boolean) {
        if (scrolling) following = atLatest
    }
    fun shouldFollow(scrolling: Boolean) = following && !scrolling
}
