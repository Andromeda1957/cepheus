package io.github.cepheus.app.ui

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ConversationFollowTest {
    @Test fun aLargeBatchDoesNotMistakeKeyAnchoringForUserScrolling() {
        val follow = ConversationFollow()
        follow.observe(scrolling = false, atLatest = true)
        // Inserting many reverse-layout rows moves the old visible key away from index 0.
        follow.observe(scrolling = false, atLatest = false)
        assertTrue(follow.shouldFollow(scrolling = false))
    }

    @Test fun readingOlderMessagesSurvivesFurtherIncomingBatches() {
        val follow = ConversationFollow()
        follow.observe(scrolling = true, atLatest = false)
        follow.observe(scrolling = false, atLatest = false)
        assertFalse(follow.shouldFollow(scrolling = false))
        follow.observe(scrolling = false, atLatest = false)
        assertFalse(follow.shouldFollow(scrolling = false))
    }

    @Test fun ReturningToLatestRestoresFollowingWithoutInterruptingAGesture() {
        val follow = ConversationFollow()
        follow.observe(scrolling = true, atLatest = false)
        follow.observe(scrolling = true, atLatest = true)
        assertFalse(follow.shouldFollow(scrolling = true))
        follow.observe(scrolling = false, atLatest = true)
        assertTrue(follow.shouldFollow(scrolling = false))
    }
}
